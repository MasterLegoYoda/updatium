//! Provider abstraction: pluggable sources of release/version information.

pub mod command;
pub mod docker_registry;
pub mod github_releases;
pub mod html_links;
pub mod source_build;

use crate::config::{AppSource, GlobalConfig};
use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::Arc;

#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> &'static str;
    /// Return all known releases/versions for the app described by `app`.
    async fn check(&self, app: &AppSource, http: &reqwest::Client) -> Result<Vec<Release>>;
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Release {
    pub version: String,
    #[serde(default)]
    pub published_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Asset {
    pub name: String,
    pub url: url::Url,
    #[serde(default)]
    pub size: Option<u64>,
    /// "sha256:<64 lowercase hex>" when known.
    #[serde(default)]
    pub digest: Option<String>,
}

impl Asset {
    pub fn digest_hex(&self) -> Option<&str> {
        match &self.digest {
            Some(d) => d.strip_prefix("sha256:"),
            None => None,
        }
    }
}

/// All registry keys a config may reference.
pub const BUILTIN_PROVIDERS: &[&str] = &[
    "github-releases",
    "html-links",
    "command",
    "source-build",
    "docker-registry",
];

/// Build the provider registry: builtins plus discovered external plugins
/// (`updatium-provider-*` on PATH and executables under `<config>/providers.d/`,
/// registered as `external:<basename>`).
pub fn registry(global: &GlobalConfig) -> HashMap<String, Arc<dyn Provider>> {
    let mut map: HashMap<String, Arc<dyn Provider>> = HashMap::new();
    map.insert(
        "github-releases".into(),
        Arc::new(github_releases::GithubReleases::new(
            global.github.token_cmd.clone(),
        )),
    );
    map.insert("html-links".into(), Arc::new(html_links::HtmlLinks));
    map.insert("command".into(), Arc::new(command::Command));
    map.insert("source-build".into(), Arc::new(source_build::SourceBuild));
    map.insert(
        "docker-registry".into(),
        Arc::new(docker_registry::DockerRegistry),
    );

    for ext in discover_external(global) {
        map.insert(format!("external:{}", ext.basename), Arc::new(ext));
    }
    map
}

/// Resolve a provider id against the registry, listing valid ids on miss.
pub fn lookup(reg: &HashMap<String, Arc<dyn Provider>>, id: &str) -> Result<Arc<dyn Provider>> {
    find(reg, id)
}

/// Same as [`lookup`] for the engine's ordered map.
pub fn lookup_btree(
    reg: &std::collections::BTreeMap<String, Arc<dyn Provider>>,
    id: &str,
) -> Result<Arc<dyn Provider>> {
    find(reg, id)
}

trait ProviderMap {
    fn get_p(&self, id: &str) -> Option<&Arc<dyn Provider>>;
    fn sorted_ids(&self) -> Vec<String>;
}

impl ProviderMap for HashMap<String, Arc<dyn Provider>> {
    fn get_p(&self, id: &str) -> Option<&Arc<dyn Provider>> {
        self.get(id)
    }
    fn sorted_ids(&self) -> Vec<String> {
        let mut v: Vec<_> = self.keys().cloned().collect();
        v.sort();
        v
    }
}

impl ProviderMap for std::collections::BTreeMap<String, Arc<dyn Provider>> {
    fn get_p(&self, id: &str) -> Option<&Arc<dyn Provider>> {
        self.get(id)
    }
    fn sorted_ids(&self) -> Vec<String> {
        self.keys().cloned().collect()
    }
}

fn find<M: ProviderMap>(reg: &M, id: &str) -> Result<Arc<dyn Provider>> {
    reg.get_p(id).map(Arc::clone).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown provider '{id}' (valid: {})",
            reg.sorted_ids().join(", ")
        )
    })
}

// ---------------------------------------------------------------------------
// External plugin discovery (protocol implemented in step 8)
// ---------------------------------------------------------------------------

pub struct ExternalProvider {
    pub basename: String,
    pub path: std::path::PathBuf,
}

/// Wire format of a plugin's `check` response.
#[derive(serde::Deserialize)]
struct PluginCheckOut {
    #[serde(default)]
    releases: Vec<Release>,
}

#[derive(serde::Deserialize)]
struct PluginDescribe {
    protocol: i64,
    id: String,
}

impl ExternalProvider {
    /// Run one plugin invocation and return its stdout (Err on nonzero exit or timeout).
    async fn invoke(&self, subcommand: &str, stdin_json: Option<&str>) -> Result<String> {
        let argv = vec![
            self.path.to_string_lossy().into_owned(),
            subcommand.to_string(),
        ];
        let out = crate::exec::run_with_stdin(
            &argv,
            None,
            std::time::Duration::from_secs(60),
            true,
            stdin_json.map(|s| s.as_bytes()),
        )
        .await?;
        anyhow::ensure!(
            out.status,
            "external provider '{}' {} failed:\n{}",
            self.basename,
            subcommand,
            out.stderr_tail
        );
        Ok(out.stdout_tail)
    }

    async fn describe(&self) -> Result<()> {
        let stdout = self.invoke("describe", None).await?;
        let d: PluginDescribe = serde_json::from_str(stdout.trim())
            .with_context(|| format!("external provider {}: bad describe JSON", self.basename))?;
        if d.protocol != 1 {
            bail!(
                "external provider '{}': unsupported protocol {} (this build speaks v1)",
                self.basename,
                d.protocol
            );
        }
        if d.id != self.basename {
            bail!(
                "external provider '{}': describe id {:?} does not match filename",
                self.basename,
                d.id
            );
        }
        Ok(())
    }
}

#[async_trait]
impl Provider for ExternalProvider {
    fn id(&self) -> &'static str {
        "external"
    }

    async fn check(&self, app: &AppSource, _http: &reqwest::Client) -> Result<Vec<Release>> {
        self.describe_cached(app).await
    }
}

impl ExternalProvider {
    async fn describe_cached(&self, app: &AppSource) -> Result<Vec<Release>> {
        // Validate the handshake once per instance per call site; cheap enough
        // to simply re-run for clarity in an MVP.
        self.describe().await?;

        let raw = match &app.kind {
            crate::config::SourceKind::External { raw, .. } => raw,
            _ => bail!("provider mismatch: external got another app kind"),
        };
        let cfg_json = serde_json::to_value(raw)?;
        let stdout = self.invoke("check", Some(&cfg_json.to_string())).await?;
        let parsed: PluginCheckOut = serde_json::from_str(stdout.trim())
            .with_context(|| format!("external provider {}: bad check JSON", self.basename))?;
        Ok(parsed.releases)
    }
}

/// Find external plugin executables: PATH entries named `updatium-provider-*`
/// plus regular executable files directly inside `<config>/providers.d/`.
pub fn discover_external(global: &GlobalConfig) -> Vec<ExternalProvider> {
    let mut found: HashMap<String, PathBufOrdered> = HashMap::new();
    struct PathBufOrdered(std::path::PathBuf);

    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for e in entries.flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if let Some(base) = name.strip_prefix("updatium-provider-") {
                        if is_executable_file(&e.path()) {
                            found
                                .entry(base.to_string())
                                .or_insert(PathBufOrdered(e.path()));
                        }
                    }
                }
            }
        }
    }

    let pdir = global.home.providers_d();
    if let Ok(entries) = std::fs::read_dir(&pdir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if is_executable_file(&e.path()) {
                // providers.d entries register under their full filename stem.
                found.insert(name, PathBufOrdered(e.path()));
            }
        }
    }

    let mut out: Vec<ExternalProvider> = found
        .into_iter()
        .map(|(basename, PathBufOrdered(path))| ExternalProvider { basename, path })
        .collect();
    out.sort_by(|a, b| a.basename.cmp(&b.basename));
    out
}

fn is_executable_file(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(p) {
        Ok(m) => m.is_file() && (m.permissions().mode() & 0o111 != 0),
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// Shared helpers used by multiple providers
// ---------------------------------------------------------------------------

/// Derive a display version from a tag: strip one leading `v`/`V` only when the
/// remainder is a non-empty digit-dot string; otherwise keep the tag verbatim.
pub fn version_from_tag(tag: &str) -> String {
    for prefix in ["v", "V"] {
        if let Some(rest) = tag.strip_prefix(prefix) {
            let ok = !rest.is_empty()
                && rest.bytes().any(|b| b.is_ascii_digit())
                && rest.bytes().all(|b| b.is_ascii_digit() || b == b'.');
            if ok {
                return rest.to_string();
            }
        }
    }
    tag.to_string()
}

/// Validate and normalize a digest string ("sha256:<hex>", hex lowercased).
pub fn normalize_digest(raw: &str) -> Option<String> {
    let hex = raw.strip_prefix("sha256:")?;
    if hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(format!("sha256:{}", hex.to_ascii_lowercase()))
    } else {
        None
    }
}

/// Parse `<hex>[ *]<filename>` lines as found in .sha256 / SHA256SUMS files.
pub fn parse_sums_line(line: &str) -> Option<(String, String)> {
    let mut it = line.trim_start().splitn(2, char::is_whitespace);
    let hex = it.next()?;
    let file = it.next()?.trim_start().trim_start_matches('*').trim();
    if hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()) && !file.is_empty() {
        Some((hex.to_ascii_lowercase(), file.to_string()))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_tag_stripping() {
        assert_eq!(version_from_tag("v1.2.3"), "1.2.3");
        assert_eq!(version_from_tag("2024.06.12"), "2024.06.12");
        assert_eq!(version_from_tag("b1234"), "b1234");
        assert_eq!(version_from_tag("v"), "v");
        assert_eq!(version_from_tag("v1.0.0-beta"), "v1.0.0-beta");
    }

    #[test]
    fn digest_normalization() {
        let up = format!("sha256:{}", "AB".repeat(32));
        assert_eq!(
            normalize_digest(&up),
            Some(format!("sha256:{}", "ab".repeat(32)))
        );
        assert_eq!(normalize_digest("md5:abcd"), None);
        assert_eq!(normalize_digest("sha256:xyz"), None);
    }

    #[test]
    fn sums_lines() {
        let hex = "a".repeat(64);
        assert_eq!(
            parse_sums_line(&format!("{hex}  app.AppImage")),
            Some((hex.clone(), "app.AppImage".into()))
        );
        assert_eq!(
            parse_sums_line(&format!("{hex} *app.AppImage")),
            Some((hex.clone(), "app.AppImage".into()))
        );
        assert_eq!(parse_sums_line("nothex file"), None);
    }
}
