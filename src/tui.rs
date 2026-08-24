//! ratatui dashboard: live table of check reports with detail pane, update and
//! approve modals, all engine I/O off the draw loop (worker thread + channels).
//!
//! `updatium tui --print-frame` renders ONE frame from current DB state as
//! plain text and exits 0 — CI-verifiable without a terminal.

use crate::engine::{CheckReport, Engine, Status};
use crate::version::cmp_version;
use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use std::path::Path;
use std::sync::mpsc;

const TICK_MS: u64 = 250;

// ---------------------------------------------------------------------------
// --print-frame (DB-only, no network)
// ---------------------------------------------------------------------------

/// One rendered frame from the current DB state.
pub fn run(home: Option<&Path>, print_frame: bool) -> Result<i32> {
    if print_frame {
        let e = Engine::new(home)?;
        let text = render_db_frame(&e);
        println!("{text}");
        return Ok(0);
    }
    interactive(home)?;
    Ok(0)
}

fn frame_rows(e: &Engine) -> Vec<crate::cli::TableRow> {
    let mut rows = Vec::new();
    for (id, provider, installed) in e.store_list().unwrap_or_default() {
        let latest = e.latest_known_pub(&id).ok().flatten();
        let status = match e.is_pending_approval(&id) {
            Ok(true) => Status::NeedsApproval,
            _ => match (&installed, &latest) {
                (_, None) => Status::NeverChecked,
                (None, Some(_)) => Status::NeverChecked,
                (Some(i), Some(l)) => {
                    if cmp_version(l, i) == std::cmp::Ordering::Greater {
                        Status::UpdateAvailable
                    } else {
                        Status::Ok
                    }
                }
            },
        };
        rows.push((id, provider, installed, latest, status));
    }
    rows
}

fn render_db_frame(e: &Engine) -> String {
    // Render through a ratatui Buffer so CI output equals interactive output.
    let rows = frame_rows(e);
    let width = 100usize;
    let height = (rows.len() + 4).max(6) as u16;
    let backend = ratatui::backend::TestBackend::new(width as u16, height.max(3));
    let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
    let pending = rows
        .iter()
        .filter(|r| matches!(r.4, Status::NeedsApproval))
        .count();
    let updates = rows
        .iter()
        .filter(|r| matches!(r.4, Status::UpdateAvailable))
        .count();
    let errors = rows
        .iter()
        .filter(|r| matches!(r.4, Status::Error(_)))
        .count();

    terminal
        .draw(|f: &mut Frame| draw_dashboard(f, &rows, 0, false, None, (pending, updates, errors)))
        .expect("draw");
    let buf = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        while out.ends_with(' ') {
            out.pop();
        }
        out.push('\n');
    }
    out.trim_end_matches('\n').to_string()
}

// ---------------------------------------------------------------------------
// Interactive mode
// ---------------------------------------------------------------------------

enum UiMsg {
    Reports(Vec<CheckReport>),
    Events(String, Vec<(String, String)>),
    Done(String),
}

enum Cmd {
    Refresh,
    Update(String),
    Approve(String),
    Events(String),
}

#[allow(clippy::too_many_lines)]
fn interactive(home: Option<&Path>) -> Result<()> {
    let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
    let (msg_tx, msg_rx) = mpsc::channel::<UiMsg>();

    // Worker thread: owns Engine + tokio runtime; never touches the screen.
    let worker_home = home.map(Path::to_path_buf);
    let worker = std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().ok()?;
        let e = Engine::new(worker_home.as_deref()).ok()?;
        let e = &e;
        loop {
            match cmd_rx.recv() {
                Ok(Cmd::Refresh) => {
                    let msg_tx = msg_tx.clone();
                    rt.block_on(async move {
                        match e.check(None).await {
                            Ok(reports) => {
                                let _ = msg_tx.send(UiMsg::Reports(reports));
                            }
                            Err(err) => {
                                let _ = msg_tx.send(UiMsg::Done(format!("check failed: {err:#}")));
                            }
                        }
                    });
                }
                Ok(Cmd::Update(id)) => {
                    let msg_tx = msg_tx.clone();
                    rt.block_on(async move {
                        match e.update(&id, &crate::engine::UpdateOpts::default()).await {
                            Ok(rep) => match rep.failed {
                                Some(f) => {
                                    let _ = msg_tx
                                        .send(UiMsg::Done(format!("{} failed: {f}", rep.app)));
                                }
                                None => {
                                    let _ =
                                        msg_tx.send(UiMsg::Done(format!("{} updated", rep.app)));
                                    if let Ok(reports) = e.check(Some(&[id])).await {
                                        let _ = msg_tx.send(UiMsg::Reports(reports));
                                    }
                                }
                            },
                            Err(err) => {
                                let _ = msg_tx.send(UiMsg::Done(format!("update error: {err:#}")));
                            }
                        }
                    });
                }
                Ok(Cmd::Approve(id)) => match e.approve(&id) {
                    Ok((_old, new)) => {
                        let short = &new[..new.len().min(12)];
                        let _ = msg_tx.send(UiMsg::Done(format!(
                            "approved {id} at {short}…; re-run update to build"
                        )));
                    }
                    Err(err) => {
                        let _ = msg_tx.send(UiMsg::Done(format!("approve failed: {err:#}")));
                    }
                },
                Ok(Cmd::Events(id)) => {
                    let events = e
                        .history(Some(&id), 20)
                        .map(|list| {
                            list.into_iter()
                                .map(|ev| (ev.kind, ev.detail.unwrap_or_default()))
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    let _ = msg_tx.send(UiMsg::Events(id, events));
                }
                Err(_) => break,
            }
            // Keep the runtime alive across block_on calls.
            #[allow(unreachable_code)]
            {}
        }
        Some(())
    });

    let mut stdout = std::io::stdout();
    crossterm::terminal::enable_raw_mode()?;
    crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableMouseCapture
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend)?;

    let res: Result<()> = drive_ui(&mut terminal, cmd_tx.clone(), msg_rx);

    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::event::DisableMouseCapture,
        crossterm::terminal::LeaveAlternateScreen
    )?;
    crossterm::terminal::disable_raw_mode()?;

    drop(cmd_tx); // worker exits on channel close
    let _ = worker.join();
    res
}

struct UiState {
    reports: Vec<CheckReport>,
    selected: usize,
    show_detail: bool,
    detail: Option<(String, Vec<(String, String)>)>,
    status_line: String,
    confirm_update: bool,
    confirm_approve: bool,
}

#[allow(clippy::too_many_lines)]
fn drive_ui(
    terminal: &mut ratatui::Terminal<CrosstermBackend<std::io::Stdout>>,
    cmd_tx: mpsc::Sender<Cmd>,
    msg_rx: mpsc::Receiver<UiMsg>,
) -> Result<()> {
    let mut ui = UiState {
        reports: Vec::new(),
        selected: 0,
        show_detail: false,
        detail: None,
        status_line: "loading… press r to refresh, q to quit".into(),
        confirm_update: false,
        confirm_approve: false,
    };
    let _ = cmd_tx.send(Cmd::Refresh);

    loop {
        let counts = (
            ui.reports
                .iter()
                .filter(|r| matches!(r.status, Status::UpdateAvailable))
                .count(),
            ui.reports
                .iter()
                .filter(|r| matches!(r.status, Status::NeedsApproval))
                .count(),
            ui.reports
                .iter()
                .filter(|r| matches!(r.status, Status::Error(_)))
                .count(),
        );
        let modal = if ui.confirm_update {
            Some("confirm-update")
        } else if ui.confirm_approve {
            Some("approve-summary")
        } else {
            None
        };
        terminal.draw(|f| {
            draw_dashboard(
                f,
                &ui.reports.iter().map(cr_to_tuple).collect::<Vec<_>>(),
                ui.selected,
                ui.show_detail,
                ui.detail.as_ref(),
                counts,
            );
            if let Some(m) = modal {
                draw_modal(f, m, &ui);
            }
        })?;

        // Drain UI messages without blocking the event poll.
        loop {
            match msg_rx.try_recv() {
                Ok(UiMsg::Reports(reports)) => {
                    ui.reports = reports;
                    ui.selected = ui.selected.min(ui.reports.len().saturating_sub(1));
                    ui.status_line = format!("checked {} apps", ui.reports.len());
                }
                Ok(UiMsg::Events(id, events)) => {
                    ui.detail = Some((id, events));
                }
                Ok(UiMsg::Done(text)) => {
                    ui.status_line = text;
                    ui.confirm_update = false;
                    ui.confirm_approve = false;
                }
                Err(_) => break,
            }
        }

        if !crossterm::event::poll(std::time::Duration::from_millis(TICK_MS))
            .map_err(anyhow::Error::msg)?
        {
            continue;
        }
        match crossterm::event::read().map_err(anyhow::Error::msg)? {
            Event::Key(k) if k.kind == KeyEventKind::Press => {
                match (k.code, k.modifiers.contains(KeyModifiers::CONTROL)) {
                    (KeyCode::Char('q'), _) | (KeyCode::Char('c'), true)
                        if !ui.confirm_update && !ui.confirm_approve =>
                    {
                        return Ok(());
                    }
                    (KeyCode::Char('r'), _) => {
                        ui.status_line = "refreshing…".into();
                        let _ = cmd_tx.send(Cmd::Refresh);
                    }
                    (KeyCode::Char('d'), _) if !ui.confirm_update && !ui.confirm_approve => {
                        ui.show_detail = !ui.show_detail;
                        if ui.show_detail {
                            if let Some(r) = ui.reports.get(ui.selected) {
                                let _ = cmd_tx.send(Cmd::Events(r.app.clone()));
                            }
                        }
                    }
                    (KeyCode::Char('j'), _) | (KeyCode::Down, _)
                        if !ui.confirm_update && !ui.confirm_approve =>
                    {
                        ui.selected = (ui.selected + 1).min(ui.reports.len().saturating_sub(1));
                    }
                    (KeyCode::Char('k'), _) | (KeyCode::Up, _)
                        if !ui.confirm_update && !ui.confirm_approve =>
                    {
                        ui.selected = ui.selected.saturating_sub(1);
                    }
                    (KeyCode::PageDown, _) => {
                        ui.selected = (ui.selected + 10).min(ui.reports.len().saturating_sub(1));
                    }
                    (KeyCode::PageUp, _) => {
                        ui.selected = ui.selected.saturating_sub(10);
                    }
                    (KeyCode::Char('u'), _) if !ui.confirm_update && !ui.confirm_approve => {
                        ui.confirm_update = true;
                    }
                    (KeyCode::Char('a'), _) if !ui.confirm_update && !ui.confirm_approve => {
                        ui.confirm_approve = true;
                    }
                    (KeyCode::Char('y'), _) if ui.confirm_update => {
                        ui.confirm_update = false;
                        if let Some(r) = ui.reports.get(ui.selected) {
                            let id = r.app.clone();
                            ui.status_line = format!("updating {id}…");
                            let _ = cmd_tx.send(Cmd::Update(id));
                        }
                    }
                    (KeyCode::Char('y'), _) if ui.confirm_approve => {
                        ui.confirm_approve = false;
                        if let Some(r) = ui.reports.get(ui.selected) {
                            let id = r.app.clone();
                            let _ = cmd_tx.send(Cmd::Approve(id));
                        }
                    }
                    (KeyCode::Esc, _) | (KeyCode::Char('n'), _) => {
                        ui.confirm_update = false;
                        ui.confirm_approve = false;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

fn cr_to_tuple(r: &CheckReport) -> (String, String, Option<String>, Option<String>, Status) {
    (
        r.app.clone(),
        r.provider.clone(),
        r.installed.clone(),
        r.latest.clone(),
        r.status.clone(),
    )
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

type Row5 = crate::cli::TableRow;

fn draw_dashboard(
    f: &mut Frame,
    rows: &[Row5],
    selected: usize,
    show_detail: bool,
    detail: Option<&(String, Vec<(String, String)>)>,
    counts: (usize, usize, usize),
) {
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .split(f.area());

    // Top bar.
    let top = Line::from(vec![
        Span::styled(" updatium ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(format!(
            "▲ updates: {}   ⚠ approvals: {}   ✖ errors: {}",
            counts.0, counts.1, counts.2
        )),
    ]);
    f.render_widget(Paragraph::new(top), chunks[0]);

    let main_area: Rect = if show_detail {
        let cols = Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(chunks[1]);
        render_detail(f, cols[1], detail);
        cols[0]
    } else {
        chunks[1]
    };

    let header = Row::new(["ID", "PROVIDER", "INSTALLED", "LATEST", "STATUS"])
        .style(Style::default().add_modifier(Modifier::BOLD));
    let body: Vec<Row> = rows
        .iter()
        .enumerate()
        .map(|(i, (id, p, inst, latest, st))| {
            let marker = match st {
                Status::Ok => "●",
                Status::UpdateAvailable => "▲",
                Status::NeedsApproval => "⚠",
                Status::Error(_) => "✖",
                Status::NeverChecked => "·",
                Status::Disabled => "○",
            };
            let label = match st {
                Status::Error(e) => format!("{marker} error ({e})"),
                other => format!(
                    "{marker} {}",
                    other.label().split_once(' ').unwrap_or(("", "")).1
                ),
            };
            let style = if i == selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            Row::new([
                Cell::from(id.clone()),
                Cell::from(p.clone()),
                Cell::from(inst.clone().unwrap_or_else(|| "—".into())),
                Cell::from(latest.clone().unwrap_or_else(|| "—".into())),
                Cell::from(label),
            ])
            .style(style)
        })
        .collect();

    let table = Table::new(
        body,
        [
            Constraint::Length(16),
            Constraint::Length(18),
            Constraint::Length(14),
            Constraint::Length(14),
            Constraint::Fill(1),
        ],
    )
    .header(header)
    .block(Block::default().borders(Borders::ALL).title("apps"));
    f.render_widget(table, main_area);

    // Bottom hints.
    let hints = if show_detail {
        "[j/k] move [r] refresh [u] update [a] approve [d] close detail [q] quit"
    } else {
        "[j/k] move [r] refresh [u] update [U] update all [a] approve [d] detail [q] quit"
    };
    f.render_widget(Paragraph::new(hints), chunks[2]);
}

fn render_detail(f: &mut Frame, area: Rect, detail: Option<&(String, Vec<(String, String)>)>) {
    let lines: Vec<Line> = match detail {
        None => vec![Line::from("press Enter… (no events loaded)")],
        Some((id, events)) if events.is_empty() => vec![Line::from(format!("{id}: no events"))],
        Some((id, events)) => {
            let mut v = vec![Line::styled(
                format!("{id} — last {} events", events.len()),
                Style::default().add_modifier(Modifier::BOLD),
            )];
            for (kind, det) in events {
                v.push(Line::from(format!("{kind:<16} {det}")));
            }
            v
        }
    };
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("detail")),
        area,
    );
}

fn draw_modal(f: &mut Frame, kind: &str, ui: &UiState) {
    let sel = ui.reports.get(ui.selected);
    let text: Vec<Line> = match (kind, sel) {
        ("confirm-update", Some(r)) => vec![
            Line::from("Confirm update"),
            Line::from(format!("app:      {}", r.app)),
            Line::from(format!(
                "installed: {}",
                r.installed.as_deref().unwrap_or("—")
            )),
            Line::from(format!("latest:    {}", r.latest.as_deref().unwrap_or("—"))),
            Line::from("[y] proceed   [n/Esc] cancel"),
        ],
        ("approve-summary", Some(r)) => vec![
            Line::from("Approve changed build scripts"),
            Line::from(format!("app: {}", r.app)),
            Line::from("The recorded hash will be recomputed from the current checkout."),
            Line::from("[y] approve   [n/Esc] cancel"),
        ],
        _ => vec![Line::from("?")],
    };
    let area = centered_rect(50, 8, f.area());
    f.render_widget(tui_clear(), area);
    f.render_widget(
        Paragraph::new(text).block(Block::default().borders(Borders::ALL).title("updatium")),
        area,
    );
}

fn tui_clear() -> impl ratatui::widgets::Widget {
    ratatui::widgets::Clear
}

fn centered_rect(percent_x: u16, height: u16, r: Rect) -> Rect {
    let w = r.width * percent_x / 100;
    let h = height.min(r.height);
    Rect {
        x: r.x + (r.width.saturating_sub(w)) / 2,
        y: r.y + (r.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}
