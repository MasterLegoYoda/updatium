//! ratatui operations dashboard and app manager. Engine I/O is serialized on a
//! worker thread; `--print-frame` remains a network-free DB/config snapshot.

use crate::config::{self, HomePaths};
use crate::engine::{Engine, Status};
use anyhow::{Context as _, Result, bail};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, Wrap};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

const TICK_MS: u64 = 250;
const ACTIVITY_HISTORY: usize = 4;

// ---------------------------------------------------------------------------
// --print-frame (DB/config-only, no network)
// ---------------------------------------------------------------------------

/// One rendered frame from the current DB state.
pub fn run(home: Option<&Path>, print_frame: bool) -> Result<i32> {
    if print_frame {
        let engine = Engine::new(home)?;
        println!("{}", render_db_frame(&engine));
        return Ok(0);
    }
    interactive(home)?;
    Ok(0)
}

fn frame_rows(engine: &Engine) -> Vec<UiRow> {
    let apps = config::load_apps(&engine.home_paths().apps_d()).unwrap_or_default();
    apps.into_iter()
        .map(|app| {
            let installed = engine.installed_version(&app.id).ok().flatten();
            let latest = engine.latest_known_pub(&app.id).ok().flatten();
            let status = if !app.enabled {
                Status::Disabled
            } else if engine.is_pending_approval(&app.id).unwrap_or(false) {
                Status::NeedsApproval
            } else {
                status_from_versions(installed.as_deref(), latest.as_deref())
            };
            let (config_path, config_issue) = match app_config_path_in(engine, &app.id) {
                Ok(path) => (Some(path), None),
                Err(err) => (None, Some(format!("{err:#}"))),
            };
            UiRow {
                app: app.id,
                provider: app.provider,
                enabled: app.enabled,
                installed,
                latest,
                status,
                config_path,
                config_issue,
            }
        })
        .collect()
}

fn render_db_frame(engine: &Engine) -> String {
    // Render through a ratatui Buffer so CI output equals an interactive frame.
    let rows = frame_rows(engine);
    let visible: Vec<usize> = (0..rows.len()).collect();
    let backend = ratatui::backend::TestBackend::new(100, (rows.len() + 6).max(8) as u16);
    let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
    let mut ui = UiState::default();
    ui.rows = rows;
    terminal
        .draw(|f| draw_dashboard(f, &ui, &visible))
        .expect("draw");
    let buffer = terminal.backend().buffer();
    let mut output = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            output.push_str(buffer[(x, y)].symbol());
        }
        while output.ends_with(' ') {
            output.pop();
        }
        output.push('\n');
    }
    output.trim_end_matches('\n').to_string()
}

fn status_from_versions(installed: Option<&str>, latest: Option<&str>) -> Status {
    match (installed, latest) {
        (Some(installed), Some(latest))
            if crate::version::cmp_version(latest, installed) == std::cmp::Ordering::Greater =>
        {
            Status::UpdateAvailable
        }
        (Some(_), Some(_)) => Status::Ok,
        _ => Status::NeverChecked,
    }
}

// ---------------------------------------------------------------------------
// Interactive mode and worker protocol
// ---------------------------------------------------------------------------

enum UiMsg {
    Rows(Vec<UiRow>),
    Events(String, Vec<crate::store::EventRow>),
    Plugins(Vec<PluginRow>),
    Activity(String),
    Scaffold(std::result::Result<PathBuf, String>),
    Fatal(String),
    Finished,
}

#[derive(Clone)]
struct UiRow {
    app: String,
    provider: String,
    enabled: bool,
    installed: Option<String>,
    latest: Option<String>,
    status: Status,
    config_path: Option<PathBuf>,
    config_issue: Option<String>,
}

#[derive(Clone)]
struct PluginRow {
    kind: String,
    key: String,
    path: String,
}

enum Cmd {
    Refresh,
    Update(String),
    UpdateMany(Vec<String>),
    Approve(String),
    Events(String),
    Plugins,
    Scaffold {
        id: String,
        provider: String,
        format: crate::cli::ScaffoldFormat,
    },
    Desktop(String, bool),
}

fn interactive(home: Option<&Path>) -> Result<()> {
    let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
    let (msg_tx, msg_rx) = mpsc::channel::<UiMsg>();
    let worker_home = home.map(Path::to_path_buf);
    let worker = std::thread::spawn(move || worker_loop(worker_home, cmd_rx, msg_tx));

    if let Err(error) = crossterm::terminal::enable_raw_mode() {
        drop(cmd_tx);
        let _ = worker.join();
        return Err(error).context("enabling terminal raw mode");
    }
    let mut stdout = std::io::stdout();
    if let Err(error) = crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableMouseCapture
    ) {
        let _ = crossterm::execute!(
            stdout,
            crossterm::event::DisableMouseCapture,
            crossterm::terminal::LeaveAlternateScreen
        );
        let _ = crossterm::terminal::disable_raw_mode();
        drop(cmd_tx);
        let _ = worker.join();
        return Err(error).context("entering TUI screen");
    }
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = match ratatui::Terminal::new(backend) {
        Ok(terminal) => terminal,
        Err(error) => {
            let mut stdout = std::io::stdout();
            let _ = crossterm::execute!(
                stdout,
                crossterm::event::DisableMouseCapture,
                crossterm::terminal::LeaveAlternateScreen
            );
            let _ = crossterm::terminal::disable_raw_mode();
            drop(cmd_tx);
            let _ = worker.join();
            return Err(error).context("initializing TUI terminal");
        }
    };

    let run_result = drive_ui(&mut terminal, home, cmd_tx.clone(), msg_rx);
    let restore_result = restore_terminal(&mut terminal);
    drop(cmd_tx);
    let _ = worker.join();
    match (run_result, restore_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(restore)) => Err(error.context(format!(
            "terminal restoration also failed: {restore:#}"
        ))),
    }
}

fn restore_terminal(
    terminal: &mut ratatui::Terminal<CrosstermBackend<std::io::Stdout>>,
) -> Result<()> {
    let screen = crossterm::execute!(
        terminal.backend_mut(),
        crossterm::event::DisableMouseCapture,
        crossterm::terminal::LeaveAlternateScreen
    );
    let raw = crossterm::terminal::disable_raw_mode();
    screen.context("leaving TUI screen")?;
    raw.context("disabling terminal raw mode")?;
    Ok(())
}

fn worker_loop(home: Option<PathBuf>, cmd_rx: mpsc::Receiver<Cmd>, msg_tx: mpsc::Sender<UiMsg>) {
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = msg_tx.send(UiMsg::Fatal(format!("starting worker runtime: {error}")));
            return;
        }
    };
    let engine = match Engine::new(home.as_deref()) {
        Ok(engine) => engine,
        Err(error) => {
            let _ = msg_tx.send(UiMsg::Fatal(format!("opening engine: {error:#}")));
            return;
        }
    };
    while let Ok(command) = cmd_rx.recv() {
        match command {
            Cmd::Refresh => worker_refresh(&engine, &runtime, &msg_tx),
            Cmd::Update(id) => {
                let result = runtime.block_on(engine.update(
                    &id,
                    &crate::engine::UpdateOpts::default(),
                ));
                let _ = msg_tx.send(UiMsg::Activity(update_result(&id, result)));
                worker_refresh(&engine, &runtime, &msg_tx);
            }
            Cmd::UpdateMany(mut ids) => {
                ids.sort();
                for id in ids {
                    let result = runtime.block_on(engine.update(
                        &id,
                        &crate::engine::UpdateOpts::default(),
                    ));
                    let _ = msg_tx.send(UiMsg::Activity(update_result(&id, result)));
                }
                worker_refresh(&engine, &runtime, &msg_tx);
            }
            Cmd::Approve(id) => {
                let text = match engine.approve(&id) {
                    Ok((_old, new)) => format!(
                        "approved {id} at {}…; re-run update to build",
                        &new[..new.len().min(12)]
                    ),
                    Err(error) => format!("approve {id} failed: {error:#}"),
                };
                let _ = msg_tx.send(UiMsg::Activity(text));
                worker_refresh(&engine, &runtime, &msg_tx);
            }
            Cmd::Events(id) => {
                match engine.history(Some(&id), 20) {
                    Ok(events) => {
                        let _ = msg_tx.send(UiMsg::Events(id, events));
                    }
                    Err(error) => {
                        let _ = msg_tx.send(UiMsg::Activity(format!(
                            "history for {id} failed: {error:#}"
                        )));
                    }
                }
                let _ = msg_tx.send(UiMsg::Finished);
            }
            Cmd::Plugins => {
                let result = crate::config::GlobalConfig::load(&engine.home_paths().config_file());
                match result {
                    Ok(mut global) => {
                        global.home = engine.home_paths().clone();
                        let mut items: Vec<_> = crate::provider::describe_registry(&global)
                            .into_iter()
                            .map(|plugin| PluginRow {
                                kind: format!("provider/{}", plugin.kind),
                                key: plugin.key,
                                path: plugin.path.map_or_else(
                                    || "builtin".into(),
                                    |path| path.display().to_string(),
                                ),
                            })
                            .collect();
                        items.extend(
                            crate::installer::describe_registry(&global)
                                .into_iter()
                                .map(|plugin| PluginRow {
                                    kind: format!("installer/{}", plugin.kind),
                                    key: plugin.key,
                                    path: plugin.path.map_or_else(
                                        || "builtin".into(),
                                        |path| path.display().to_string(),
                                    ),
                                }),
                        );
                        let _ = msg_tx.send(UiMsg::Plugins(items));
                    }
                    Err(error) => {
                        let _ = msg_tx.send(UiMsg::Activity(format!(
                            "plugin catalog failed: {error:#}"
                        )));
                    }
                }
                let _ = msg_tx.send(UiMsg::Finished);
            }
            Cmd::Desktop(id, remove) => {
                let result = crate::cli::manage_desktop(&engine, &id, remove);
                let text = result.map_or_else(
                    |error| format!("desktop action for {id} failed: {error:#}"),
                    |_| {
                        if remove {
                            format!("removed desktop entry for {id}")
                        } else {
                            format!("wrote desktop entry for {id}")
                        }
                    },
                );
                let _ = msg_tx.send(UiMsg::Activity(text));
                let _ = msg_tx.send(UiMsg::Finished);
            }
            Cmd::Scaffold { id, provider, format } => {
                let result = crate::cli::create_app_scaffold(home.as_deref(), &id, &provider, format)
                    .map_err(|error| format!("{error:#}"));
                let _ = msg_tx.send(UiMsg::Scaffold(result));
                let _ = msg_tx.send(UiMsg::Finished);
            }
        }
    }
}

fn update_result(id: &str, result: Result<crate::engine::UpdateReport>) -> String {
    match result {
        Ok(report) => report.failed.map_or_else(
            || format!("{} updated", report.app),
            |error| format!("{} update failed: {error}", report.app),
        ),
        Err(error) => format!("update {id} failed: {error:#}"),
    }
}

fn worker_refresh(
    engine: &Engine,
    runtime: &tokio::runtime::Runtime,
    msg_tx: &mpsc::Sender<UiMsg>,
) {
    let check_result = runtime.block_on(async {
        let apps = config::load_apps(&engine.home_paths().apps_d())?;
        let checked = engine.check(None).await;
        Ok::<_, anyhow::Error>((apps, checked))
    });
    match check_result {
        Err(error) => {
            let _ = msg_tx.send(UiMsg::Activity(format!("refresh failed: {error:#}")));
        }
        Ok((apps, checked)) => {
            let check_error = checked.as_ref().err().map(|error| format!("{error:#}"));
            let reports: std::collections::HashMap<_, _> = checked
                .unwrap_or_default()
                .into_iter()
                .map(|report| (report.app.clone(), report))
                .collect();
            let mut rows = Vec::with_capacity(apps.len());
            let mut row_error = None;
            for app in apps {
                let installed = match engine.installed_version(&app.id) {
                    Ok(value) => value,
                    Err(error) => {
                        row_error = Some(error);
                        None
                    }
                };
                let latest = match engine.latest_known_pub(&app.id) {
                    Ok(value) => value,
                    Err(error) => {
                        row_error = Some(error);
                        None
                    }
                };
                let status = if !app.enabled {
                    Status::Disabled
                } else if let Some(report) = reports.get(&app.id) {
                    match &report.status {
                        Status::Error(_) => report.status.clone(),
                        _ if engine.is_pending_approval(&app.id).unwrap_or(false) => {
                            Status::NeedsApproval
                        }
                        _ => report.status.clone(),
                    }
                } else if let Some(error) = check_error.as_ref() {
                    Status::Error(format!("check failed: {error}"))
                } else if engine.is_pending_approval(&app.id).unwrap_or(false) {
                    Status::NeedsApproval
                } else {
                    status_from_versions(installed.as_deref(), latest.as_deref())
                };
                let (config_path, config_issue) = match app_config_path_in(engine, &app.id) {
                    Ok(path) => (Some(path), None),
                    Err(error) => (None, Some(format!("{error:#}"))),
                };
                rows.push(UiRow {
                    app: app.id,
                    provider: app.provider,
                    enabled: app.enabled,
                    installed,
                    latest,
                    status,
                    config_path,
                    config_issue,
                });
            }
            if let Some(error) = row_error {
                let _ = msg_tx.send(UiMsg::Activity(format!("refresh state error: {error:#}")));
            }
            if let Some(error) = check_error {
                let _ = msg_tx.send(UiMsg::Activity(format!("check refresh failed: {error}")));
            } else {
                let _ = msg_tx.send(UiMsg::Activity(format!("refreshed {} configured app(s)", rows.len())));
            }
            let _ = msg_tx.send(UiMsg::Rows(rows));
        }
    }
    let _ = msg_tx.send(UiMsg::Finished);
}

// ---------------------------------------------------------------------------
// Dashboard state, filtering and input
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    Updates,
    Approvals,
    Errors,
    Disabled,
}

impl Filter {
    fn next(self) -> Self {
        match self {
            Self::All => Self::Updates,
            Self::Updates => Self::Approvals,
            Self::Approvals => Self::Errors,
            Self::Errors => Self::Disabled,
            Self::Disabled => Self::All,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Updates => "Updates",
            Self::Approvals => "Approvals",
            Self::Errors => "Errors",
            Self::Disabled => "Disabled",
        }
    }

    fn matches(self, status: &Status) -> bool {
        match self {
            Self::All => true,
            Self::Updates => matches!(status, Status::UpdateAvailable),
            Self::Approvals => matches!(status, Status::NeedsApproval),
            Self::Errors => matches!(status, Status::Error(_)),
            Self::Disabled => matches!(status, Status::Disabled),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Dashboard,
    Help,
    Plugins,
    NewApp,
}

enum Confirmation {
    Update(String),
    BulkUpdate(Vec<String>),
    Approve(String),
    DeleteConfig { id: String, path: PathBuf },
    RemoveDesktop(String),
}

struct NewAppForm {
    id: String,
    provider: String,
    format: crate::cli::ScaffoldFormat,
    field: usize,
}

struct UiState {
    rows: Vec<UiRow>,
    selected: usize,
    show_detail: bool,
    detail: Option<(String, Vec<crate::store::EventRow>)>,
    status_line: String,
    activity: VecDeque<String>,
    busy: usize,
    filter: Filter,
    search: String,
    search_input: Option<String>,
    screen: Screen,
    confirmation: Option<Confirmation>,
    plugins: Vec<PluginRow>,
    plugins_loaded: bool,
    form: Option<NewAppForm>,
    worker_failed: bool,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            selected: 0,
            show_detail: false,
            detail: None,
            status_line: "starting…".into(),
            activity: VecDeque::new(),
            busy: 0,
            filter: Filter::All,
            search: String::new(),
            search_input: None,
            screen: Screen::Dashboard,
            confirmation: None,
            plugins: Vec::new(),
            plugins_loaded: false,
            form: None,
            worker_failed: false,
        }
    }
}

impl UiState {
    fn visible(&self) -> Vec<usize> {
        let needle = self.search.to_lowercase();
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                self.filter.matches(&row.status)
                    && (needle.is_empty()
                        || row.app.to_lowercase().contains(&needle)
                        || row.provider.to_lowercase().contains(&needle))
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn selected_id(&self, visible: &[usize]) -> Option<String> {
        visible
            .get(self.selected)
            .and_then(|index| self.rows.get(*index))
            .map(|row| row.app.clone())
    }

    fn keep_selection(&mut self, previous: Option<String>) -> Vec<usize> {
        let visible = self.visible();
        self.selected = previous
            .as_ref()
            .and_then(|id| {
                visible
                    .iter()
                    .position(|index| self.rows[*index].app == *id)
            })
            .unwrap_or(0);
        visible
    }

    fn push_activity(&mut self, text: String) {
        self.status_line.clone_from(&text);
        self.activity.push_back(text);
        while self.activity.len() > ACTIVITY_HISTORY {
            self.activity.pop_front();
        }
    }
}

fn app_config_path(home: Option<&Path>, id: &str) -> Result<PathBuf> {
    let paths = HomePaths::resolve(home)?;
    app_config_path_in_dir(&paths.apps_d(), id)
}

fn app_config_path_in(engine: &Engine, id: &str) -> Result<PathBuf> {
    app_config_path_in_dir(&engine.home_paths().apps_d(), id)
}

fn app_config_path_in_dir(apps_dir: &Path, id: &str) -> Result<PathBuf> {
    let mut candidates = Vec::new();
    for extension in ["toml", "yaml", "yml", "json"] {
        let path = apps_dir.join(format!("{id}.{extension}"));
        if path.exists() {
            candidates.push(path);
        }
    }
    match candidates.as_slice() {
        [path] => Ok(path.clone()),
        [] => bail!("no config file found for '{id}' in {}", apps_dir.display()),
        _ => bail!(
            "multiple config files found for '{id}': {}",
            candidates
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn send_command(ui: &mut UiState, tx: &mpsc::Sender<Cmd>, command: Cmd) {
    match tx.send(command) {
        Ok(()) => ui.busy += 1,
        Err(error) => ui.push_activity(format!("worker unavailable: {error}")),
    }
}

fn request_events(ui: &mut UiState, visible: &[usize], tx: &mpsc::Sender<Cmd>) {
    if let Some(id) = ui.selected_id(visible) {
        send_command(ui, tx, Cmd::Events(id));
    } else {
        ui.detail = None;
    }
}

fn selection_changed(
    ui: &mut UiState,
    before: Option<String>,
    visible: &[usize],
    tx: &mpsc::Sender<Cmd>,
) {
    if ui.show_detail && before != ui.selected_id(visible) {
        request_events(ui, visible, tx);
    }
}

#[allow(clippy::too_many_lines)]
fn drive_ui(
    terminal: &mut ratatui::Terminal<CrosstermBackend<std::io::Stdout>>,
    home: Option<&Path>,
    cmd_tx: mpsc::Sender<Cmd>,
    msg_rx: mpsc::Receiver<UiMsg>,
) -> Result<()> {
    let mut ui = UiState::default();
    send_command(&mut ui, &cmd_tx, Cmd::Refresh);
    loop {
        loop {
            match msg_rx.try_recv() {
                Ok(UiMsg::Rows(rows)) => {
                    let visible_before = ui.visible();
                    let old_id = ui.selected_id(&visible_before);
                    ui.rows = rows;
                    let visible_after = ui.keep_selection(old_id.clone());
                    let selection_changed = old_id.as_ref() != ui.selected_id(&visible_after).as_ref();
                    if ui.show_detail && selection_changed {
                        request_events(&mut ui, &visible_after, &cmd_tx);
                    }
                    if ui.rows.is_empty() {
                        ui.status_line = "no configured apps".into();
                    }
                }
                Ok(UiMsg::Events(id, events)) => ui.detail = Some((id, events)),
                Ok(UiMsg::Plugins(plugins)) => {
                    ui.plugins = plugins;
                    ui.plugins_loaded = true;
                    if let Some(form) = ui.form.as_mut() {
                        let providers = provider_choices(&ui.plugins);
                        if !providers.iter().any(|provider| provider == &form.provider) {
                            form.provider = providers.first().cloned().unwrap_or_default();
                        }
                    }
                }
                Ok(UiMsg::Scaffold(result)) => match result {
                    Ok(path) => {
                        ui.push_activity(format!("created {}; opening $EDITOR…", path.display()));
                        let editor = with_suspended_terminal(terminal, || crate::cli::edit_config(&path));
                        match editor {
                            Ok(()) => ui.push_activity(format!("edited {}", path.display())),
                            Err(error) => ui.push_activity(format!("scaffold saved at {}; editor failed: {error:#}", path.display())),
                        }
                        send_command(&mut ui, &cmd_tx, Cmd::Refresh);
                    }
                    Err(error) => ui.push_activity(format!("cannot create app: {error}")),
                },
                Ok(UiMsg::Activity(text)) => ui.push_activity(text),
                Ok(UiMsg::Finished) => ui.busy = ui.busy.saturating_sub(1),
                Ok(UiMsg::Fatal(text)) => {
                    ui.worker_failed = true;
                    ui.busy = 0;
                    ui.push_activity(text);
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    ui.worker_failed = true;
                    ui.busy = 0;
                    ui.push_activity("worker stopped unexpectedly".into());
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }
        let visible = ui.visible();
        terminal.draw(|frame| draw_dashboard(frame, &ui, &visible))?;
        if !crossterm::event::poll(std::time::Duration::from_millis(TICK_MS))
            .map_err(anyhow::Error::msg)?
        {
            continue;
        }
        if let Event::Key(key) = crossterm::event::read().map_err(anyhow::Error::msg)?
            && key.kind == KeyEventKind::Press
            && handle_key(key, &mut ui, &visible, home, &cmd_tx, terminal)?
        {
            return Ok(());
        }
    }
}

#[allow(clippy::too_many_lines)]
fn handle_key(
    key: KeyEvent,
    ui: &mut UiState,
    visible: &[usize],
    home: Option<&Path>,
    tx: &mpsc::Sender<Cmd>,
    terminal: &mut ratatui::Terminal<CrosstermBackend<std::io::Stdout>>,
) -> Result<bool> {
    if ui.confirmation.is_some() {
        match key.code {
            KeyCode::Char('y') => confirm_action(ui, home, tx)?,
            KeyCode::Char('n') | KeyCode::Esc => ui.confirmation = None,
            _ => {}
        }
        return Ok(false);
    }
    if ui.search_input.is_some() {
        match key.code {
            KeyCode::Enter => {
                let previous = ui.selected_id(visible);
                ui.search = ui.search_input.take().unwrap_or_default();
                let new_visible = ui.keep_selection(previous.clone());
                selection_changed(ui, previous, &new_visible, tx);
            }
            KeyCode::Esc => ui.search_input = None,
            KeyCode::Backspace => {
                if let Some(input) = ui.search_input.as_mut() {
                    input.pop();
                }
            }
            KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(input) = ui.search_input.as_mut() {
                    input.push(character);
                }
            }
            _ => {}
        }
        return Ok(false);
    }
    match ui.screen {
        Screen::Help | Screen::Plugins => {
            if key.code == KeyCode::Esc {
                ui.screen = Screen::Dashboard;
            }
            return Ok(false);
        }
        Screen::NewApp => {
            handle_form_key(key, ui, tx)?;
            return Ok(false);
        }
        Screen::Dashboard => {}
    }

    if key.code == KeyCode::Char('q')
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
    {
        return Ok(true);
    }
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => {
            let before = ui.selected_id(visible);
            ui.selected = (ui.selected + 1).min(visible.len().saturating_sub(1));
            selection_changed(ui, before, visible, tx);
        }
        KeyCode::Char('k') | KeyCode::Up => {
            let before = ui.selected_id(visible);
            ui.selected = ui.selected.saturating_sub(1);
            selection_changed(ui, before, visible, tx);
        }
        KeyCode::PageDown => {
            let before = ui.selected_id(visible);
            ui.selected = (ui.selected + 10).min(visible.len().saturating_sub(1));
            selection_changed(ui, before, visible, tx);
        }
        KeyCode::PageUp => {
            let before = ui.selected_id(visible);
            ui.selected = ui.selected.saturating_sub(10);
            selection_changed(ui, before, visible, tx);
        }
        KeyCode::Char('r') if !ui.worker_failed => {
            ui.push_activity("refreshing configured apps…".into());
            send_command(ui, tx, Cmd::Refresh);
        }
        KeyCode::Char('/') => ui.search_input = Some(ui.search.clone()),
        KeyCode::Char('f') => {
            let previous = ui.selected_id(visible);
            ui.filter = ui.filter.next();
            let new_visible = ui.keep_selection(previous.clone());
            selection_changed(ui, previous, &new_visible, tx);
        }
        KeyCode::Char('d') => {
            ui.show_detail = !ui.show_detail;
            if ui.show_detail {
                request_events(ui, visible, tx);
            }
        }
        KeyCode::Char('u') => {
            if let Some(id) = ui.selected_id(visible) {
                ui.confirmation = Some(Confirmation::Update(id));
            }
        }
        KeyCode::Char('U') => {
            let mut ids: Vec<_> = ui
                .rows
                .iter()
                .filter(|row| row.enabled && matches!(row.status, Status::UpdateAvailable))
                .map(|row| row.app.clone())
                .collect();
            ids.sort();
            ui.confirmation = Some(Confirmation::BulkUpdate(ids));
        }
        KeyCode::Char('a') => {
            if let Some(id) = ui.selected_id(visible) {
                ui.confirmation = Some(Confirmation::Approve(id));
            }
        }
        KeyCode::Char('e') => {
            if let Some(id) = ui.selected_id(visible) {
                match app_config_path(home, &id) {
                    Ok(path) => {
                        ui.push_activity(format!("opening {} in $EDITOR…", path.display()));
                        let result = with_suspended_terminal(terminal, || {
                            crate::cli::edit_config(&path)
                        });
                        match result {
                            Ok(()) => ui.push_activity(format!("edited {}", path.display())),
                            Err(error) => ui.push_activity(format!("editor failed: {error:#}")),
                        }
                        send_command(ui, tx, Cmd::Refresh);
                    }
                    Err(error) => ui.push_activity(format!("cannot edit config: {error:#}")),
                }
            }
        }
        KeyCode::Char('n') => {
            let providers = provider_choices(&ui.plugins);
            let provider = providers.first().cloned().unwrap_or_else(|| "github-releases".into());
            ui.form = Some(NewAppForm {
                id: String::new(),
                provider,
                format: crate::cli::ScaffoldFormat::Toml,
                field: 0,
            });
            ui.screen = Screen::NewApp;
            if !ui.plugins_loaded {
                send_command(ui, tx, Cmd::Plugins);
            }
        }
        KeyCode::Char('x') => {
            if let Some(id) = ui.selected_id(visible) {
                match app_config_path(home, &id) {
                    Ok(path) => {
                        ui.confirmation = Some(Confirmation::DeleteConfig { id, path });
                    }
                    Err(error) => ui.push_activity(format!("cannot delete config: {error:#}")),
                }
            }
        }
        KeyCode::Char('g') => {
            if let Some(id) = ui.selected_id(visible) {
                send_command(ui, tx, Cmd::Desktop(id, false));
            }
        }
        KeyCode::Char('G') => {
            if let Some(id) = ui.selected_id(visible) {
                ui.confirmation = Some(Confirmation::RemoveDesktop(id));
            }
        }
        KeyCode::Char('p') => {
            ui.screen = Screen::Plugins;
            send_command(ui, tx, Cmd::Plugins);
        }
        KeyCode::F(1) => ui.screen = Screen::Help,
        _ => {}
    }
    Ok(false)
}

fn provider_choices(plugins: &[PluginRow]) -> Vec<String> {
    plugins
        .iter()
        .filter(|plugin| plugin.kind.starts_with("provider/"))
        .map(|plugin| plugin.key.clone())
        .collect()
}

fn handle_form_key(
    key: KeyEvent,
    ui: &mut UiState,
    tx: &mpsc::Sender<Cmd>,
) -> Result<()> {
    if key.code == KeyCode::Esc {
        ui.screen = Screen::Dashboard;
        ui.form = None;
        return Ok(());
    }
    let choices = provider_choices(&ui.plugins);
    let Some(form) = ui.form.as_mut() else {
        ui.screen = Screen::Dashboard;
        return Ok(());
    };
    match key.code {
        KeyCode::Tab => form.field = (form.field + 1) % 3,
        KeyCode::BackTab => form.field = (form.field + 2) % 3,
        KeyCode::Left | KeyCode::Up | KeyCode::Right | KeyCode::Down if form.field == 1 => {
            if !choices.is_empty() {
                let current = choices
                    .iter()
                    .position(|provider| provider == &form.provider)
                    .unwrap_or(0);
                let next = if matches!(key.code, KeyCode::Left | KeyCode::Up) {
                    current.checked_sub(1).unwrap_or(choices.len() - 1)
                } else {
                    (current + 1) % choices.len()
                };
                form.provider.clone_from(&choices[next]);
            }
        }
        KeyCode::Left | KeyCode::Up | KeyCode::Right | KeyCode::Down if form.field == 2 => {
            form.format = cycle_format(form.format, matches!(key.code, KeyCode::Left | KeyCode::Up));
        }
        KeyCode::Backspace if form.field == 0 => {
            form.id.pop();
        }
        KeyCode::Char(character)
            if form.field == 0 && !key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            form.id.push(character);
        }
        KeyCode::Enter => {
            let id = form.id.clone();
            let provider = form.provider.clone();
            let format = form.format;
            ui.screen = Screen::Dashboard;
            ui.form = None;
            ui.push_activity(format!("creating scaffold for {id}…"));
            send_command(ui, tx, Cmd::Scaffold { id, provider, format });
        }
        _ => {}
    }
    Ok(())
}

fn cycle_format(
    format: crate::cli::ScaffoldFormat,
    backwards: bool,
) -> crate::cli::ScaffoldFormat {
    use crate::cli::ScaffoldFormat::{Json, Toml, Yaml};
    match (format, backwards) {
        (Toml, false) | (Json, true) => Yaml,
        (Yaml, false) | (Toml, true) => Json,
        (Json, false) | (Yaml, true) => Toml,
    }
}
fn confirm_action(ui: &mut UiState, home: Option<&Path>, tx: &mpsc::Sender<Cmd>) -> Result<()> {
    let Some(action) = ui.confirmation.take() else {
        return Ok(());
    };
    match action {
        Confirmation::Update(id) => send_command(ui, tx, Cmd::Update(id)),
        Confirmation::BulkUpdate(ids) => {
            if ids.is_empty() {
                ui.push_activity("bulk update: no enabled update-available apps".into());
            } else {
                send_command(ui, tx, Cmd::UpdateMany(ids));
            }
        }
        Confirmation::Approve(id) => send_command(ui, tx, Cmd::Approve(id)),
        Confirmation::DeleteConfig { id, path } => {
            match app_config_path(home, &id).and_then(|current| {
                anyhow::ensure!(current == path, "config file identity changed; refusing to delete {}", path.display());
                Ok(current)
            }) {
                Ok(path) => match std::fs::remove_file(&path)
                    .with_context(|| format!("removing config {}", path.display()))
                {
                    Ok(()) => {
                        ui.push_activity(format!(
                            "removed config for {id}; installed files, desktop entry, and database history/state retained"
                        ));
                        send_command(ui, tx, Cmd::Refresh);
                    }
                    Err(error) => ui.push_activity(format!("config removal failed: {error:#}")),
                },
                Err(error) => ui.push_activity(format!("config removal failed: {error:#}")),
            }
        }
        Confirmation::RemoveDesktop(id) => send_command(ui, tx, Cmd::Desktop(id, true)),
    }
    Ok(())
}

fn with_suspended_terminal<T>(
    terminal: &mut ratatui::Terminal<CrosstermBackend<std::io::Stdout>>,
    action: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let leave_screen = crossterm::execute!(
        terminal.backend_mut(),
        crossterm::event::DisableMouseCapture,
        crossterm::terminal::LeaveAlternateScreen
    );
    let leave_raw = crossterm::terminal::disable_raw_mode();
    let operation = match (leave_screen, leave_raw) {
        (Ok(()), Ok(())) => action(),
        (Err(error), _) => Err(error).context("suspending TUI for editor"),
        (_, Err(error)) => Err(error).context("disabling raw mode for editor"),
    };
    let restore_raw = crossterm::terminal::enable_raw_mode();
    let restore_screen = crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableMouseCapture
    );
    let clear = terminal.clear();
    let mut restore_error = None;
    if let Err(error) = restore_raw {
        restore_error = Some(anyhow::Error::from(error).context("restoring raw mode"));
    }
    if let Err(error) = restore_screen {
        restore_error.get_or_insert_with(|| anyhow::Error::from(error).context("restoring TUI screen"));
    }
    if let Err(error) = clear {
        restore_error.get_or_insert_with(|| anyhow::Error::from(error).context("clearing restored TUI"));
    }
    match (operation, restore_error) {
        (Ok(value), None) => Ok(value),
        (Err(error), None) => Err(error),
        (Ok(_), Some(error)) => Err(error),
        (Err(error), Some(restore)) => Err(error.context(format!(
            "terminal restoration also failed: {restore:#}"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

fn draw_dashboard(f: &mut Frame, ui: &UiState, visible: &[usize]) {
    let areas = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(f.area());
    let updates = ui
        .rows
        .iter()
        .filter(|row| matches!(row.status, Status::UpdateAvailable))
        .count();
    let approvals = ui
        .rows
        .iter()
        .filter(|row| matches!(row.status, Status::NeedsApproval))
        .count();
    let errors = ui
        .rows
        .iter()
        .filter(|row| matches!(row.status, Status::Error(_)))
        .count();
    let disabled = ui
        .rows
        .iter()
        .filter(|row| matches!(row.status, Status::Disabled))
        .count();
    let title = Line::from(vec![
        Span::styled(
            " updatium ",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            "apps {}   updates {updates}   approvals {approvals}   errors {errors}   disabled {disabled}",
            ui.rows.len()
        )),
        if ui.busy > 0 {
            Span::styled("   ⟳ working", Style::default().fg(Color::Yellow))
        } else {
            Span::raw("")
        },
    ]);
    f.render_widget(Paragraph::new(title), areas[0]);

    match ui.screen {
        Screen::Dashboard => draw_app_screen(f, areas[1], ui, visible),
        Screen::Help => draw_help(f, areas[1]),
        Screen::Plugins => draw_plugins(f, areas[1], &ui.plugins),
        Screen::NewApp => draw_form(f, areas[1], ui),
    }

    let activity = if ui.activity.is_empty() {
        ui.status_line.clone()
    } else {
        ui.activity.iter().cloned().collect::<Vec<_>>().join(" | ")
    };
    f.render_widget(
        Paragraph::new(activity).style(Style::default().fg(Color::Yellow)),
        areas[2],
    );
    let footer = if ui.search_input.is_some() {
        "search: type text  [Enter] apply  [Esc] cancel"
    } else {
        match ui.screen {
            Screen::Dashboard => {
                "j/k/arrows move  PgUp/PgDn page  / search  f filter  r refresh  d history  u update  U bulk  a approve  e edit  n new  x delete  g/G desktop  p plugins  F1 help  q quit"
            }
            Screen::Help => "[Esc] return to dashboard",
            Screen::Plugins => "[Esc] return to dashboard",
            Screen::NewApp => "[Tab] field  arrows choose  [Enter] create/edit  [Esc] cancel",
        }
    };
    f.render_widget(Paragraph::new(footer).style(Style::default().fg(Color::Gray)), areas[3]);
    if let Some(confirmation) = ui.confirmation.as_ref() {
        draw_confirmation(f, confirmation);
    }
}

fn draw_app_screen(f: &mut Frame, area: Rect, ui: &UiState, visible: &[usize]) {
    let show_detail = ui.show_detail && area.width >= 100 && area.height >= 9;
    let (table_area, detail_area) = if show_detail {
        let columns = Layout::horizontal([Constraint::Percentage(61), Constraint::Percentage(39)])
            .split(area);
        (columns[0], Some(columns[1]))
    } else {
        (area, None)
    };
    if let Some(detail_area) = detail_area {
        render_detail(
            f,
            detail_area,
            ui.selected_id(visible).as_deref(),
            ui.rows.get(visible.get(ui.selected).copied().unwrap_or(usize::MAX)),
            ui.detail.as_ref(),
        );
    }
    let search = ui
        .search_input
        .as_ref()
        .map_or_else(|| ui.search.as_str(), String::as_str);
    let search_label = if search.is_empty() {
        "".to_string()
    } else {
        format!("  search: {search}")
    };
    let title = format!(
        "apps — {} visible / {} configured · {}{search_label}",
        visible.len(),
        ui.rows.len(),
        ui.filter.label()
    );
    let header = Row::new(["ID", "PROVIDER", "INSTALLED", "LATEST", "STATUS"])
        .style(Style::default().add_modifier(Modifier::BOLD));
    let body: Vec<Row> = visible
        .iter()
        .enumerate()
        .filter_map(|(position, index)| ui.rows.get(*index).map(|row| (position, row)))
        .map(|(position, row)| {
            let label = match &row.status {
                Status::Error(error) => format!("✖ error: {error}"),
                status => status.label().to_string(),
            };
            let selected_style = if position == ui.selected {
                Style::default()
                    .bg(Color::DarkGray)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Row::new([
                Cell::from(row.app.clone()),
                Cell::from(row.provider.clone()),
                Cell::from(row.installed.as_deref().unwrap_or("—").to_string()),
                Cell::from(row.latest.as_deref().unwrap_or("—").to_string()),
                Cell::from(label).style(status_style(&row.status)),
            ])
            .style(selected_style)
        })
        .collect();
    let table = Table::new(
        body,
        [
            Constraint::Percentage(23),
            Constraint::Percentage(19),
            Constraint::Percentage(17),
            Constraint::Percentage(17),
            Constraint::Percentage(24),
        ],
    )
    .header(header)
    .block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(table, table_area);
    if visible.is_empty() {
        let inner = Block::default().borders(Borders::ALL).inner(table_area);
        f.render_widget(ratatui::widgets::Clear, inner);
        f.render_widget(
            Paragraph::new(if ui.rows.is_empty() {
                "No configured apps. Press n to create one."
            } else {
                "No apps match the current search and status filter."
            })
            .style(Style::default().fg(Color::Gray)),
            inner,
        );
    }
}

fn status_style(status: &Status) -> Style {
    match status {
        Status::Ok => Style::default().fg(Color::Green),
        Status::UpdateAvailable => Style::default().fg(Color::Cyan),
        Status::NeedsApproval => Style::default().fg(Color::Yellow),
        Status::Error(_) => Style::default().fg(Color::Red),
        Status::Disabled | Status::NeverChecked => Style::default().fg(Color::DarkGray),
    }
}

fn render_detail(
    f: &mut Frame,
    area: Rect,
    selected: Option<&str>,
    row: Option<&UiRow>,
    detail: Option<&(String, Vec<crate::store::EventRow>)>,
) {
    let mut lines = Vec::new();
    if let Some(row) = row {
        lines.push(Line::styled(
            format!("{} · {}", row.app, row.provider),
            Style::default().add_modifier(Modifier::BOLD),
        ));
        lines.push(Line::from(format!("status: {}", row.status.label())));
        lines.push(Line::from(format!(
            "config: {}",
            row.config_path
                .as_ref()
                .map_or_else(|| row.config_issue.as_deref().unwrap_or("unresolved").to_string(), |path| path.display().to_string())
        )));
    }
    match detail {
        Some((id, events)) if Some(id.as_str()) == selected && events.is_empty() => {
            lines.push(Line::styled("recent history: no events", Style::default().fg(Color::Gray)));
        }
        Some((id, events)) if Some(id.as_str()) == selected => {
            lines.push(Line::styled(
                format!("recent history · newest {}", events.len()),
                Style::default().add_modifier(Modifier::BOLD),
            ));
            for event in events {
                lines.push(Line::from(format!(
                    "#{} {} {} — {}",
                    event.seq,
                    event.ts,
                    event.kind,
                    event.detail.as_deref().unwrap_or("")
                )));
            }
        }
        _ => lines.push(Line::from("loading selected app history…")),
    }
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL).title("selected app / history")),
        area,
    );
}

fn draw_help(f: &mut Frame, area: Rect) {
    let lines = vec![
        Line::styled("Dashboard keys", Style::default().add_modifier(Modifier::BOLD)),
        Line::from("j/k, arrows: move; PageUp/PageDown: move by page"),
        Line::from("r: refresh; /: search ID/provider; f: cycle All, Updates, Approvals, Errors, Disabled"),
        Line::from("d: toggle selected app history; u: confirm selected update; U: confirm eligible bulk updates"),
        Line::from("a: confirm approval; e: edit config in $EDITOR; n: new app form; x: delete config only"),
        Line::from("g: write desktop entry; G: confirm desktop-entry removal; p: plugin catalog"),
        Line::from("F1: help; q or Ctrl-C: quit; confirmations use y, n, or Esc"),
        Line::from("Search: printable characters, Backspace, Enter applies, Esc cancels"),
        Line::from("New app: Tab selects field, arrows cycle provider/format, Enter creates and opens editor"),
        Line::from("App removal retains installed files, desktop entries, and database history/state"),
    ];
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL).title("help · F1")),
        area,
    );
}

fn draw_plugins(f: &mut Frame, area: Rect, plugins: &[PluginRow]) {
    let header = Row::new(["KIND", "KEY", "EXECUTABLE / PATH"])
        .style(Style::default().add_modifier(Modifier::BOLD));
    let rows = plugins.iter().map(|plugin| {
        Row::new([
            Cell::from(plugin.kind.clone()),
            Cell::from(plugin.key.clone()),
            Cell::from(plugin.path.clone()),
        ])
    });
    f.render_widget(
        Table::new(
            rows,
            [Constraint::Percentage(22), Constraint::Percentage(30), Constraint::Percentage(48)],
        )
        .header(header)
        .block(Block::default().borders(Borders::ALL).title("provider / installer catalog")),
        area,
    );
}

fn draw_form(f: &mut Frame, area: Rect, ui: &UiState) {
    let Some(form) = ui.form.as_ref() else {
        return;
    };
    let providers = provider_choices(&ui.plugins);
    let fields = [
        format!("App ID: {}", form.id),
        format!("Provider: {}", if form.provider.is_empty() { "(loading catalog)" } else { &form.provider }),
        format!("Format: {}", format_label(form.format)),
    ];
    let mut lines = vec![Line::styled(
        "Create configured app",
        Style::default().add_modifier(Modifier::BOLD),
    )];
    for (index, field) in fields.into_iter().enumerate() {
        let style = if index == form.field {
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::styled(field, style));
    }
    lines.push(Line::from(format!("available provider choices: {}", providers.join(", "))));
    lines.push(Line::from("Format choices: TOML, YAML, JSON (default TOML)"));
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL).title("new app")),
        area,
    );
}

fn format_label(format: crate::cli::ScaffoldFormat) -> &'static str {
    match format {
        crate::cli::ScaffoldFormat::Toml => "TOML",
        crate::cli::ScaffoldFormat::Yaml => "YAML",
        crate::cli::ScaffoldFormat::Json => "JSON",
    }
}

fn draw_confirmation(f: &mut Frame, confirmation: &Confirmation) {
    let lines = match confirmation {
        Confirmation::Update(id) => vec![
            Line::styled("Confirm selected app update", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Line::from(format!("app: {id}")),
            Line::from("Engine default update options will be used."),
            Line::from("[y] proceed   [n/Esc] cancel"),
        ],
        Confirmation::BulkUpdate(ids) => vec![
            Line::styled("Confirm bulk update", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Line::from(format!("{} enabled update-available app(s) will be processed in ID order.", ids.len())),
            Line::from(if ids.is_empty() { "No eligible apps.".into() } else { ids.join(", ") }),
            Line::from("[y] proceed   [n/Esc] cancel"),
        ],
        Confirmation::Approve(id) => vec![
            Line::styled("Approve changed build scripts", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Line::from(format!("app: {id}")),
            Line::from("The recorded hash will be recomputed from the current checkout."),
            Line::from("[y] approve   [n/Esc] cancel"),
        ],
        Confirmation::DeleteConfig { id, path } => vec![
            Line::styled("Confirm config-only deletion", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            Line::from(format!("app: {id}")),
            Line::from(format!("config: {}", path.display())),
            Line::from("Installed files, desktop entry, and database history/state are retained."),
            Line::from("[y] delete config   [n/Esc] cancel"),
        ],
        Confirmation::RemoveDesktop(id) => vec![
            Line::styled("Confirm desktop-entry removal", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Line::from(format!("app: {id}")),
            Line::from("Only the app's desktop entry will be removed."),
            Line::from("[y] remove   [n/Esc] cancel"),
        ],
    };
    let height = (lines.len() as u16 + 2).min(f.area().height);
    let area = centered_rect(75, height, f.area());
    f.render_widget(ratatui::widgets::Clear, area);
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL).title("confirmation"))
            .style(Style::default().bg(Color::Black)),
        area,
    );
}

fn centered_rect(percent_x: u16, height: u16, r: Rect) -> Rect {
    let width = r.width.saturating_mul(percent_x) / 100;
    let height = height.min(r.height);
    Rect {
        x: r.x + r.width.saturating_sub(width) / 2,
        y: r.y + r.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, provider: &str, status: Status) -> UiRow {
        UiRow {
            app: id.into(),
            provider: provider.into(),
            enabled: true,
            installed: None,
            latest: None,
            status,
            config_path: None,
            config_issue: None,
        }
    }

    #[test]
    fn search_and_status_filter_are_conjunctive_and_case_insensitive() {
        let mut ui = UiState::default();
        ui.rows = vec![
            row("alpha", "github-releases", Status::UpdateAvailable),
            row("beta", "github-releases", Status::UpdateAvailable),
            row("gamma", "flatpak", Status::Ok),
        ];
        ui.search = "GITHUB".into();
        ui.filter = Filter::Updates;
        assert_eq!(ui.visible(), vec![0, 1]);
        ui.search = "beta".into();
        assert_eq!(ui.visible(), vec![1]);
        ui.filter = Filter::Errors;
        assert!(ui.visible().is_empty());
    }

    #[test]
    fn selection_is_retained_by_id_across_filter_changes() {
        let mut ui = UiState::default();
        ui.rows = vec![row("alpha", "command", Status::Ok), row("beta", "command", Status::UpdateAvailable)];
        let all = ui.visible();
        ui.selected = 1;
        let selected = ui.selected_id(&all);
        ui.filter = Filter::Updates;
        let visible = ui.keep_selection(selected.clone());
        assert_eq!(ui.selected_id(&visible), selected);
        assert_eq!(ui.selected_id(&visible).as_deref(), Some("beta"));
    }

    #[test]
    fn config_path_requires_one_supported_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(app_config_path_in_dir(dir.path(), "demo").is_err());
        let toml = dir.path().join("demo.toml");
        std::fs::write(&toml, "id = 'demo'").unwrap();
        assert_eq!(app_config_path_in_dir(dir.path(), "demo").unwrap(), toml);
        std::fs::write(dir.path().join("demo.yaml"), "id: demo").unwrap();
        let error = app_config_path_in_dir(dir.path(), "demo").unwrap_err().to_string();
        assert!(error.contains("multiple config files"));
    }

    #[test]
    fn dashboard_handles_narrow_and_empty_surfaces() {
        for (width, height) in [(24, 8), (48, 12), (100, 20)] {
            let backend = ratatui::backend::TestBackend::new(width, height);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            let ui = UiState::default();
            let visible = ui.visible();
            terminal.draw(|frame| draw_dashboard(frame, &ui, &visible)).unwrap();
            let buffer = terminal.backend().buffer();
            let rendered = (0..height)
                .flat_map(|y| (0..width).map(move |x| buffer[(x, y)].symbol()))
                .collect::<String>();
            assert!(rendered.contains("No configured apps"));
        }
    }
}

