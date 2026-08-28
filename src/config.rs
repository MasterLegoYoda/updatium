//! Configuration: global `config.{toml,yaml,yml,json}`, per-app
//! `apps.d/<id>.{toml,yaml,yml,json}`, home-path resolution.
//!
//! Load-bearing: app files are parsed in TWO passes because serde's
//! `deny_unknown_fields` does not work on internally-tagged enums. Pass 1
//! parses the file via [`ConfigFormat`] into a canonical `serde_json::Value`
//! table and pops `"provider"`; pass 2 deserializes the remainder into the
//! concrete per-provider struct which DOES declare
//! `#[serde(deny_unknown_fields)]`. Typo'd keys are therefore startup errors.

pub mod format;

pub use self::format::ConfigFormat;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Home layout
// ---------------------------------------------------------------------------

/// All runtime locations. With no override each root follows XDG; with
/// `UPDATIUM_HOME` or `--home`, everything is relocated under that directory so
/// tests never touch real user data.
#[derive(Debug, Clone)]
pub struct HomePaths {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub desktop_dir: PathBuf,
    /// ~/.local/bin equivalent (relocated under UPDATIUM_HOME for tests).
    pub local_bin: PathBuf,
    /// true when UPDATIUM_HOME/--home relocated everything under one root.
    pub relocated: bool,
}

impl HomePaths {
    /// Default AppImage install target: ~/Applications normally; under the data
    /// root when relocated so tests never touch the user's real Applications.
    pub fn default_appimage_target(&self) -> PathBuf {
        if self.relocated {
            self.state_dir.join("Applications")
        } else {
            expand_tilde("~/Applications")
        }
    }
}

impl HomePaths {
    /// Precedence: `--home` flag > `UPDATIUM_HOME` env > XDG defaults.
    pub fn resolve(home_override: Option<&Path>) -> Result<HomePaths> {
        if let Some(h) = home_override {
            return Ok(Self::under(h));
        }
        if let Some(h) = std::env::var_os("UPDATIUM_HOME") {
            let h = PathBuf::from(h);
            if h.as_os_str().is_empty() {
                bail!("UPDATIUM_HOME is set but empty");
            }
            return Ok(Self::under(&h));
        }
        let cfg = dirs::config_dir()
            .context("cannot determine XDG config dir ($XDG_CONFIG_HOME)")?
            .join("updatium");
        let state = dirs::data_dir()
            .context("cannot determine XDG data dir ($XDG_DATA_HOME)")?
            .join("updatium");
        let cache = dirs::cache_dir()
            .context("cannot determine XDG cache dir ($XDG_CACHE_HOME)")?
            .join("updatium");
        let local_bin = dirs::executable_dir()
            .or_else(|| dirs::home_dir().map(|h| h.join(".local").join("bin")))
            .context("cannot determine ~/.local/bin")?;
        Ok(HomePaths {
            desktop_dir: desktop_dir_default(),
            local_bin,
            config_dir: cfg,
            state_dir: state,
            cache_dir: cache,
            relocated: false,
        })
    }

    /// Last-resort layout when XDG dirs cannot be determined (headless CI).
    pub fn fallback() -> HomePaths {
        let base = std::env::temp_dir().join("updatium-fallback-home");
        Self::under(&base)
    }

    fn under(home: &Path) -> HomePaths {
        let state = home.join("state");
        HomePaths {
            config_dir: home.join("config"),
            local_bin: state.join("bin"),
            desktop_dir: state.join("applications"),
            state_dir: state,
            cache_dir: home.join("cache"),
            relocated: true,
        }
    }

    pub fn config_file(&self) -> PathBuf {
        // Fixed precedence; an existing config.toml always keeps winning so
        // existing setups are unaffected.
        const CANDIDATES: [&str; 4] = ["config.toml", "config.yaml", "config.yml", "config.json"];
        for name in CANDIDATES {
            let p = self.config_dir.join(name);
            if p.exists() {
                return p;
            }
        }
        self.config_dir.join("config.toml")
    }
    pub fn apps_d(&self) -> PathBuf {
        self.config_dir.join("apps.d")
    }
    pub fn providers_d(&self) -> PathBuf {
        self.config_dir.join("providers.d")
    }
    pub fn installers_d(&self) -> PathBuf {
        self.config_dir.join("installers.d")
    }
    pub fn db_path(&self) -> PathBuf {
        self.state_dir.join("state.db")
    }
    pub fn downloads(&self) -> PathBuf {
        self.cache_dir.join("downloads")
    }
    pub fn installs(&self) -> PathBuf {
        self.state_dir.join("installs")
    }
    pub fn install_current(&self, id: &str) -> PathBuf {
        self.installs().join(id).join("current")
    }
    pub fn src(&self) -> PathBuf {
        self.state_dir.join("src")
    }
    pub fn script_snapshots(&self) -> PathBuf {
        self.state_dir.join("script-snapshots")
    }
}

fn desktop_dir_default() -> PathBuf {
    // ~/.local/share/applications per freedesktop spec.
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("~/.local/share"))
        .join("applications")
}

/// Expand a leading `~/` (or bare `~`) using the user's home directory.
pub fn expand_tilde(p: &str) -> PathBuf {
    if p == "~" {
        if let Some(h) = dirs::home_dir() {
            return h;
        }
    } else if let Some(rest) = p.strip_prefix("~/") {
        if let Some(h) = dirs::home_dir() {
            return h.join(rest);
        }
    }
    PathBuf::from(p)
}

// ---------------------------------------------------------------------------
// Global config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubGlobalCfg {
    /// argv whose stdout line 1 is a GitHub PAT; falls back to env GITHUB_TOKEN.
    /// Never stored plaintext anywhere.
    #[serde(default)]
    pub token_cmd: Vec<String>,
}

impl Default for GithubGlobalCfg {
    fn default() -> Self {
        GithubGlobalCfg {
            token_cmd: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GlobalConfig {
    #[serde(default = "d_max_parallel_checks")]
    pub max_parallel_checks: u32,
    /// argv template receiving a JSON summary {"updates":[...],"failures":[...]}
    /// on stdin after check --all / update --all. Empty = disabled.
    #[serde(default)]
    pub notify_cmd: Vec<String>,
    #[serde(default = "d_download_timeout_secs")]
    pub download_timeout_secs: u64,
    #[serde(default = "d_max_download_mb")]
    pub max_download_mb: u64,
    #[serde(default)]
    pub github: GithubGlobalCfg,
    /// Resolved runtime paths, injected by the engine after HomePaths::resolve.
    /// Not serialized; defaults to XDG locations.
    #[serde(skip, default = "default_home_paths")]
    pub home: HomePaths,
}

fn default_home_paths() -> HomePaths {
    HomePaths::resolve(None).unwrap_or_else(|_| HomePaths::fallback())
}

fn d_max_parallel_checks() -> u32 {
    4
}
fn d_download_timeout_secs() -> u64 {
    600
}
fn d_max_download_mb() -> u64 {
    2048
}

impl Default for GlobalConfig {
    fn default() -> Self {
        serde_json::from_str("{}").expect("static defaults parse")
    }
}

impl GlobalConfig {
    /// Missing file is fine (all defaults); malformed file names the path.
    pub fn load(path: &Path) -> Result<GlobalConfig> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(GlobalConfig::default());
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let ir = ConfigFormat::from_path(path)?
            .parse(&text)
            .with_context(|| format!("parsing {}", path.display()))?;
        serde_json::from_value(ir).with_context(|| format!("parsing {}", path.display()))
    }
}

// ---------------------------------------------------------------------------
// Per-app configs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DigestPolicy {
    Required,
    Prefer,
    Off,
}

impl Default for DigestPolicy {
    fn default() -> Self {
        DigestPolicy::Prefer
    }
}

/// How the engine installs a release. Builtins cover the three native shapes;
/// `External(name)` delegates installation to an installer plugin (registry key
/// `external:<name>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallType {
    Appimage,
    Archive,
    Dir,
    None,
    External(String),
}

impl InstallType {
    /// Registry key: `"appimage"` | `"archive"` | `"dir"` | `"external:<name>"`.
    /// `None` never reaches registries.
    pub fn key(&self) -> String {
        match self {
            InstallType::Appimage => "appimage".into(),
            InstallType::Archive => "archive".into(),
            InstallType::Dir => "dir".into(),
            InstallType::None => "none".into(),
            InstallType::External(n) => format!("external:{n}"),
        }
    }
}

impl<'de> serde::Deserialize<'de> for InstallType {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        match s.as_str() {
            "appimage" => Ok(InstallType::Appimage),
            "archive" => Ok(InstallType::Archive),
            "dir" => Ok(InstallType::Dir),
            "none" => Ok(InstallType::None),
            _ if s.starts_with("external:") => {
                let name = &s["external:".len()..];
                if name.is_empty() {
                    Err(serde::de::Error::custom(
                        "install.type \"external:\" requires a non-empty plugin name",
                    ))
                } else {
                    Ok(InstallType::External(name.to_string()))
                }
            }
            other => Err(serde::de::Error::custom(format!(
                "invalid install.type {other:?} (expected \"appimage\", \"archive\", \
                 \"dir\", \"none\" or \"external:<name>\")"
            ))),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesktopCfg {
    pub name: String,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub terminal: bool,
}

#[derive(Debug, Clone)]
pub struct InstallCfg {
    pub kind: InstallType,
    /// appimage only; default ~/Applications
    pub target: Option<String>,
    /// archive: staged paths published into current/
    pub bin_glob: Vec<String>,
    /// names symlinked into ~/.local/bin
    pub link_bins: Vec<String>,
    pub desktop: Option<DesktopCfg>,
    /// external installers only: per-invocation timeout
    pub timeout_secs: Option<u64>,
    /// Remaining `[install]` keys; only populated when `kind` is
    /// [`InstallType::External`] (passed through to the plugin as its config).
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl InstallCfg {
    /// The remaining `[install]` keys as a table value (empty table for
    /// builtins); handed to external installer plugins verbatim.
    pub fn extra_table(&self) -> serde_json::Value {
        serde_json::Value::Object(
            self.extra.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        )
    }
}

impl<'de> serde::Deserialize<'de> for InstallCfg {
    // Manual impl instead of `deny_unknown_fields`: builtin installers keep the
    // strict behavior (unknown keys are errors), while `type = "external:<name>"`
    // collects every unknown key into `extra` for pass-through to the plugin.
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct InstallVisitor;
        impl<'de> serde::de::Visitor<'de> for InstallVisitor {
            type Value = InstallCfg;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an [install] table")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<InstallCfg, A::Error> {
                let mut kind: Option<InstallType> = None;
                let mut target = None;
                let mut bin_glob = Vec::new();
                let mut link_bins = Vec::new();
                let mut desktop = None;
                let mut timeout_secs = None;
                let mut extra = BTreeMap::new();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "type" => kind = Some(map.next_value()?),
                        "target" => target = map.next_value()?,
                        "bin_glob" => bin_glob = map.next_value()?,
                        "link_bins" => link_bins = map.next_value()?,
                        "desktop" => desktop = map.next_value()?,
                        "timeout_secs" => timeout_secs = map.next_value()?,
                        other => {
                            extra.insert(other.to_string(), map.next_value::<serde_json::Value>()?);
                        }
                    }
                }
                let kind = kind.ok_or_else(|| serde::de::Error::missing_field("type"))?;
                if !extra.is_empty() && !matches!(kind, InstallType::External(_)) {
                    return Err(serde::de::Error::custom(format!(
                        "unknown [install] key(s) {}; extra [install] keys are only allowed \
                         with type = \"external:<name>\"",
                        extra.keys().cloned().collect::<Vec<_>>().join(", ")
                    )));
                }
                Ok(InstallCfg {
                    kind,
                    target,
                    bin_glob,
                    link_bins,
                    desktop,
                    timeout_secs,
                    extra,
                })
            }
        }
        d.deserialize_map(InstallVisitor)
    }
}

impl Default for InstallCfg {
    fn default() -> Self {
        InstallCfg {
            kind: InstallType::None,
            target: None,
            bin_glob: Vec::new(),
            link_bins: Vec::new(),
            desktop: None,
            timeout_secs: None,
            extra: BTreeMap::new(),
        }
    }
}

// --- provider = "github-releases" ------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubReleasesCfg {
    pub repo: String, // owner/name
    #[serde(default)]
    pub tag_regex: Option<String>,
    #[serde(default)]
    pub asset_regex: Option<String>,
    #[serde(default)]
    pub include_prereleases: bool,
    #[serde(default = "d_gh_api_base")]
    pub api_base: String,
}

fn d_gh_api_base() -> String {
    "https://api.github.com".into()
}

// --- provider = "html-links" ------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HtmlLinksCfg {
    pub url: String,
    pub link_regex: String,      // named group "href" required
    pub version_capture: String, // named group "version" required
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub digest_suffix: Option<String>,
}

// --- provider = "command" ---------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandCfg {
    pub check_cmd: Vec<String>,
    #[serde(default)]
    pub apply_cmd: Option<Vec<String>>,
}

// --- provider = "source-build" ----------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildCfg {
    pub steps: Vec<Vec<String>>,
    pub tracked_files: Vec<String>,
    pub artifacts: Vec<String>,
    #[serde(default = "d_build_timeout")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

fn d_build_timeout() -> u64 {
    3600
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceBuildCfg {
    pub repo_url: String,
    pub ref_regex: String,
    /// If set, ALSO query GitHub releases for faster metadata.
    #[serde(default)]
    pub api_base: Option<String>,
    pub build: BuildCfg,
}

// --- provider = "docker-registry" -------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DockerRegistryCfg {
    pub image: String,
    #[serde(default)]
    pub tag_regex: Option<String>,
    pub pull_cmd: Vec<String>,
    #[serde(default)]
    pub redeploy_cmd: Option<Vec<String>>,
    #[serde(default = "d_docker_api_base")]
    pub api_base: String,
}

fn d_docker_api_base() -> String {
    "https://registry.hub.docker.com".into()
}

// --- provider = "apt" ------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AptCfg {
    pub package: String,
    #[serde(default)]
    pub apply_cmd: Option<Vec<String>>,
}

// --- provider = "flatpak" --------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlatpakCfg {
    pub app_id: String,
    pub remote: String,
    #[serde(default = "d_flatpak_user")]
    pub user: bool,
    #[serde(default = "d_flatpak_version_field")]
    pub version_field: String,
    #[serde(default)]
    pub branch_regex: Option<String>,
    #[serde(default)]
    pub apply_cmd: Option<Vec<String>>,
}

fn d_flatpak_user() -> bool {
    true
}

fn d_flatpak_version_field() -> String {
    "auto".into()
}

// ---------------------------------------------------------------------------
// AppSource: what a Provider receives
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum SourceKind {
    GithubReleases(GithubReleasesCfg),
    HtmlLinks(HtmlLinksCfg),
    Command(CommandCfg),
    SourceBuild(SourceBuildCfg),
    DockerRegistry(DockerRegistryCfg),
    Apt(AptCfg),
    Flatpak(FlatpakCfg),
    /// External plugin; raw remaining config passed as JSON on stdin.
    External {
        id: String,
        raw: serde_json::Value,
    },
}

/// The type handed to `Provider::check`.
#[derive(Debug, Clone)]
pub struct AppSource {
    pub allow_insecure_http: bool,
    pub kind: SourceKind,
}

// ---------------------------------------------------------------------------
// AppFile: parsed apps.d/<id>.toml
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct AppFile {
    pub id: String,
    pub provider: String,
    pub enabled: bool,
    pub digest_policy: DigestPolicy,
    pub allow_insecure_http: bool,
    pub install: InstallCfg,
    pub source_kind: SourceKind,
}

impl AppFile {
    /// Build the `AppSource` handed to providers.
    pub fn app_source(&self) -> AppSource {
        AppSource {
            allow_insecure_http: self.allow_insecure_http,
            kind: self.source_kind.clone(),
        }
    }

    /// Two-pass parse of one app definition file. The format is inferred from
    /// the extension of `path_for_errors`, which is otherwise used only for
    /// error messages; the caller enforces filename/id.
    pub fn parse(text: &str, path_for_errors: &str) -> Result<AppFile> {
        let value = ConfigFormat::from_path(Path::new(path_for_errors))
            .and_then(|fmt| fmt.parse(text))
            .with_context(|| format!("parsing {path_for_errors}"))?;
        Self::parse_value(value, path_for_errors)
    }

    /// Two-pass parse from the canonical IR (`serde_json::Value`); exposed so
    /// app definitions can be built without going through a file format.
    pub fn parse_value(value: serde_json::Value, path_for_errors: &str) -> Result<AppFile> {
        let mut table = value
            .as_object()
            .cloned()
            .with_context(|| format!("{path_for_errors}: not a table"))?;

        let provider = match table.remove("provider") {
            Some(serde_json::Value::String(s)) => s,
            Some(_) => bail!("{path_for_errors}: key 'provider' must be a string"),
            None => bail!("{path_for_errors}: missing required key 'provider'"),
        };

        let id = match table.remove("id") {
            Some(serde_json::Value::String(s)) => s,
            Some(_) => bail!("{path_for_errors}: key 'id' must be a string"),
            None => bail!("{path_for_errors}: missing required key 'id'"),
        };
        let enabled = opt_field::<bool>(&mut table, "enabled", path_for_errors)?.unwrap_or(true);
        let digest_policy =
            opt_field::<DigestPolicy>(&mut table, "digest_policy", path_for_errors)?
                .unwrap_or_default();
        let allow_insecure_http =
            opt_field::<bool>(&mut table, "allow_insecure_http", path_for_errors)?.unwrap_or(false);
        let install =
            opt_field::<InstallCfg>(&mut table, "install", path_for_errors)?.unwrap_or_default();

        // Pass 2: everything left belongs to the provider-specific struct
        // (or the external plugin), which enforces its own unknown-field denial.
        let rest = serde_json::Value::Object(table);

        let source_kind = match provider.as_str() {
            "github-releases" => SourceKind::GithubReleases(deserialize_strict(
                &rest,
                path_for_errors,
                "github-releases",
            )?),
            "html-links" => {
                SourceKind::HtmlLinks(deserialize_strict(&rest, path_for_errors, "html-links")?)
            }
            "command" => {
                SourceKind::Command(deserialize_strict(&rest, path_for_errors, "command")?)
            }
            "source-build" => {
                SourceKind::SourceBuild(deserialize_strict(&rest, path_for_errors, "source-build")?)
            }
            "docker-registry" => SourceKind::DockerRegistry(deserialize_strict(
                &rest,
                path_for_errors,
                "docker-registry",
            )?),
            "apt" => SourceKind::Apt(deserialize_strict(&rest, path_for_errors, "apt")?),
            "flatpak" => SourceKind::Flatpak(deserialize_strict(&rest, path_for_errors, "flatpak")?),
            other if other.starts_with("external:") => SourceKind::External {
                id: other["external:".len()..].to_string(),
                raw: rest,
            },
            _ => {
                bail!(
                    "{path_for_errors}: unknown provider '{provider}' (valid: github-releases, \
                     html-links, command, source-build, docker-registry, apt, flatpak, \
                     external:<name>)"
                )
            }
        };

        Ok(AppFile {
            id,
            provider,
            enabled,
            digest_policy,
            allow_insecure_http,
            install,
            source_kind,
        })
    }
}

fn deserialize_strict<T: for<'de> Deserialize<'de>>(
    v: &serde_json::Value,
    file: &str,
    provider: &str,
) -> Result<T> {
    T::deserialize(v.clone()).map_err(|e| anyhow::Error::msg(format!("{file} [{provider}]: {e}")))
}

/// Pop and deserialize one optional key, naming file + key on type errors.
fn opt_field<T: serde::de::DeserializeOwned>(
    table: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    file: &str,
) -> Result<Option<T>> {
    match table.remove(key) {
        None => Ok(None),
        Some(v) => T::deserialize(v)
            .map(Some)
            .map_err(|e| anyhow::Error::msg(format!("{file}: invalid key '{key}': {e}"))),
    }
}

/// Load every app file under `<config>/apps.d/`. Sorted by filename for
/// deterministic ordering. Filename stem MUST equal `id`.
pub fn load_apps(apps_dir: &Path) -> Result<Vec<AppFile>> {
    let mut out = Vec::new();
    if !apps_dir.exists() {
        return Ok(out);
    }
    let mut entries: Vec<PathBuf> = std::fs::read_dir(apps_dir)
        .with_context(|| format!("reading {}", apps_dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| ConfigFormat::from_extension(e).is_some())
        })
        .collect();
    entries.sort();
    for path in entries {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .with_context(|| format!("{}: non-UTF8 filename", path.display()))?;
        let display = path.display().to_string();
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {display}"))?;
        let app = AppFile::parse(&text, &display)?;
        if app.id != stem {
            bail!(
                "{display}: id '{}' must equal filename stem '{stem}'",
                app.id
            );
        }
        out.push(app);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GH_OK: &str = r#"
        id = "freecad"
        provider = "github-releases"
        repo = "FreeCAD/FreeCAD"
        tag_regex = '^v?\d'
        asset_regex = '\.AppImage$'

        [install]
        type = "appimage"

        [install.desktop]
        name = "FreeCAD"
        categories = ["Graphics"]
        terminal = false
    "#;

    #[test]
    fn github_app_parses_with_defaults() {
        let app = AppFile::parse(GH_OK, "apps.d/freecad.toml").unwrap();
        assert_eq!(app.id, "freecad");
        assert_eq!(app.enabled, true);
        assert_eq!(app.digest_policy, DigestPolicy::Prefer);
        match &app.source_kind {
            SourceKind::GithubReleases(c) => {
                assert_eq!(c.repo, "FreeCAD/FreeCAD");
                assert_eq!(c.include_prereleases, false);
                assert_eq!(c.api_base, "https://api.github.com");
            }
            other => panic!("wrong kind {other:?}"),
        }
        assert_eq!(app.install.kind, InstallType::Appimage);
        assert_eq!(app.install.desktop.as_ref().unwrap().name, "FreeCAD");
    }

    #[test]
    fn typo_key_rejected_and_named() {
        let bad = GH_OK.replace("tag_regex", "tag_regexx");
        let err = AppFile::parse(&bad, "apps.d/freecad.toml")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("tag_regexx") && err.contains("freecad.toml"),
            "error must name key and file: {err}"
        );
    }

    #[test]
    fn unknown_provider_rejected_listing_valid() {
        let bad = "id = \"x\"\nprovider = \"gitlab\"\n";
        let err = AppFile::parse(bad, "apps.d/x.toml")
            .unwrap_err()
            .to_string();
        assert!(err.contains("github-releases"), "{err}");
    }

    #[test]
    fn external_provider_keeps_raw_config() {
        let toml_text = r#"
            id = "stub"
            provider = "external:myplug"
            url = "https://example.com/"
            depth = 3
        "#;
        let app = AppFile::parse(toml_text, "apps.d/stub.toml").unwrap();
        match app.source_kind {
            SourceKind::External { id, raw } => {
                assert_eq!(id, "myplug");
                assert_eq!(raw.get("depth").and_then(|v| v.as_i64()), Some(3));
            }
            other => panic!("wrong kind {other:?}"),
        }
    }

    #[test]
    fn source_build_parses() {
        let t = r#"
            id = "llamacpp"
            provider = "source-build"
            repo_url = "https://github.com/ggml-org/llama.cpp"
            ref_regex = '^refs/tags/b\d+$'
            [build]
            steps = [["cmake", "-B", "build"], ["cmake", "--build", "build"]]
            tracked_files = ["CMakeLists.txt"]
            artifacts = ["build/bin/*"]
            timeout_secs = 3600
            env = { CMAKE_BUILD_TYPE = "Release" }
            [install]
            type = "dir"
            link_bins = ["llama-cli"]
        "#;
        let app = AppFile::parse(t, "apps.d/llamacpp.toml").unwrap();
        match &app.source_kind {
            SourceKind::SourceBuild(c) => {
                assert_eq!(c.build.steps.len(), 2);
                assert_eq!(c.build.env["CMAKE_BUILD_TYPE"], "Release");
            }
            other => panic!("wrong kind {other:?}"),
        }
        assert_eq!(app.install.kind, InstallType::Dir);
    }

    #[test]
    fn docker_parses_defaults() {
        let t = r#"
            id = "openwebui"
            provider = "docker-registry"
            image = "ghcr.io/open-webui/open-webui"
            pull_cmd = ["docker", "pull", "{image}:{version}"]
            [install]
            type = "none"
        "#;
        let app = AppFile::parse(t, "apps.d/openwebui.toml").unwrap();
        match &app.source_kind {
            SourceKind::DockerRegistry(c) => {
                assert_eq!(c.api_base, "https://registry.hub.docker.com");
            }
            other => panic!("wrong kind {other:?}"),
        }
    }

    #[test]
    fn load_apps_enforces_filename_id_match() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("mismatch.toml");
        std::fs::write(&p, GH_OK).unwrap();
        let err = load_apps(dir.path()).unwrap_err().to_string();
        assert!(err.contains("must equal filename stem 'mismatch'"), "{err}");
    }

    #[test]
    fn load_apps_sorted_and_missing_dir_ok() {
        assert!(
            load_apps(Path::new("/nonexistent/apps.d"))
                .unwrap()
                .is_empty()
        );
        let dir = tempfile::tempdir().unwrap();
        for name in ["bbb.toml", "aaa.toml"] {
            std::fs::write(dir.path().join(name), GH_OK.replace("freecad", &name[..3])).unwrap();
        }
        let apps = load_apps(dir.path()).unwrap();
        let ids: Vec<_> = apps.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec!["aaa", "bbb"]);
    }

    #[test]
    fn global_config_defaults_and_reject() {
        let g = GlobalConfig::load(Path::new("/nonexistent/config.toml")).unwrap();
        assert_eq!(g.max_parallel_checks, 4);
        assert_eq!(g.download_timeout_secs, 600);
        assert_eq!(g.max_download_mb, 2048);

        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "max_paralel_checks = 4\n").unwrap();
        let err = GlobalConfig::load(&p).unwrap_err();
        assert!(format!("{err:#}").contains("max_paralel_checks"), "{err:#}");
    }

    #[test]
    fn home_paths_xdg_and_override() {
        // Override wins over env.
        let hp = HomePaths::resolve(Some(Path::new("/tmp/h1"))).unwrap();
        assert_eq!(
            hp.config_file(),
            PathBuf::from("/tmp/h1/config/config.toml")
        );
        assert_eq!(hp.db_path(), PathBuf::from("/tmp/h1/state/state.db"));
        assert_eq!(hp.downloads(), PathBuf::from("/tmp/h1/cache/downloads"));
        assert_eq!(hp.desktop_dir, PathBuf::from("/tmp/h1/state/applications"));
    }

    #[test]
    fn tilde_expansion() {
        let h = dirs::home_dir().unwrap();
        assert_eq!(expand_tilde("~/Applications"), h.join("Applications"));
        assert_eq!(expand_tilde("~"), h);
        assert_eq!(expand_tilde("/plain/path"), PathBuf::from("/plain/path"));
    }

    #[test]
    fn external_install_type_parses() {
        let t = r#"
            id = "tool"
            provider = "html-links"
            url = "https://example.com/"
            link_regex = 'href="([^"]+\.AppImage)"'
            version_capture = "1"
            [install]
            type = "external:copytool"
        "#;
        let app = AppFile::parse(t, "apps.d/tool.toml").unwrap();
        assert_eq!(app.install.kind, InstallType::External("copytool".into()));
        assert_eq!(app.install.kind.key(), "external:copytool");
    }

    #[test]
    fn install_extra_keys_rejected_for_builtins() {
        let t = r#"
            id = "tool"
            provider = "github-releases"
            repo = "a/b"
            [install]
            type = "appimage"
            dest = "/opt/tool"
        "#;
        let err = AppFile::parse(t, "apps.d/tool.toml").unwrap_err().to_string();
        assert!(err.contains("dest"), "{err}");
        assert!(err.contains("only allowed"), "{err}");
    }

    #[test]
    fn install_extra_keys_captured_for_external() {
        let t = r#"
            id = "tool"
            provider = "html-links"
            url = "https://example.com/"
            link_regex = 'href="([^"]+\.AppImage)"'
            version_capture = "1"
            [install]
            type = "external:copytool"
            dest = "/opt/tool"
            mode = 493
        "#;
        let app = AppFile::parse(t, "apps.d/tool.toml").unwrap();
        assert_eq!(app.install.kind.key(), "external:copytool");
        assert_eq!(app.install.extra.len(), 2);
        assert_eq!(
            app.install.extra["dest"].as_str(),
            Some("/opt/tool")
        );
        assert_eq!(app.install.extra["mode"].as_i64(), Some(493));
        // extra_table round-trips the same content.
        match app.install.extra_table() {
            serde_json::Value::Object(m) => assert_eq!(m.len(), 2),
            other => panic!("expected table, got {other:?}"),
        }
    }

    #[test]
    fn install_timeout_secs_parses() {
        let t = r#"
            id = "tool"
            provider = "html-links"
            url = "https://example.com/"
            link_regex = 'href="([^"]+\.AppImage)"'
            version_capture = "1"
            [install]
            type = "external:slow"
            timeout_secs = 30
        "#;
        let app = AppFile::parse(t, "apps.d/tool.toml").unwrap();
        assert_eq!(app.install.timeout_secs, Some(30));
    }

    #[test]
    fn empty_external_name_rejected() {
        let t = GH_OK.replace("type = \"appimage\"", "type = \"external:\"");
        let err = AppFile::parse(&t, "apps.d/freecad.toml")
            .unwrap_err()
            .to_string();
        assert!(err.contains("non-empty"), "{err}");
    }

    #[test]
    fn yaml_and_json_apps_parse_like_toml() {
        let yaml = r#"
id: freecad
provider: github-releases
repo: FreeCAD/FreeCAD
tag_regex: '^v?\d'
asset_regex: '\.AppImage$'

install:
  type: appimage
"#;
        let app_yaml = AppFile::parse(yaml, "apps.d/freecad.yaml").unwrap();
        assert_eq!(app_yaml.id, "freecad");
        assert_eq!(app_yaml.provider, "github-releases");
        assert_eq!(app_yaml.enabled, true);
        assert_eq!(app_yaml.digest_policy, DigestPolicy::Prefer);
        assert_eq!(
            app_yaml.install.kind,
            InstallType::Appimage
        );

        let json = r#"{
            "id": "freecad",
            "provider": "github-releases",
            "repo": "FreeCAD/FreeCAD",
            "tag_regex": "^v?\\d",
            "asset_regex": "\\.AppImage$",
            "install": { "type": "appimage" }
        }"#;
        let app_json = AppFile::parse(json, "apps.d/freecad.json").unwrap();
        assert_eq!(app_json.id, "freecad");
        assert_eq!(app_json.provider, "github-releases");
        match (&app_json.source_kind, &app_yaml.source_kind) {
            (
                SourceKind::GithubReleases(a),
                SourceKind::GithubReleases(b),
            ) => {
                assert_eq!(a.repo, b.repo);
                assert_eq!(a.tag_regex, b.tag_regex);
                assert_eq!(a.asset_regex, b.asset_regex);
            }
            other => panic!("wrong kinds {other:?}"),
        }
    }

    #[test]
    fn unknown_app_extension_rejected() {
        let err = AppFile::parse(GH_OK, "apps.d/freecad.ini")
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("unsupported config extension"), "{msg}");
        assert!(msg.contains(".toml"), "{msg}");
    }
    #[test]
    fn global_config_loads_yaml_and_denies_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.yaml");
        std::fs::write(&p, "max_parallel_checks: 7\n").unwrap();
        let g = GlobalConfig::load(&p).unwrap();
        assert_eq!(g.max_parallel_checks, 7);

        std::fs::write(&p, "bogus_key: 1\n").unwrap();
        let err = GlobalConfig::load(&p).unwrap_err();
        assert!(format!("{err:#}").contains("bogus_key"), "{err:#}");
    }

    #[test]
    fn config_file_probes_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let hp = HomePaths {
            config_dir: dir.path().to_path_buf(),
            state_dir: dir.path().to_path_buf(),
            cache_dir: dir.path().to_path_buf(),
            desktop_dir: dir.path().to_path_buf(),
            local_bin: dir.path().to_path_buf(),
            relocated: true,
        };
        std::fs::write(dir.path().join("config.yaml"), "").unwrap();
        assert_eq!(hp.config_file(), dir.path().join("config.yaml"));
        std::fs::write(dir.path().join("config.toml"), "").unwrap();
        assert_eq!(hp.config_file(), dir.path().join("config.toml"));
    }
}
