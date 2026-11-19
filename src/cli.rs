//! clap CLI: subcommands, table renderer, JSON output, exit-code contract.
//!
//! Exit codes: 0 success/no-op, 1 hard error, 2 completed-with-failures or
//! pending approvals.

use crate::config::AppFile;
use crate::engine::{CheckReport, Engine, InstalledInfo, InstalledSource, Status, UpdateOpts};
use anyhow::{Context, Result, bail};
use clap::{CommandFactory as _, Parser, Subcommand, ValueEnum};
use std::io::Write as _;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "updatium",
    version,
    about = "Unified software-update tracker across GitHub releases, AppImages, version managers, source builds and Docker tags"
)]
pub struct Cli {
    /// Relocate config/state/cache under this directory (overrides UPDATIUM_HOME)
    #[arg(long, value_name = "DIR", global = true)]
    pub home: Option<PathBuf>,

    /// Emit machine-readable JSON where supported
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
    PowerShell,
    Elvish,
}

/// Output format of the `add` scaffold; mirrors config::ConfigFormat.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ScaffoldFormat {
    Toml,
    Yaml,
    Json,
}

impl ScaffoldFormat {
    fn config_format(self) -> crate::config::ConfigFormat {
        match self {
            ScaffoldFormat::Toml => crate::config::ConfigFormat::Toml,
            ScaffoldFormat::Yaml => crate::config::ConfigFormat::Yaml,
            ScaffoldFormat::Json => crate::config::ConfigFormat::Json,
        }
    }
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Write a scaffolded apps.d/<id>.<fmt> from the provider template
    Add {
        id: String,
        #[arg(long, short)]
        provider: String,
        /// Open $EDITOR on the scaffold afterwards
        #[arg(long, short)]
        edit: bool,
        /// File format of the generated app file
        #[arg(long, value_enum, default_value_t = ScaffoldFormat::Toml)]
        format: ScaffoldFormat,
    },
    /// Interactively add a GitHub AppImage or maintained recipe without installing
    Onboard {
        /// App identifier
        id: String,
        /// HTTPS GitHub repository URL, or recipe:ripgrep / recipe:fd
        repository: String,
    },
    /// Validate a trusted app configuration without installing it
    Validate {
        /// App identifier
        id: String,
    },
    /// List configured apps and their recorded state
    List {
        /// JSON output
        #[arg(long)]
        json: bool,
    },
    /// Check for updates (default: all enabled apps)
    Check {
        ids: Vec<String>,
        #[arg(long)]
        all: bool,
        /// JSON output
        #[arg(long)]
        json: bool,
    },
    /// Download/verify/build/install an update
    Update {
        id: Option<String>,
        #[arg(long)]
        all: bool,
        #[arg(long, short = 'y')]
        yes: bool,
        /// Resolve the plan only; mutate nothing
        #[arg(long, short = 'n')]
        dry_run: bool,
        /// Re-run even when already at the latest version
        #[arg(long)]
        force: bool,
    },
    /// Release an app's version pin without changing its other policies
    Unpin {
        /// App identifier
        id: String,
    },
    /// Stop tracking an app; retain its installation, integration, and history
    Untrack {
        id: String,
        /// Apply the removal of configuration after reviewing the preview
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Remove proven Updatium-owned installation files and integration
    Uninstall {
        id: String,
        /// Apply the ownership-aware removal after reviewing the preview
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Inspect or clean selected per-app cached and retained artifacts
    Cleanup {
        id: String,
        /// Include obsolete downloads (may require re-downloading)
        #[arg(long)]
        downloads: bool,
        /// Include safely identifiable stale staging directories
        #[arg(long)]
        staging: bool,
        /// Include retained rollback snapshots (destroys rollback points)
        #[arg(long)]
        retained: bool,
        /// Delete the selected artifacts after reviewing the preview
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// List or restore retained managed installation files
    Rollback {
        id: String,
        /// Retained version to restore (default: newest retained)
        #[arg(long, conflicts_with = "list")]
        version: Option<String>,
        /// List retained versions without restoring
        #[arg(long)]
        list: bool,
        /// Confirm replacement of the current managed tree
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Clear a source-build script-change gate after review
    Approve {
        id: String,
        /// Diff newest snapshot pair via `git diff --no-index`
        #[arg(long)]
        show_diff: bool,
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Show recent events (optionally per app)
    History {
        id: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: u32,
        /// JSON output
        #[arg(long)]
        json: bool,
    },
    /// List provider and installer plugins (builtins + discovered externals)
    Plugins {
        /// JSON output
        #[arg(long)]
        json: bool,
    },
    /// Manage the .desktop entry of an app
    Desktop {
        id: String,
        #[arg(long)]
        remove: bool,
    },
    /// Terminal dashboard
    Tui {
        /// Render one frame from current DB state to stdout and exit (CI)
        #[arg(long)]
        print_frame: bool,
    },
    /// Generate shell completions
    Completions {
        #[arg(value_enum)]
        shell: Shell,
    },
}

// ---------------------------------------------------------------------------
// Table renderer shared by list/check
// ---------------------------------------------------------------------------

/// Rows of (id, provider, installed, latest, status) rendered as an ASCII grid.
pub type TableRow = (String, String, InstalledInfo, Option<String>, Status);

pub(crate) fn format_installed(version: Option<&str>, source: InstalledSource) -> String {
    let label = match source {
        InstalledSource::Observed => "observed",
        InstalledSource::Recorded => "recorded",
        InstalledSource::Inferred => "inferred",
        InstalledSource::Unknown => "unknown",
    };
    match version {
        Some(version) => format!("{label}: {version}"),
        None => label.to_string(),
    }
}

pub fn render_table(rows: &[TableRow]) -> String {
    let headers = ["ID", "PROVIDER", "INSTALLED", "LATEST", "STATUS"];
    let status_str = |s: &Status| match s {
        Status::Error(e) => format!("✖ error ({e})"),
        Status::Policy(reason) => format!("policy ({reason})"),
        other => other.label().to_string(),
    };
    let body: Vec<[String; 5]> = rows
        .iter()
        .map(|(id, p, i, l, s)| {
            [
                id.clone(),
                p.clone(),
                format_installed(i.version.as_deref(), i.source),
                l.clone().unwrap_or_else(|| "—".into()),
                match &i.diagnostic {
                    Some(diagnostic) if !matches!(s, Status::Error(error) if error.contains(diagnostic)) => format!("{}; {diagnostic}", status_str(s)),
                    _ => status_str(s),
                },
            ]
        })
        .collect();

    let mut widths = [0usize; 5];
    for (i, h) in headers.iter().enumerate() {
        widths[i] = h.len();
    }
    for r in &body {
        for (i, c) in r.iter().enumerate() {
            widths[i] = widths[i].max(c.chars().count());
        }
    }

    let mut out = String::new();
    {
        out.push('+');
        for w in widths {
            out.push_str(&"-".repeat(w + 2));
            out.push('+');
        }
        out.push('\n');
    }
    let row = |out: &mut String, cells: &[String; 5]| {
        out.push('|');
        for (i, c) in cells.iter().enumerate() {
            let pad = widths[i].saturating_sub(c.chars().count());
            out.push(' ');
            out.push_str(c);
            out.push_str(&" ".repeat(pad + 1));
            out.push('|');
        }
        out.push('\n');
    };

    row(
        &mut out,
        &[
            headers[0].to_string(),
            headers[1].to_string(),
            headers[2].to_string(),
            headers[3].to_string(),
            headers[4].to_string(),
        ],
    );
    out.push('+');
    for w in widths {
        out.push_str(&"-".repeat(w + 2));
        out.push('+');
    }
    out.push('\n');
    for r in &body {
        row(&mut out, r);
    }
    out.push('+');
    for w in widths {
        out.push_str(&"-".repeat(w + 2));
        out.push('+');
    }
    out.push('\n');
    out
}

fn report_rows(reports: &[CheckReport]) -> Vec<TableRow> {
    reports
        .iter()
        .map(|r| {
            (
                r.app.clone(),
                r.provider.clone(),
                InstalledInfo { version: r.installed.clone(), source: r.installed_source, diagnostic: r.installed_diagnostic.clone() },
                r.latest.clone(),
                r.status.clone(),
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

const PROVIDER_TEMPLATES: &[(&str, &str)] = &[
    (
        "github-releases",
        r#"
id = "<ID>"
provider = "github-releases"
repo = "owner/name"              # required: owner/name
tag_regex = '^v?\d'              # optional filter on tag/release name
asset_regex = '\.AppImage$'      # optional filter on assets
include_prereleases = false

[install]
type = "appimage"                # appimage | archive | dir | deb | none

[install.desktop]
name = "<ID>"                    # writes ~/.local/share/applications/updatium.<ID>.desktop
"#,
    ),
    (
        "html-links",
        r#"
id = "<ID>"
provider = "html-links"
url = "https://example.com/download/"                       # required
link_regex = 'href="(?P<href>[^"]+\.AppImage)"'             # named group href required
version_capture = '(?P<version>\d+\.\d+(\.\d+)?)'           # named group version required
base_url = "https://example.com/"                           # resolves relative hrefs
digest_suffix = ".sha256"                                   # optional digest sidecars

[install]
type = "none"
"#,
    ),
    (
        "command",
        r#"
id = "<ID>"
provider = "command"
check_cmd = ["bash", "-c", "source ~/.nvm/nvm.sh && nvm ls-remote"]
apply_cmd = ["bash", "-c", "... nvm install {version} ..."]   # omit = track-only

[install]
type = "none"
"#,
    ),
    (
        "source-build",
        r#"
id = "<ID>"
provider = "source-build"
repo_url = "https://github.com/ggml-org/llama.cpp"
ref_regex = '^refs/tags/b\d+$'

[build]
steps = [["cmake", "-B", "build"], ["cmake", "--build", "build", "-j"]]
tracked_files = ["CMakeLists.txt", "cmake/**", "scripts/*.sh"]
install_steps = [["cmake", "--install", "build", "--prefix", "{install_dir}", "--strip"]]

[install]
type = "dir"
link_bins = []
"#,
    ),
    (
        "docker-registry",
        r#"
id = "<ID>"
provider = "docker-registry"
image = "ghcr.io/owner/name"
tag_regex = '^v\d'
pull_cmd = ["docker", "pull", "{image}:{version}"]
redeploy_cmd = ["docker", "compose", "-f", "~/compose.yaml", "up", "-d"]   # optional

[install]
type = "none"
"#,
    ),
    (
        "apt",
        r#"
id = "<ID>"
provider = "apt"
package = "htop"                  # required: apt package name
apply_cmd = ["sudo", "-n", "apt-get", "install", "--only-upgrade", "-y", "htop"]   # omit = track-only

[install]
type = "none"
"#,
    ),
    (
        "flatpak",
        r#"
id = "<ID>"
provider = "flatpak"
app_id = "org.mozilla.firefox"    # required: flatpak application id
remote = "flathub"                # required: remote name
user = true                       # --user scope (default true); false = --system
version_field = "auto"            # version | branch | auto
branch_regex = ""                 # optional regex filter on branch
apply_cmd = ["flatpak", "install", "--user", "-y", "flathub", "org.mozilla.firefox"]   # omit = track-only

[install]
type = "none"
"#,
    ),
];

fn cmd_add(cli: &Cli, id: &str, provider: &str, edit: bool, fmt: ScaffoldFormat) -> Result<()> {
    let path = create_app_scaffold(cli.home.as_deref(), id, provider, fmt)?;
    println!("wrote {}", path.display());
    if edit {
        edit_config(&path)?;
    }
    Ok(())
}

pub(crate) fn create_app_scaffold(
    home: Option<&Path>,
    id: &str,
    provider: &str,
    fmt: ScaffoldFormat,
) -> Result<PathBuf> {
    if !provider.starts_with("external:") {
        anyhow::ensure!(
            PROVIDER_TEMPLATES.iter().any(|(p, _)| *p == provider),
            "unknown provider '{provider}'"
        );
    }
    anyhow::ensure!(
        !id.is_empty()
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "app id must be [a-zA-Z0-9_-]: {id:?}"
    );
    let engine = Engine::new(home)?;
    let dir = engine.home_paths().apps_d();
    let cfg_fmt = fmt.config_format();
    let path = dir.join(format!("{id}.{}", cfg_fmt.extension()));
    if path.exists() {
        bail!("refusing to overwrite {}", path.display());
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let body = if let Some((_, tpl)) = PROVIDER_TEMPLATES.iter().find(|(p, _)| *p == provider) {
        tpl.replace("<ID>", id)
    } else {
        format!(
            r#"id = "{id}"
provider = "{provider}"

[install]
type = "none"
"#
        )
    };
    let body = if fmt == ScaffoldFormat::Toml {
        body
    } else {
        let ir = crate::config::ConfigFormat::Toml.parse(&body)?;
        cfg_fmt.serialize(&ir)?
    };
    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}


/// Remove only the policy pin from the format-independent configuration value.
fn clear_pin(home: Option<&Path>, id: &str) -> Result<bool> {
    use std::io::Read as _;
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};

    anyhow::ensure!(
        !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "app id must be [a-zA-Z0-9_-]: {id:?}"
    );
    let engine = Engine::new(home)?;
    let dir = engine.home_paths().apps_d();
    let dir_meta = std::fs::symlink_metadata(&dir).with_context(|| format!("reading {}", dir.display()))?;
    anyhow::ensure!(dir_meta.is_dir() && !dir_meta.file_type().is_symlink(), "refusing symlink or non-directory {}", dir.display());
    let mut matches = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let path = entry?.path();
        if path.file_stem().and_then(|s| s.to_str()) == Some(id)
            && path.extension().and_then(|s| s.to_str()).and_then(crate::config::ConfigFormat::from_extension).is_some()
        {
            matches.push(path);
        }
    }
    anyhow::ensure!(matches.len() == 1, "expected exactly one config for '{id}', found {}", matches.len());
    let path = &matches[0];
    let mut file = std::fs::OpenOptions::new().read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(path).with_context(|| format!("opening {} without following symlinks", path.display()))?;
    let original_meta = file.metadata()?;
    anyhow::ensure!(original_meta.is_file(), "not a regular file: {}", path.display());
    let mut original = String::new();
    file.read_to_string(&mut original)?;
    let format = crate::config::ConfigFormat::from_path(path)?;
    let mut value = format.parse(&original).with_context(|| format!("parsing {}", path.display()))?;
    anyhow::ensure!(value.get("id").and_then(|v| v.as_str()) == Some(id), "config id does not match filename: {}", path.display());
    let Some(policy) = value.get_mut("policy") else { return Ok(false) };
    let policy = policy.as_object_mut().context("policy must be a table")?;
    if policy.remove("pin").is_none() { return Ok(false); }
    let updated = remove_pin_text(format, &original, &value).with_context(|| format!(
        "cannot safely remove pin while preserving comments and formatting; edit {} and remove policy.pin manually", path.display()
    ))?;
    let mut tmp = tempfile::Builder::new().prefix(".updatium-unpin-").tempfile_in(&dir)?;
    tmp.as_file().set_permissions(original_meta.permissions())?;
    tmp.write_all(updated.as_bytes())?;
    tmp.as_file().sync_all()?;
    // Detect replacements and ordinary concurrent edits before the atomic rename.
    // A writer racing between this check and rename cannot be excluded here.
    let current = std::fs::symlink_metadata(path)?;
    anyhow::ensure!(current.is_file() && current.dev() == original_meta.dev()
        && current.ino() == original_meta.ino() && current.len() == original_meta.len()
        && current.mtime() == original_meta.mtime() && current.mtime_nsec() == original_meta.mtime_nsec()
        && current.ctime() == original_meta.ctime() && current.ctime_nsec() == original_meta.ctime_nsec(),
        "config changed while releasing pin: {}", path.display());
    tmp.persist(path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(true)
}

/// Make a narrowly scoped textual edit and verify its parsed meaning before writing.
fn remove_pin_text(format: crate::config::ConfigFormat, original: &str, expected: &serde_json::Value) -> Result<String> {
    let valid = |candidate: &str| format.parse(candidate).is_ok_and(|parsed| parsed == *expected);
    match format {
        crate::config::ConfigFormat::Toml | crate::config::ConfigFormat::Yaml => {
            let pattern = match format {
                crate::config::ConfigFormat::Toml => r"^\s*pin\s*=",
                _ => r"^\s*pin\s*:",
            };
            let key = regex::Regex::new(pattern)?;
            let mut offset = 0;
            for line in original.split_inclusive('\n') {
                // Never discard a comment, even one attached to the pin itself.
                if !line.contains('#') && key.is_match(line) {
                    let mut candidate = original.to_owned();
                    candidate.replace_range(offset..offset + line.len(), "");
                    if valid(&candidate) { return Ok(candidate); }
                }
                offset += line.len();
            }
        }
        crate::config::ConfigFormat::Json => {
            // JSON has no comments; only remove the key/value and one separator.
            let key_value = regex::Regex::new(r#""pin"\s*:\s*"(?:\\.|[^"\\])*""#)?;
            for found in key_value.find_iter(original) {
                let before = &original[..found.start()];
                let after = &original[found.end()..];
                let next = after.len() - after.trim_start().len();
                let prev = before.trim_end().len();
                let mut ranges = vec![(found.start(), found.end())];
                if after.as_bytes().get(next) == Some(&b',') {
                    ranges.push((found.start(), found.end() + next + 1));
                }
                if prev > 0 && before.as_bytes()[prev - 1] == b',' {
                    ranges.push((prev - 1, found.end()));
                }
                for (start, end) in ranges {
                    let mut candidate = original.to_owned();
                    candidate.replace_range(start..end, "");
                    if valid(&candidate) { return Ok(candidate); }
                }
            }
        }
    }
    bail!("no formatting-preserving edit found")
}

#[cfg(test)]
mod unpin_tests {
    use super::*;

    #[test]
    fn unpin_preserves_comments_and_refuses_inline_comment() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("config/apps.d");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("example.toml");
        let original = "# top\nid = 'example'\nprovider = 'github-releases'\n[policy]\n# keep\npin = '1.0'\nhold = true # keep too\n";
        std::fs::write(&path, original).unwrap();
        assert!(clear_pin(Some(home.path()), "example").unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original.replace("pin = '1.0'\n", ""));
        let inline = original.replace("pin = '1.0'", "pin = '1.0' # important");
        std::fs::write(&path, &inline).unwrap();
        assert!(clear_pin(Some(home.path()), "example").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), inline);
    }

    #[test]
    fn unpin_preserves_other_values_in_all_formats() {
        for (ext, text) in [
            ("toml", "id = 'example'\nprovider = 'github-releases'\n[policy]\npin = '1.0'\nhold = true\n"),
            ("yaml", "id: example\nprovider: github-releases\npolicy:\n  pin: '1.0'\n  hold: true\n"),
            ("json", r#"{"id":"example","provider":"github-releases","policy":{"pin":"1.0","hold":true}}"#),
        ] {
            let home = tempfile::tempdir().unwrap();
            let dir = home.path().join("config/apps.d");
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join(format!("example.{ext}"));
            std::fs::write(&path, text).unwrap();
            let fmt = crate::config::ConfigFormat::from_path(&path).unwrap();
            let mut expected = fmt.parse(text).unwrap();
            expected["policy"].as_object_mut().unwrap().remove("pin");
            assert!(clear_pin(Some(home.path()), "example").unwrap());
            assert_eq!(fmt.parse(&std::fs::read_to_string(&path).unwrap()).unwrap(), expected);
            assert!(!clear_pin(Some(home.path()), "example").unwrap());
        }
    }

    #[test]
    fn unpin_refuses_symlink() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("config/apps.d");
        std::fs::create_dir_all(&dir).unwrap();
        let target = home.path().join("target");
        std::fs::write(&target, "unchanged").unwrap();
        std::os::unix::fs::symlink(&target, dir.join("example.toml")).unwrap();
        assert!(clear_pin(Some(home.path()), "example").is_err());
        assert_eq!(std::fs::read_to_string(target).unwrap(), "unchanged");
    }
}

pub(crate) fn edit_config(path: &Path) -> Result<()> {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
    let status = std::process::Command::new(editor).arg(path).status()?;
    anyhow::ensure!(status.success(), "$EDITOR failed");
    Ok(())
}

pub(crate) fn manage_desktop(e: &Engine, id: &str, remove: bool) -> Result<()> {
    let _lock = e.lock_app(id)?;
    let app: AppFile = crate::config::load_apps(&e.home_paths().apps_d())?
        .into_iter()
        .find(|a| a.id == id)
        .with_context(|| format!("unknown app '{id}'"))?;
    let path = e.home_paths().desktop_dir.join(format!("updatium.{id}.desktop"));
    match std::fs::symlink_metadata(&path) {
        Ok(_) => anyhow::ensure!(e.desktop_is_owned(id, &path)?, "desktop entry {} is foreign or modified; left untouched", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
        Err(error) => return Err(error.into()),
    }
    if remove {
        crate::desktop::remove_desktop(&e.home_paths().desktop_dir, id)?;
        return Ok(());
    }
    let d = app
        .install
        .desktop
        .as_ref()
        .context("app has no [install.desktop]")?;
    let current = e.home_paths().install_current(id);
    let exec_hint =
        first_executable_under(&current, &app.install.link_bins).unwrap_or(current);
    let written = crate::desktop::write_desktop(
        &e.home_paths().desktop_dir,
        id,
        &d.name,
        &exec_hint,
        d.icon
            .as_deref()
            .map(crate::config::expand_tilde)
            .as_deref(),
        &d.categories,
        d.terminal,
    )?;
    e.record_desktop_ownership(&app, &written)?;
    Ok(())
}


fn print_removal_preview(preview: &crate::engine::RemovalPreview, title: &str) {
    println!("{title}: {} ({})", preview.app_id, preview.operation);
    if preview.removable.is_empty() { println!("  no removable items"); }
    for item in &preview.removable { println!("  remove: {item}"); }
    for item in &preview.preserved { println!("  preserve: {item}"); }
    for item in &preview.limitations { println!("  note: {item}"); }
}

fn summarize_exit(failures: usize, pending: usize) -> Result<i32> {
    Ok(if failures > 0 || pending > 0 { 2 } else { 0 })
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let code = dispatch(&cli)?;
    std::process::exit(code);
}

fn dispatch(cli: &Cli) -> Result<i32> {
    match &cli.command {
        Cmd::Add {
            id,
            provider,
            edit,
            format,
        } => {
            cmd_add(cli, id, provider, *edit, *format)?;
            Ok(0)
        }
        Cmd::Onboard { id, repository } => {
            tokio::runtime::Runtime::new()?.block_on(crate::onboarding::github(cli.home.as_deref(), id, repository))?;
            Ok(0)
        }
        Cmd::Validate { id } => {
            tokio::runtime::Runtime::new()?.block_on(crate::onboarding::validate_config(cli.home.as_deref(), id))?;
            Ok(0)
        }
        Cmd::Unpin { id } => {
            let changed = clear_pin(cli.home.as_deref(), id)?;
            if cli.json {
                println!("{}", serde_json::json!({"app": id, "pin_removed": changed}));
            } else if changed {
                println!("released version pin for '{id}'");
            } else {
                println!("no version pin configured for '{id}'");
            }
            Ok(0)
        }
        Cmd::Untrack { id, yes } | Cmd::Uninstall { id, yes } => {
            let engine = Engine::new(cli.home.as_deref())?;
            let untrack = matches!(&cli.command, Cmd::Untrack { .. });
            let preview = if untrack { engine.preview_untrack(id)? } else { engine.preview_uninstall(id)? };
            if !cli.json {
                print_removal_preview(&preview, "Preview");
                if !yes { println!("No changes made. Pass --yes to apply; the scope is rechecked."); }
            }
            let applied = if *yes {
                Some(if untrack { engine.apply_untrack(id, &preview)? } else { engine.apply_uninstall(id, &preview)? })
            } else { None };
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&serde_json::json!({"preview": preview, "applied": applied}))?);
            } else if let Some(result) = &applied {
                print_removal_preview(result, "Applied");
            }
            Ok(0)
        }
        Cmd::Cleanup { id, downloads, staging, retained, yes } => {
            anyhow::ensure!(*downloads || *staging || *retained, "select at least one of --downloads, --staging, or --retained");
            let home = crate::config::HomePaths::resolve(cli.home.as_deref())?;
            let options = crate::cleanup::CleanupOptions { downloads: *downloads, staging: *staging, snapshots: *retained };
            let preview = crate::cleanup::preview(&home, id, &options)?;
            if !cli.json {
                println!("Cleanup preview for {id} (current installation, configuration, history, and application data are preserved):");
                for item in &preview { println!("  {:?}: {}", item.category, item.path.display()); }
                if preview.is_empty() { println!("  no eligible artifacts"); }
                if *retained { println!("  retained snapshots selected for deletion will no longer be available for rollback"); }
                if !yes { println!("No changes made. Pass --yes to apply; candidates are rechecked."); }
            }
            let applied = if *yes { Some(crate::cleanup::apply(&home, id, &options, &preview)?) } else { None };
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&serde_json::json!({"app": id, "preview": preview, "applied": applied, "retained_selected": retained}))?);
            } else if let Some(items) = &applied {
                println!("Removed {} cleanup artifacts:", items.len());
                for item in items { println!("  {:?}: {}", item.category, item.path.display()); }
            }
            Ok(0)
        }
        Cmd::Rollback { id, version, list, yes } => {
            let engine = Engine::new(cli.home.as_deref())?;
            if *list {
                let versions = engine.rollback_versions(id)?;
                if cli.json { println!("{}", serde_json::json!({"app": id, "retained_versions": versions})); }
                else if versions.is_empty() { println!("no retained versions for '{id}'"); }
                else { for version in versions { println!("{version}"); } }
            } else {
                anyhow::ensure!(*yes, "rollback replaces the current managed tree; inspect retained versions with rollback {id} --list, then pass --yes. External effects and application data are not restored");
                let restored = engine.rollback(id, version.as_deref())?;
                if cli.json { println!("{}", serde_json::json!({"app": id, "restored_version": restored, "external_state_restored": false})); }
                else {
                    println!("restored managed installation {id} to {restored}");
                    println!("External commands, application data, binary links and desktop integration were not rolled back; inspect them before use.");
                }
            }
            Ok(0)
        }
        Cmd::List { json } => {
            let e = Engine::new(cli.home.as_deref())?;
            let rows = tokio::runtime::Runtime::new()?.block_on(e.store_list())?;
            if *json || cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &rows
                            .into_iter()
                            .map(|(id, p, i)| {
                                let pending = e.pending_update(&id)?;
                                Ok(serde_json::json!({
                                    "id": id, "provider": p, "installed": i.version,
                                    "installed_source": i.source, "installed_diagnostic": i.diagnostic, "update_pending": pending
                                }))
                            })
                            .collect::<Result<Vec<_>>>()?
                    )?
                );
            } else {
                let trows = rows
                    .into_iter()
                    .map(|(id, p, i)| {
                        let status = match e.pending_update(&id) {
                            Ok(Some(version)) => Status::Error(format!("update to {version} incomplete or running")),
                            Ok(None) if i.diagnostic.is_some() => Status::Error(i.diagnostic.clone().unwrap()),
                            Ok(None) if i.source == InstalledSource::Unknown => Status::NeverChecked,
                            Ok(None) => Status::Ok,
                            Err(error) => Status::Error(format!("{error:#}")),
                        };
                        (id, p, i, None::<String>, status)
                    })
                    .collect::<Vec<_>>();
                print!("{}", render_table(&trows));
            }
            Ok(0)
        }
        Cmd::Check { ids, all, json } => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async {
                let e = Engine::new(cli.home.as_deref())?;
                let selection: Option<Vec<String>> = if *all || ids.is_empty() {
                    None
                } else {
                    Some(ids.clone())
                };
                let reports = e.check(selection.as_deref()).await?;
                let failures: Vec<String> = reports
                    .iter()
                    .filter_map(|r| match &r.status {
                        Status::Error(msg) => Some(format!("{}: {msg}", r.app)),
                        _ => None,
                    })
                    .collect();

                if *json || cli.json {
                    let payload: Vec<_> = reports
                        .iter()
                        .map(|r| {
                            serde_json::json!({
                                "app": r.app, "provider": r.provider,
                                "installed": r.installed, "installed_source": r.installed_source,
                                "installed_diagnostic": r.installed_diagnostic,
                                "latest": r.latest,
                                "release_url": r.release_url,
                                "status": match &r.status {
                                    Status::NeverChecked => "never_checked",
                                    Status::Ok => "ok",
                                    Status::UpdateAvailable => "update_available",
                                    Status::NeedsApproval => "needs_approval",
                                    Status::Disabled => "disabled",
                                    Status::Policy(_) => "policy",
                                    Status::Error(_) => "error",
                                },
                                "error": match &r.status { Status::Error(m) => Some(m), _ => None },
                                "policy_reason": match &r.status { Status::Policy(m) => Some(m), _ => None },
                            })
                        })
                        .collect();
                    println!("{}", serde_json::to_string_pretty(&payload)?);
                } else {
                    print!("{}", render_table(&report_rows(&reports)));
                    for report in &reports {
                        if let Some(url) = &report.release_url { println!("{} release notes: {url}", report.app); }
                    }
                }

                e.notify(&reports, &failures)?;
                let pending = e.pending_approvals()?.len();
                summarize_exit(failures.len(), pending)
            })
        }
        Cmd::Update {
            id,
            all,
            yes,
            dry_run,
            force,
        } => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async {
                let e = Engine::new(cli.home.as_deref())?;
                let ids = if *all {
                    e.load_enabled_ids()?
                } else {
                    vec![id.clone().context("specify <id> or --all")?]
                };
                let opts = UpdateOpts {
                    dry_run: *dry_run,
                    yes: *yes,
                    force: *force,
                };

                let mut failures = Vec::new();
                let mut any_pending = false;
                let mut update_reports = Vec::new();
                for tid in &ids {
                    let rep = if *all { e.update_bulk(tid, &opts).await? } else { e.update(tid, &opts).await? };
                    if let Some(f) = &rep.failed {
                        failures.push(format!("{}: {f}", rep.app));
                    }
                    if !rep.integration_failures.is_empty() {
                        failures.push(format!("{}: installed, but post-install work failed: {}", rep.app, rep.integration_failures.join("; ")));
                    }
                    if e.pending_approvals()?.contains(tid) {
                        any_pending = true;
                    }
                    if cli.json {
                        update_reports.push(rep);
                    } else {
                        println!("== {}", rep.app);
                        if rep.from.is_some() || rep.to.is_some() {
                            println!("   version: {} -> {}", rep.from.as_deref().unwrap_or("unknown"), rep.to.as_deref().unwrap_or("unknown"));
                        }
                        if let Some(url) = &rep.release_url { println!("   release notes: {url}"); }
                        for a in &rep.actions {
                            println!("   {a}");
                        }
                        for w in &rep.warnings {
                            println!("   warning: {w}");
                        }
                        for failure in &rep.integration_failures {
                            println!("   POST-INSTALL FAILURE (installation retained): {failure}");
                        }
                        if let Some(f) = &rep.failed {
                            println!("   FAILED: {f}");
                        }
                    }
                }
                e.notify(&[], &failures)?;

                if cli.json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(
                            &update_reports
                                .iter()
                                .map(|r| serde_json::json!({
                                    "app": r.app, "from": r.from, "to": r.to,
                                    "track_only": r.track_only,
                                    "installed": r.installed,
                                    "recorded": r.recorded,
                                    "verified": r.verified, "release_url": r.release_url,
                                    "integration_failures": r.integration_failures,
                                    "actions": r.actions, "warnings": r.warnings,
                                    "failed": r.failed,
                                }))
                                .collect::<Vec<_>>()
                        )?
                    );
                }
                summarize_exit(failures.len(), if any_pending { 1 } else { 0 })
            })
        }
        Cmd::Approve { id, show_diff, yes } => {
            let e = Engine::new(cli.home.as_deref())?;

            if *show_diff {
                let snaps = e.newest_snapshots(id)?;
                anyhow::ensure!(!snaps.is_empty(), "no snapshots recorded for '{id}'");

                // Extract snapshots (oldest..newest) to a tmp dir for diffing.
                let tmp =
                    std::env::temp_dir().join(format!("updatium-diff-{}", std::process::id()));
                let _ = std::fs::remove_dir_all(&tmp);
                std::fs::create_dir_all(&tmp)?;
                let mut dirs = Vec::new();
                for (i, tar_path) in snaps.iter().rev().enumerate() {
                    let dst = tmp.join(format!("snap{i}"));
                    std::fs::create_dir_all(&dst)?;
                    let f = std::fs::File::open(tar_path)?;
                    let mut ar = tar::Archive::new(f);
                    ar.unpack(&dst)
                        .with_context(|| format!("unpacking {}", tar_path.display()))?;
                    dirs.push(dst);
                }
                if dirs.len() >= 2 {
                    let old = dirs[0].display().to_string();
                    let new = dirs[1].display().to_string();
                    let out = std::process::Command::new("git")
                        .args(["diff", "--no-index", "--unified=2", &old, &new])
                        .output()?;
                    print!("{}", String::from_utf8_lossy(&out.stdout));
                } else {
                    println!("only one snapshot available; nothing to diff against");
                }
                let _ = std::fs::remove_dir_all(&tmp);
            }

            if !*yes {
                print!("approve changed build scripts for '{id}'? [y/N] ");
                std::io::stdout().flush()?;
                let mut ans = String::new();
                std::io::stdin().read_line(&mut ans)?;
                if !ans.trim().eq_ignore_ascii_case("y") {
                    println!("aborted");
                    return Ok(2);
                }
            }
            let (_old, new_hash) = e.approve(id)?;
            println!(
                "approved '{id}' at hash {}…",
                &new_hash[..new_hash.len().min(12)]
            );
            Ok(0)
        }
        Cmd::History { id, limit, json } => {
            let e = Engine::new(cli.home.as_deref())?;
            let events = e.history(id.as_deref(), *limit)?;
            if *json || cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &events
                            .iter()
                            .map(|ev| serde_json::json!({
                                "seq": ev.seq, "ts": ev.ts, "app": ev.app_id,
                                "kind": ev.kind, "detail": ev.detail,
                            }))
                            .collect::<Vec<_>>()
                    )?
                );
            } else {
                for ev in events {
                    println!(
                        "{:>6}  {}  {:<14} {:<16} {}",
                        ev.seq,
                        ev.ts,
                        ev.app_id.as_deref().unwrap_or("-"),
                        ev.kind,
                        ev.detail.as_deref().unwrap_or("")
                    );
                }
            }
            Ok(0)
        }
        Cmd::Plugins { json } => {
            cmd_plugins(cli, *json)?;
            Ok(0)
        }
        Cmd::Desktop { id, remove } => {
            let e = Engine::new(cli.home.as_deref())?;
            manage_desktop(&e, id, *remove)?;
            if *remove {
                println!("removed desktop entry for '{id}' (if present)");
            } else {
                println!("wrote desktop entry for '{id}'");
            }
            Ok(0)
        }
        Cmd::Tui { print_frame } => crate::tui::run(cli.home.as_deref(), *print_frame),
        Cmd::Completions { shell } => {
            use clap_complete::{generate, shells};
            let mut cmd = Cli::command();
            match shell {
                Shell::Bash => generate(shells::Bash, &mut cmd, "updatium", &mut std::io::stdout()),
                Shell::Zsh => generate(shells::Zsh, &mut cmd, "updatium", &mut std::io::stdout()),
                Shell::Fish => generate(shells::Fish, &mut cmd, "updatium", &mut std::io::stdout()),
                Shell::PowerShell => generate(
                    shells::PowerShell,
                    &mut cmd,
                    "updatium",
                    &mut std::io::stdout(),
                ),
                Shell::Elvish => {
                    generate(shells::Elvish, &mut cmd, "updatium", &mut std::io::stdout())
                }
            }
            Ok(0)
        }
    }
}

fn first_executable_under(dir: &Path, preferred: &[String]) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    for name in preferred {
        let mut stack = vec![dir.to_path_buf()];
        while let Some(p) = stack.pop() {
            if p.is_file() && p.file_name().is_some_and(|f| f == name.as_str()) {
                return Some(p);
            }
            if p.is_dir() {
                if let Ok(rd) = std::fs::read_dir(&p) {
                    stack.extend(rd.filter_map(|e| e.ok()).map(|e| e.path()));
                }
            }
        }
    }
    // Fallback: first file carrying an exec bit.
    let mut stack = vec![dir.to_path_buf()];
    while let Some(p) = stack.pop() {
        if p.is_file() {
            if let Ok(md) = std::fs::metadata(&p) {
                if md.permissions().mode() & 0o111 != 0 {
                    return Some(p);
                }
            }
        }
        if p.is_dir() {
            if let Ok(rd) = std::fs::read_dir(&p) {
                stack.extend(rd.filter_map(|e| e.ok()).map(|e| e.path()));
            }
        }
    }
    None
}

/// `updatium plugins`: list provider and installer registries (builtins +
/// discovered external executables).
fn cmd_plugins(cli: &Cli, json: bool) -> Result<()> {
    let home = crate::config::HomePaths::resolve(cli.home.as_deref())?;
    let mut global =
        crate::config::GlobalConfig::load(&home.config_file()).with_context(|| {
            format!("loading {}", home.config_file().display())
        })?;
    global.home = home;

    let providers = crate::provider::describe_registry(&global);
    let installers = crate::installer::describe_registry(&global);

    if json || cli.json {
        let prov_json = |p: &crate::provider::PluginInfo| {
            serde_json::json!({
                "key": p.key,
                "kind": p.kind,
                "path": p.path.as_ref().map(|q| q.display().to_string()),
            })
        };
        let inst_json = |p: &crate::installer::PluginInfo| {
            serde_json::json!({
                "key": p.key,
                "kind": p.kind,
                "path": p.path.as_ref().map(|q| q.display().to_string()),
            })
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "providers": providers.iter().map(prov_json).collect::<Vec<_>>(),
                "installers": installers.iter().map(inst_json).collect::<Vec<_>>(),
            }))?
        );
        return Ok(());
    }

    // KIND | ID | SOURCE grid in the render_table style.
    let mut rows: Vec<(&str, &str, String)> = Vec::new();
    for p in &providers {
        rows.push(("provider", &p.key, source_str(p.path.as_ref())));
    }
    for p in &installers {
        rows.push(("installer", &p.key, source_str(p.path.as_ref())));
    }
    let headers = ["KIND", "ID", "SOURCE"];
    let mut widths = [0usize; 3];
    for (i, h) in headers.iter().enumerate() {
        widths[i] = h.len();
    }
    for r in &rows {
        widths[0] = widths[0].max(r.0.chars().count());
        widths[1] = widths[1].max(r.1.chars().count());
        widths[2] = widths[2].max(r.2.chars().count());
    }
    let rule = |widths: &[usize; 3]| {
        let mut out = String::from("+");
        for w in widths {
            out.push_str(&"-".repeat(w + 2));
            out.push('+');
        }
        out
    };
    let row_str = |cells: [&str; 3], widths: &[usize; 3]| {
        let mut out = String::from("|");
        for (c, w) in cells.iter().zip(widths) {
            out.push(' ');
            out.push_str(c);
            out.push_str(&" ".repeat(w - c.chars().count()));
            out.push_str(" |");
        }
        out
    };
    println!("{}", rule(&widths));
    println!("{}", row_str(headers, &widths));
    println!("{}", rule(&widths));
    for r in &rows {
        println!("{}", row_str([r.0, r.1, r.2.as_str()], &widths));
    }
    println!("{}", rule(&widths));
    Ok(())
}

fn source_str(path: Option<&std::path::PathBuf>) -> String {
    path.map(|p| p.display().to_string())
        .unwrap_or_else(|| "builtin".into())
}
