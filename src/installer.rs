//! Installers: appimage / archive / dir publication with zip-slip and
//! symlink-conflict guards. The ONLY module that touches untrusted archives.

use crate::config::{GlobalConfig, HomePaths, InstallCfg};
use std::collections::HashMap;
use std::sync::Arc;
use crate::hash::collect_files;
use anyhow::{Context, Result, bail};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
#[derive(Debug)]
pub struct Publication {
    /// Paths published into current/ (relative to current/).
    pub published: Vec<String>,
    /// Binaries symlinked into the local bin dir.
    pub linked: Vec<PathBuf>,
    /// link_bins entries refused because a foreign file occupies the name.
    pub conflicts: Vec<String>,
}

// ---------------------------------------------------------------------------
// Archive format sniffing (never trust extensions)
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub enum Format {
    TarGzip,
    TarXz,
    TarBzip2,
    Zip,
    Unknown,
}

pub fn sniff(head: &[u8]) -> Format {
    if head.starts_with(&[0x1f, 0x8b]) {
        Format::TarGzip
    } else if head.starts_with(b"\xfd7zXZ\x00") {
        Format::TarXz
    } else if head.starts_with(b"BZh") {
        Format::TarBzip2
    } else if head.starts_with(b"PK\x03\x04") {
        Format::Zip
    } else {
        Format::Unknown
    }
}

fn read_head(path: &Path) -> Result<[u8; 8]> {
    let mut f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut buf = [0u8; 8];
    let n = Read::read(&mut f, &mut buf).context("sniffing archive")?;
    if n < buf.len() {
        buf[n..].fill(0);
    }
    Ok(buf)
}

// ---------------------------------------------------------------------------
// Path safety
// ---------------------------------------------------------------------------

/// Validate an untrusted relative member path: no absolute, no `..`, no NUL.
/// Returns the normalized staging-relative path.
fn safe_rel(name: &str) -> Result<PathBuf> {
    let p = Path::new(name);
    if p.is_absolute() || name.contains('\0') {
        bail!("archive member has absolute/NUL path: {name:?}");
    }
    for comp in p.components() {
        match comp {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => bail!("archive member escapes extraction dir: {name:?}"),
            _ => bail!("archive member has unsupported component: {name:?}"),
        }
    }
    Ok(p.to_path_buf())
}

// ---------------------------------------------------------------------------
// Extraction into a staging directory
// ---------------------------------------------------------------------------

/// Extract `download` into `staging` after sniffing its real format.
/// Rejects absolute/escaping paths and ALL symlink/hardlink members.
pub fn extract_archive(download: &Path, staging: &Path) -> Result<()> {
    std::fs::create_dir_all(staging).with_context(|| format!("creating {}", staging.display()))?;
    let head = read_head(download)?;
    match sniff(&head) {
        Format::TarGzip => untar(
            staging,
            flate2::read::GzDecoder::new(std::fs::File::open(download)?),
        ),
        Format::TarXz => untar(
            staging,
            xz2::read::XzDecoder::new(std::fs::File::open(download)?),
        ),
        Format::TarBzip2 => untar(
            staging,
            bzip2::read::MultiBzDecoder::new(std::fs::File::open(download)?),
        ),
        Format::Zip => unzip(download, staging),
        Format::Unknown => bail!(
            "unrecognized archive format for {} (magic {:02x?})",
            download.display(),
            &head[..4]
        ),
    }
}

fn untar<R: Read>(staging: &Path, reader: R) -> Result<()> {
    let mut ar = tar::Archive::new(reader);
    ar.set_preserve_permissions(true);
    for entry in ar.entries().context("iterating tar entries")? {
        let mut entry = entry.context("reading tar entry")?;
        let raw = entry
            .path()
            .context("entry path")?
            .to_string_lossy()
            .into_owned();
        use tar::EntryType;
        match entry.header().entry_type() {
            EntryType::Symlink | EntryType::Link => {
                bail!("tar contains link member {raw:?}: rejected");
            }
            EntryType::Regular | EntryType::GNUSparse | EntryType::Continuous => {}
            EntryType::Directory => {
                safe_rel(raw.trim_end_matches('/'))?;
                continue; // dirs created on demand
            }
            _ => bail!("tar contains unsupported member type {raw:?}"),
        }
        let rel = safe_rel(&raw)?;
        let dest = staging.join(&rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut out =
            std::fs::File::create(&dest).with_context(|| format!("creating {}", dest.display()))?;
        std::io::copy(&mut entry, &mut out).context("extracting file bytes")?;
        // Preserve exec bits from the header.
        if let Ok(mode) = entry.header().mode() {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(mode & 0o7777));
        }
    }
    Ok(())
}

fn unzip(download: &Path, staging: &Path) -> Result<()> {
    let f = std::fs::File::open(download)?;
    let mut ar =
        zip::ZipArchive::new(f).with_context(|| format!("opening zip {}", download.display()))?;
    for i in 0..ar.len() {
        let mut zf = ar.by_index(i).context("reading zip entry")?;
        let raw = zf.name().to_string();
        // Reject links outright via unix mode bits.
        if let Some(mode) = zf.unix_mode() {
            const S_IFLNK: u32 = 0o120000;
            if mode & 0o170000 == S_IFLNK {
                bail!("zip contains symlink member {raw:?}: rejected");
            }
        }
        let rel = safe_rel(&raw)?;
        if zf.is_dir() {
            continue;
        }
        let dest = staging.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut out =
            std::fs::File::create(&dest).with_context(|| format!("creating {}", dest.display()))?;
        std::io::copy(&mut zf, &mut out).context("extracting zip bytes")?;
        if let Some(mode) = zf.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                &dest,
                std::fs::Permissions::from_mode((mode >> 16) & 0o7777),
            );
        }
    }
    Ok(())
}

/// If extraction produced exactly one top-level directory containing everything,
/// flatten it in place (strip the root).
pub fn strip_single_root_dir(dir: &Path) -> Result<()> {
    let entries: Vec<std::fs::DirEntry> = std::fs::read_dir(dir)?.filter_map(|e| e.ok()).collect();
    if entries.len() != 1 || !entries[0].path().is_dir() {
        return Ok(());
    }
    let root = entries[0].path();
    let tmp = dir.join(".flatten-tmp");
    std::fs::rename(&root, &tmp).with_context(|| format!("moving {} aside", root.display()))?;
    for e in std::fs::read_dir(&tmp)?.filter_map(|e| e.ok()) {
        std::fs::rename(e.path(), dir.join(e.file_name()))
            .with_context(|| format!("moving up {}", e.path().display()))?;
    }
    std::fs::remove_dir(&tmp).ok();
    Ok(())
}

// ---------------------------------------------------------------------------
// Publication into current/
// ---------------------------------------------------------------------------

fn copy_matched(
    src_root: &Path,
    rel_files: &[(String, Vec<u8>)],
    current_dir: &Path,
) -> Result<Vec<String>> {
    let staged = current_dir.with_file_name(format!(".current-staging-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staged);
    std::fs::create_dir_all(&staged)?;

    let mut published = Vec::new();
    for (rel, _bytes) in rel_files.iter() {
        let dest = staged.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let src_path = src_root.join(rel);
        std::fs::copy(&src_path, &dest).with_context(|| format!("publishing {rel}"))?;
        // Preserve exec bits.
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&src_path) {
            let mode = meta.permissions().mode();
            if mode & 0o111 != 0 {
                let _ =
                    std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(mode & 0o7777));
            }
        }
        published.push(rel.clone());
    }

    // Cutover: only now replace the previous current/.
    if current_dir.exists() {
        std::fs::remove_dir_all(current_dir)
            .with_context(|| format!("replacing {}", current_dir.display()))?;
    }
    if let Some(parent) = current_dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(&staged, current_dir)
        .with_context(|| format!("activating {}", current_dir.display()))?;
    Ok(published)
}

/// Publish files matching `install.bin_glob` from an extracted staging dir into
/// `current_dir` (wipe-after-success cutover).
pub fn publish_from_staging(
    staging: &Path,
    install: &InstallCfg,
    current_dir: &Path,
) -> Result<Publication> {
    if install.bin_glob.is_empty() {
        bail!("archive install requires at least one bin_glob pattern");
    }
    let files = collect_files(staging, &install.bin_glob)?;
    let published = copy_matched(staging, &files, current_dir)?;
    Ok(Publication {
        published,
        linked: vec![],
        conflicts: vec![],
    })
}

/// Publish files matching `artifacts` globs from a source checkout into
/// `current_dir`.
pub fn publish_from_checkout(
    checkout: &Path,
    artifacts: &[String],
    current_dir: &Path,
) -> Result<Publication> {
    let files = collect_files(checkout, artifacts)?;
    let published = copy_matched(checkout, &files, current_dir)?;
    Ok(Publication {
        published,
        linked: vec![],
        conflicts: vec![],
    })
}

// ---------------------------------------------------------------------------
// Symlink publication (~/.local/bin)
// ---------------------------------------------------------------------------

/// Build a relative path from `from_dir` to `target`.
pub fn relative_to(from_dir: &Path, target: &Path) -> PathBuf {
    use Component::*;
    let from: Vec<_> = from_dir.components().collect();
    let to: Vec<_> = target.components().collect();
    let mut common = 0;
    while common < from.len() && common < to.len() && from[common] == to[common] {
        common += 1;
    }
    let mut out = PathBuf::new();
    for _ in common..from.len() {
        out.push("..");
    }
    for c in &to[common..] {
        match c {
            Normal(p) => out.push(p),
            _ => {}
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// For each name in `link_bins`, find `<current>/…/<name>` and symlink it into
/// `bin_dir`. Existing entries are touched ONLY if they are our own symlinks
/// pointing under this app's installs/<app_id>/ directory; otherwise the name is
/// reported as a conflict and left untouched.
///
/// Returns the updated [`Publication`] with `linked` + `conflicts` filled.
pub fn link_bins(
    mut pubn: Publication,
    current_dir: &Path,
    app_id: &str,
    installs_root: &Path,
    names: &[String],
    bin_dir: &Path,
) -> Result<Publication> {
    std::fs::create_dir_all(bin_dir)?;
    'names: for name in names {
        // Find first (sorted) artifact whose basename matches the requested bin.
        let mut found: Option<PathBuf> = None;
        for e in walkdir::WalkDir::new(current_dir)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if e.file_type().is_file() && e.file_name().to_string_lossy() == *name {
                found = Some(e.path().to_path_buf());
                break;
            }
        }
        let Some(target_abs) = found else {
            pubn.conflicts
                .push(format!("{name}: not present among installed artifacts"));
            continue;
        };

        let dest = bin_dir.join(name);
        if dest.symlink_metadata().is_ok() {
            // Only replace when it is our own symlink under this app's installs.
            let ours = std::fs::read_link(&dest).ok().is_some_and(|l| {
                let abs = bin_dir.join(&l);
                let abs = abs.canonicalize().unwrap_or(abs);
                abs.starts_with(installs_root.join(app_id))
                    || l.to_string_lossy()
                        .contains(&format!("/installs/{app_id}/"))
            });
            if !ours {
                pubn.conflicts.push(format!(
                    "{name}: foreign entry at {}, left untouched",
                    dest.display()
                ));
                continue 'names;
            }
            std::fs::remove_file(&dest).ok();
        }

        let rel = relative_to(bin_dir, &target_abs);
        #[cfg(unix)]
        std::os::unix::fs::symlink(&rel, &dest)
            .with_context(|| format!("linking {} -> {:?}", dest.display(), rel))?;
        pubn.linked.push(dest);
    }
    Ok(pubn)
}

// ---------------------------------------------------------------------------
// AppImage installation
// ---------------------------------------------------------------------------

/// Copy an already-downloaded AppImage into `target/<filename>` atomically and
/// mark it executable. Returns the installed path.
pub fn install_appimage(download: &Path, filename: &str, target: &Path) -> Result<PathBuf> {
    let safe_name = filename.rsplit('/').next().unwrap_or(filename);
    anyhow::ensure!(
        !safe_name.is_empty() && !safe_name.contains('\0') && safe_name != "." && safe_name != "..",
        "unsafe AppImage filename {filename:?}"
    );
    std::fs::create_dir_all(target).with_context(|| format!("creating {}", target.display()))?;
    let dest = target.join(safe_name);
    let tmp = target.join(format!(".{}.tmp-{}", safe_name, std::process::id()));
    std::fs::copy(download, &tmp).with_context(|| format!("copying into {}", tmp.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&tmp, &dest).with_context(|| format!("activating {}", dest.display()))?;
    Ok(dest)
}

// ---------------------------------------------------------------------------
// Installer plugins: builtin shapes + external executables (protocol v2)
// ---------------------------------------------------------------------------

/// Everything one installation needs: the app identity, the resolved `[install]`
/// config, and whatever inputs the engine has prepared for the install shape
/// (a digest-verified download, a source checkout, or nothing at all).
pub struct InstallCtx<'a> {
    pub app_id: &'a str,
    pub version: &'a str,
    pub cfg: &'a InstallCfg,
    /// Remaining `[install]` keys as a table; empty table for builtins.
    pub extra: &'a toml::Value,
    /// Digest-verified download (asset apps).
    pub download: Option<&'a Path>,
    /// Asset filename.
    pub filename: Option<&'a str>,
    /// Source-build checkout.
    pub checkout: Option<&'a Path>,
    /// Source-build artifact globs (dir installer).
    pub artifacts: &'a [String],
    pub home: &'a HomePaths,
}

/// What an installer did. `publication: Some` hands the post-publish tail
/// (link_bins, desktop, state) back to the engine; otherwise the engine runs
/// the plain finish path with `exec_hint`.
#[derive(Debug)]
pub struct InstallOutcome {
    /// Appended to UpdateReport.actions verbatim.
    pub actions: Vec<String>,
    /// Desktop Exec target; None → engine derives/omits.
    pub exec_hint: Option<PathBuf>,
    /// Some → engine runs publish_common tail.
    pub publication: Option<Publication>,
}

#[async_trait::async_trait]
pub trait Installer: Send + Sync {
    /// Registry key.
    fn key(&self) -> String;
    /// External installers override → false: the engine then never assumes a
    /// conventional executable layout when writing desktop entries.
    fn core_desktop(&self) -> bool {
        true
    }
    async fn install(&self, ctx: &InstallCtx) -> Result<InstallOutcome>;
}

struct AppimageInstaller;

#[async_trait::async_trait]
impl Installer for AppimageInstaller {
    fn key(&self) -> String {
        "appimage".into()
    }
    async fn install(&self, ctx: &InstallCtx) -> Result<InstallOutcome> {
        let (download, filename) = match (ctx.download, ctx.filename) {
            (Some(d), Some(f)) => (d, f),
            _ => bail!("appimage install requires a downloaded asset"),
        };
        let target_dir = match &ctx.cfg.target {
            Some(t) => crate::config::expand_tilde(t),
            None => ctx.home.default_appimage_target(),
        };
        let dest = install_appimage(download, filename, &target_dir)?;
        Ok(InstallOutcome {
            actions: vec![format!("installed {}", dest.display())],
            exec_hint: Some(dest),
            publication: None,
        })
    }
}

struct ArchiveInstaller;

#[async_trait::async_trait]
impl Installer for ArchiveInstaller {
    fn key(&self) -> String {
        "archive".into()
    }
    async fn install(&self, ctx: &InstallCtx) -> Result<InstallOutcome> {
        let Some(download) = ctx.download else {
            bail!("archive install requires a downloaded asset");
        };
        let staging = ctx
            .home
            .installs()
            .join(ctx.app_id)
            .join(format!(".staging-{}", ctx.version));
        let _ = std::fs::remove_dir_all(&staging);
        extract_archive(download, &staging)?;
        strip_single_root_dir(&staging)?;
        let pubn = publish_from_staging(&staging, ctx.cfg, &ctx.home.install_current(ctx.app_id))?;
        Ok(InstallOutcome {
            actions: vec![format!("published {} artifacts", pubn.published.len())],
            exec_hint: None,
            publication: Some(pubn),
        })
    }
}

struct DirInstaller;

#[async_trait::async_trait]
impl Installer for DirInstaller {
    fn key(&self) -> String {
        "dir".into()
    }
    async fn install(&self, ctx: &InstallCtx) -> Result<InstallOutcome> {
        let Some(checkout) = ctx.checkout else {
            bail!("install.type dir requires a source-build checkout");
        };
        let pubn = publish_from_checkout(checkout, ctx.artifacts, &ctx.home.install_current(ctx.app_id))?;
        Ok(InstallOutcome {
            actions: vec![format!("published {} artifacts", pubn.published.len())],
            exec_hint: None,
            publication: Some(pubn),
        })
    }
}

/// An external installer plugin executable (protocol v2 `install` verb).
#[derive(Debug)]
pub struct ExternalInstaller {
    pub basename: String,
    pub path: PathBuf,
}

/// Wire format of an external plugin's `describe` response (v2 adds `verbs`).
#[derive(serde::Deserialize)]
struct InstallerDescribe {
    protocol: i64,
    id: String,
    #[serde(default)]
    verbs: Vec<String>,
}

/// Wire format of an external installer's `install` response.
#[derive(serde::Deserialize)]
struct PluginInstallOut {
    #[serde(default)]
    actions: Vec<String>,
    exec: Option<String>,
    /// Part of the wire contract (what the plugin claims it installed);
    /// parsed for completeness, not consumed by the engine.
    #[allow(dead_code)]
    #[serde(default)]
    installed: Vec<String>,
}

impl ExternalInstaller {
    async fn invoke(&self, subcommand: &str, stdin_json: Option<&str>) -> Result<String> {
        let argv = vec![self.path.to_string_lossy().into_owned(), subcommand.to_string()];
        let out = crate::exec::run_with_stdin(
            &argv,
            None,
            std::time::Duration::from_secs(60),
            true,
            stdin_json.map(|s| s.as_bytes()),
            &[],
        )
        .await?;
        anyhow::ensure!(
            out.status,
            "external installer '{}' {} failed:\n{}",
            self.basename,
            subcommand,
            out.stderr_tail
        );
        Ok(out.stdout_tail)
    }

    /// Handshake: protocol >= 2, the `install` verb declared, id matches filename.
    async fn handshake(&self) -> Result<()> {
        let stdout = self.invoke("describe", None).await?;
        let d: InstallerDescribe = serde_json::from_str(stdout.trim())
            .with_context(|| format!("external installer {}: bad describe JSON", self.basename))?;
        if d.protocol < 2 {
            bail!(
                "external installer '{}': unsupported protocol {} (this build speaks v2)",
                self.basename,
                d.protocol
            );
        }
        if !d.verbs.iter().any(|v| v == "install") {
            bail!(
                "external installer '{}': does not declare the 'install' verb",
                self.basename
            );
        }
        if d.id != self.basename {
            bail!(
                "external installer '{}': describe id {:?} does not match filename",
                self.basename,
                d.id
            );
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Installer for ExternalInstaller {
    fn key(&self) -> String {
        format!("external:{}", self.basename)
    }

    fn core_desktop(&self) -> bool {
        false
    }

    async fn install(&self, ctx: &InstallCtx) -> Result<InstallOutcome> {
        self.handshake().await?;
        let home_root = home_root_path(ctx.home);
        let payload = serde_json::json!({
            "app": ctx.app_id,
            "version": ctx.version,
            "config": serde_json::to_value(ctx.extra)?,
            "paths": {
                "download": ctx.download.map(|p| p.to_string_lossy().into_owned()),
                "checkout": ctx.checkout.map(|p| p.to_string_lossy().into_owned()),
                "home": home_root.as_ref().map(|p| p.to_string_lossy().into_owned()),
            },
        });
        let payload_str = payload.to_string();
        // UPDATIUM_HOME is only set when the data root was relocated; under XDG
        // there is no single root to hand out.
        let home_str = home_root.map(|p| p.to_string_lossy().into_owned());
        let mut env_pairs: Vec<(&str, &str)> = Vec::new();
        if let Some(hs) = &home_str {
            env_pairs.push(("UPDATIUM_HOME", hs));
        }
        let argv = vec![
            self.path.to_string_lossy().into_owned(),
            "install".to_string(),
        ];
        let timeout = std::time::Duration::from_secs(ctx.cfg.timeout_secs.unwrap_or(600));
        let out = crate::exec::run_with_stdin(
            &argv,
            None,
            timeout,
            true,
            Some(payload_str.as_bytes()),
            &env_pairs,
        )
        .await?;
        anyhow::ensure!(
            out.status,
            "external installer '{}' install failed:\n{}",
            self.basename,
            out.stderr_tail
        );
        let parsed: PluginInstallOut = serde_json::from_str(out.stdout_tail.trim())
            .with_context(|| format!("external installer {}: bad install JSON", self.basename))?;
        Ok(InstallOutcome {
            actions: parsed.actions,
            exec_hint: parsed.exec.map(PathBuf::from),
            publication: None,
        })
    }
}

/// The unified data root when relocated under `UPDATIUM_HOME`/`--home`;
/// otherwise None (XDG spreads dirs across several roots).
fn home_root_path(home: &HomePaths) -> Option<&Path> {
    if home.relocated {
        home.config_dir.parent()
    } else {
        None
    }
}

/// Build the installer registry: builtins plus discovered external plugins
/// (`updatium-installer-*` on PATH and executables under `<config>/installers.d/`,
/// registered as `external:<basename>`).
pub fn registry(global: &GlobalConfig) -> HashMap<String, Arc<dyn Installer>> {
    let mut map: HashMap<String, Arc<dyn Installer>> = HashMap::new();
    map.insert("appimage".into(), Arc::new(AppimageInstaller));
    map.insert("archive".into(), Arc::new(ArchiveInstaller));
    map.insert("dir".into(), Arc::new(DirInstaller));
    for ext in discover_external(global) {
        let key = format!("external:{}", ext.basename);
        map.insert(key, Arc::new(ext));
    }
    map
}

/// Resolve a registry key against the installer registry, listing valid keys on miss.
pub fn lookup(reg: &HashMap<String, Arc<dyn Installer>>, key: &str) -> Result<Arc<dyn Installer>> {
    reg.get(key).map(Arc::clone).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown installer '{key}' (valid: {})",
            sorted_keys(reg).join(", ")
        )
    })
}

fn sorted_keys(reg: &HashMap<String, Arc<dyn Installer>>) -> Vec<String> {
    let mut v: Vec<_> = reg.keys().cloned().collect();
    v.sort();
    v
}

/// Find external installer executables: PATH entries named
/// `updatium-installer-*` plus regular executable files directly inside
/// `<config>/installers.d/` (registered under their exact file name).
pub fn discover_external(global: &GlobalConfig) -> Vec<ExternalInstaller> {
    let mut found: HashMap<String, PathBuf> = HashMap::new();

    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for e in entries.flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if let Some(base) = name.strip_prefix("updatium-installer-") {
                        if crate::provider::is_executable_file(&e.path()) {
                            found.entry(base.to_string()).or_insert_with(|| e.path());
                        }
                    }
                }
            }
        }
    }

    let idir = global.home.installers_d();
    if let Ok(entries) = std::fs::read_dir(&idir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if crate::provider::is_executable_file(&e.path()) {
                found.insert(name, e.path());
            }
        }
    }

    out_sort(found)
}

fn out_sort(found: HashMap<String, PathBuf>) -> Vec<ExternalInstaller> {
    let mut out: Vec<ExternalInstaller> = found
        .into_iter()
        .map(|(basename, path)| ExternalInstaller { basename, path })
        .collect();
    out.sort_by(|a, b| a.basename.cmp(&b.basename));
    out
}

/// One row of [`describe_registry`].
pub struct PluginInfo {
    pub key: String,
    /// `"builtin"` or `"external"`.
    pub kind: &'static str,
    /// Executable path for externals; None for builtins.
    pub path: Option<PathBuf>,
}

/// Registry contents for `updatium plugins`: builtins first, externals after.
pub fn describe_registry(global: &GlobalConfig) -> Vec<PluginInfo> {
    let mut out = vec![
        PluginInfo { key: "appimage".into(), kind: "builtin", path: None },
        PluginInfo { key: "archive".into(), kind: "builtin", path: None },
        PluginInfo { key: "dir".into(), kind: "builtin", path: None },
    ];
    let mut ext: Vec<_> = discover_external(global)
        .into_iter()
        .map(|e| PluginInfo {
            key: format!("external:{}", e.basename),
            kind: "external",
            path: Some(e.path),
        })
        .collect();
    ext.sort_by(|a, b| a.key.cmp(&b.key));
    out.extend(ext);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_zip(path: &Path, entries: &[(&str, &[u8])], mode: Option<u32>) {
        let f = std::fs::File::create(path).unwrap();
        let mut w = zip::ZipWriter::new(f);
        let opts: zip::write::SimpleFileOptions =
            zip::write::SimpleFileOptions::default().unix_permissions(mode.unwrap_or(0o644));
        for (name, data) in entries {
            w.start_file(*name, opts).unwrap();
            std::io::Write::write_all(&mut w, data).unwrap();
        }
        w.finish().unwrap();
    }

    #[test]
    fn magic_sniffing() {
        assert_eq!(sniff(&[0x1f, 0x8b, 0, 0]), Format::TarGzip);
        assert_eq!(sniff(b"\xfd7zXZ\x00abc"), Format::TarXz);
        assert_eq!(sniff(b"BZh9xx"), Format::TarBzip2);
        assert_eq!(sniff(b"PK\x03\x04rest"), Format::Zip);
        assert_eq!(sniff(b"MZ...."), Format::Unknown);
    }

    #[test]
    fn zip_slip_paths_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let zipped = dir.path().join("evil.zip");
        let outside = tempfile::tempdir().unwrap();

        // ../ escape payload
        write_zip(&zipped, &[("../evil.txt", b"gotcha")], None);
        let staging = dir.path().join("stage1");
        assert!(extract_archive(&zipped, &staging).is_err());
        assert!(!outside.path().join("evil.txt").exists());
        assert!(
            std::fs::read_dir(&staging).unwrap().next().is_none(),
            "nothing extracted"
        );

        // Absolute path payload
        let zipped2 = dir.path().join("abs.zip");
        write_zip(
            &zipped2,
            &(vec![("/tmp/updatium-evil", b"x" as &[u8])]),
            None,
        );
        assert!(extract_archive(&zipped2, &dir.path().join("stage2")).is_err());

        // Nested .. inside a longer path also rejected
        let zipped3 = dir.path().join("nest.zip");
        write_zip(&zipped3, &[("a/b/../../../evil2.txt", b"x")], None);
        assert!(extract_archive(&zipped3, &dir.path().join("stage3")).is_err());
    }

    #[test]
    fn zip_symlink_member_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("link.zip");
        let f = std::fs::File::create(&p).unwrap();
        let mut w = zip::ZipWriter::new(f);
        w.add_symlink(
            "innocent-name",
            "/etc/passwd",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        w.finish().unwrap();
        let err = extract_archive(&p, &dir.path().join("stage")).err();
        let err = err.map(|e| format!("{e:#}"));
        let err = err.unwrap_or_else(|| panic!("symlink zip must be rejected"));
        assert!(err.contains("symlink"));
    }

    #[test]
    fn tar_gz_extraction_with_exec_bits_and_root_strip() {
        let dir = tempfile::tempdir().unwrap();
        let tgz = dir.path().join("app-1.0.tar.gz");
        let f = std::fs::File::create(&tgz).unwrap();
        let enc = flate2::write::GzEncoder::new(f, flate2::Compression::fast());
        {
            let mut tb = tar::Builder::new(enc);
            let mut hdr = tar::Header::new_gnu();
            hdr.set_size(4);
            hdr.set_mode(0o755);
            hdr.set_cksum();
            tb.append_data(
                &mut hdr,
                "pkg-root/bin/tool.sh",
                std::io::Cursor::new(b"#!/sh\n"),
            )
            .unwrap();
            let mut hdr2 = tar::Header::new_gnu();
            hdr2.set_size(3);
            hdr2.set_mode(0o644);
            hdr2.set_cksum();
            tb.append_data(
                &mut hdr2,
                "pkg-root/readme.md",
                std::io::Cursor::new(b"hi\n"),
            )
            .unwrap();
            tb.into_inner().unwrap().finish().unwrap();
        }

        let staging = dir.path().join("staging");
        extract_archive(&tgz, &staging).unwrap();
        strip_single_root_dir(&staging).unwrap();
        assert!(staging.join("bin/tool.sh").exists(), "root stripped");
        assert!(!staging.join("pkg-root").exists());
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(staging.join("bin/tool.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111, "exec bits preserved");
    }

    #[test]
    fn unknown_format_rejected_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("fake.AppImage.tar.gz");
        std::fs::write(&p, b"this is not an archive at all........").unwrap();
        let err = extract_archive(&p, &dir.path().join("stage")).unwrap_err();
        assert!(format!("{err:#}").contains("unrecognized"), "{err:#}");
    }

    #[test]
    fn publish_wipes_previous_current_only_after_success() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("stage");
        std::fs::create_dir_all(staging.join("bin")).unwrap();
        std::fs::write(staging.join("bin/cli"), b"#!/sh\nnew").unwrap();

        let current = dir.path().join("installs/demo/current");
        std::fs::create_dir_all(&current).unwrap();
        std::fs::write(current.join("OLD"), b"old junk").unwrap();

        let install = InstallCfg {
            kind: crate::config::InstallType::Archive,
            target: None,
            bin_glob: vec!["bin/*".into()],
            link_bins: vec![],
            desktop: None,
            ..Default::default()
        };
        let pubn = publish_from_staging(&staging, &install, &current).unwrap();
        assert_eq!(pubn.published, vec!["bin/cli"]);
        assert!(!current.join("OLD").exists());
        assert!(current.join("bin/cli").exists());
        // No staging litter left behind.
        let siblings: Vec<_> = std::fs::read_dir(current.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".current-staging"))
            .collect();
        assert!(siblings.is_empty(), "{siblings:?}");
    }

    #[test]
    fn symlink_conflict_refuses_foreign_entry() {
        let dir = tempfile::tempdir().unwrap();
        let installs = dir.path().join("installs");
        let current = installs.join("demo/current");
        std::fs::create_dir_all(current.join("bin")).unwrap();
        std::fs::write(current.join("bin/mytool"), b"#!/sh\n").unwrap();

        let bindir = dir.path().join("local-bin");
        std::fs::create_dir_all(&bindir).unwrap();
        // Foreign occupant:
        std::fs::write(bindir.join("mytool"), b"user's own precious script").unwrap();

        let install = InstallCfg {
            kind: crate::config::InstallType::Archive,
            target: None,
            bin_glob: vec!["**".into()],
            link_bins: vec!["mytool".into()],
            desktop: None,
            ..Default::default()
        };
        // Build the staging dir for this test:
        let staging = dir.path().join("stage_prepared");
        std::fs::create_dir_all(staging.join("bin")).unwrap();
        std::fs::write(staging.join("bin/mytool"), b"#!/sh\nnew").unwrap();
        let pubn = publish_from_staging(&staging, &install, &current).unwrap();
        let pubn = link_bins(
            pubn,
            &current,
            "demo",
            &installs,
            &["mytool".into()],
            &bindir,
        )
        .unwrap();

        assert_eq!(pubn.conflicts.len(), 1, "{:?}", pubn.conflicts);
        assert!(pubn.conflicts[0].starts_with("mytool"));
        assert_eq!(
            std::fs::read(bindir.join("mytool")).unwrap(),
            b"user's own precious script",
            "foreign file untouched"
        );
        assert!(pubn.linked.is_empty());
    }

    #[test]
    fn own_symlink_is_replaced_and_relative() {
        let dir = tempfile::tempdir().unwrap();
        let installs = dir.path().join("installs");
        let current = installs.join("demo/current");
        std::fs::create_dir_all(current.join("bin")).unwrap();
        std::fs::write(current.join("bin/mytool"), b"v2").unwrap();

        let bindir = dir.path().join("local-bin");
        std::fs::create_dir_all(&bindir).unwrap();

        // First install links it.
        let staging = dir.path().join("stage");
        std::fs::create_dir_all(staging.join("bin")).unwrap();
        std::fs::write(staging.join("bin/mytool"), b"v2").unwrap();
        let install = InstallCfg {
            kind: crate::config::InstallType::Archive,
            target: None,
            bin_glob: vec!["**".into()],
            link_bins: vec!["mytool".into()],
            desktop: None,
            ..Default::default()
        };
        let pubn = publish_from_staging(&staging, &install, &current).unwrap();
        let pubn = link_bins(
            pubn,
            &current,
            "demo",
            &installs,
            &["mytool".into()],
            &bindir,
        )
        .unwrap();
        assert!(pubn.conflicts.is_empty(), "{:?}", pubn.conflicts);
        let link_target = std::fs::read_link(bindir.join("mytool")).unwrap();
        assert!(
            link_target.is_relative(),
            "must be relative: {link_target:?}"
        );
        assert_eq!(std::fs::read(bindir.join("mytool")).unwrap(), b"v2");

        // Re-install over OUR OWN symlink succeeds.
        std::fs::write(staging.join("bin/mytool"), b"v3").unwrap();
        let pubn = publish_from_staging(&staging, &install, &current).unwrap();
        let pubn = link_bins(
            pubn,
            &current,
            "demo",
            &installs,
            &["mytool".into()],
            &bindir,
        )
        .unwrap();
        assert!(pubn.conflicts.is_empty(), "{:?}", pubn.conflicts);
        assert_eq!(std::fs::read(bindir.join("mytool")).unwrap(), b"v3");
    }

    #[test]
    fn relative_path_helper() {
        let base = Path::new("/a/b/c");
        assert_eq!(
            relative_to(base, Path::new("/a/b/c/d/e")),
            PathBuf::from("d/e")
        );
        assert_eq!(
            relative_to(base, Path::new("/a/x/y")),
            PathBuf::from("../../x/y")
        );
    }

    #[test]
    fn appimage_install_atomic_and_executable() {
        let dir = tempfile::tempdir().unwrap();
        let dl = dir.path().join("dl");
        std::fs::create_dir_all(&dl).unwrap();
        let src = dl.join("src.AppImage");
        std::fs::write(&src, b"#!/bin/sh\necho app\n").unwrap();

        let target = dir.path().join("Applications");
        let dest = install_appimage(&src, "FreeCAD-1.0-x86_64.AppImage", &target).unwrap();
        assert_eq!(dest.file_name().unwrap(), "FreeCAD-1.0-x86_64.AppImage");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777,
            0o755
        );
        // No tmp leftovers.
        assert_eq!(std::fs::read_dir(&target).unwrap().count(), 1);
    }
}

#[cfg(test)]
mod plugin_tests {
    use super::*;

    fn test_global(dir: &Path) -> GlobalConfig {
        let mut g = GlobalConfig::load(Path::new("/nonexistent/config.toml")).unwrap();
        g.home = HomePaths::resolve(Some(dir)).unwrap();
        g
    }

    /// Write a stub installer plugin that answers `describe` and rejects everything else.
    fn write_plugin(dir: &Path, name: &str, describe_json: &str) -> PathBuf {
        let p = dir.join(name);
        let body = format!(
            "#!/bin/sh\ncase \"$1\" in\n  describe) printf '%s\\n' '{}' ;;\n  *) echo \"unexpected $1\" >&2; exit 9 ;;\nesac\n",
            describe_json
        );
        std::fs::write(&p, body).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn empty_table() -> toml::Value {
        toml::Value::Table(toml::map::Map::new())
    }

    #[test]
    fn registry_lookup_hit_and_miss_lists_keys() {
        let dir = tempfile::tempdir().unwrap();
        let g = test_global(dir.path());
        let reg = registry(&g);
        for key in ["appimage", "archive", "dir"] {
            assert_eq!(lookup(&reg, key).unwrap().key(), key);
        }
        let err = match lookup(&reg, "nope") {
            Err(e) => e.to_string(),
            Ok(_) => panic!("'nope' must not resolve"),
        };
        assert!(err.contains("unknown installer 'nope'"), "{err}");
        assert!(err.contains("appimage") && err.contains("archive") && err.contains("dir"), "{err}");
    }

    #[test]
    fn discovery_finds_installers_d_executable_only() {
        let dir = tempfile::tempdir().unwrap();
        let g = test_global(dir.path());
        let idir = g.home.installers_d();
        std::fs::create_dir_all(&idir).unwrap();
        write_plugin(&idir, "mytool", r#"{"protocol":2,"id":"mytool","verbs":["install"]}"#);
        std::fs::write(idir.join("notexec.txt"), "plain data").unwrap();
        // A directory must never register even if named plausibly.
        std::fs::create_dir_all(idir.join("subdir-plugin")).unwrap();
        let found = discover_external(&g);
        assert!(
            found.iter().any(|e| e.basename == "mytool"),
            "executable plugin discovered: {found:?}"
        );
        assert!(
            !found.iter().any(|e| e.basename == "notexec.txt"),
            "non-executable ignored: {found:?}"
        );
        assert!(
            !found.iter().any(|e| e.basename == "subdir-plugin"),
            "directories ignored: {found:?}"
        );
    }

    #[tokio::test]
    async fn dir_installer_bails_without_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let g = test_global(dir.path());
        let cfg = InstallCfg {
            kind: crate::config::InstallType::Dir,
            ..Default::default()
        };
        let extra = empty_table();
        let ctx = InstallCtx {
            app_id: "demo",
            version: "1.0.0",
            cfg: &cfg,
            extra: &extra,
            download: None,
            filename: None,
            checkout: None,
            artifacts: &[],
            home: &g.home,
        };
        let err = DirInstaller.install(&ctx).await.unwrap_err().to_string();
        assert!(err.contains("requires a source-build checkout"), "{err}");
    }

    #[tokio::test]
    async fn external_handshake_rejects_protocol_1() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_plugin(dir.path(), "stub", r#"{"protocol":1,"id":"stub"}"#);
        let ext = ExternalInstaller {
            basename: "stub".into(),
            path,
        };
        let cfg = InstallCfg {
            kind: crate::config::InstallType::External("stub".into()),
            ..Default::default()
        };
        let extra = empty_table();
        let mut g = GlobalConfig::load(Path::new("/nonexistent/config.toml")).unwrap();
        g.home = HomePaths::resolve(Some(dir.path())).unwrap();
        let ctx = InstallCtx {
            app_id: "demo",
            version: "1.0.0",
            cfg: &cfg,
            extra: &extra,
            download: None,
            filename: None,
            checkout: None,
            artifacts: &[],
            home: &g.home,
        };
        let err = ext.install(&ctx).await.unwrap_err().to_string();
        assert!(err.contains("unsupported protocol 1"), "{err}");
    }

    #[tokio::test]
    async fn external_handshake_rejects_missing_install_verb() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_plugin(
            dir.path(),
            "stub",
            r#"{"protocol":2,"id":"stub","verbs":["check"]}"#,
        );
        let ext = ExternalInstaller {
            basename: "stub".into(),
            path,
        };
        let cfg = InstallCfg {
            kind: crate::config::InstallType::External("stub".into()),
            ..Default::default()
        };
        let extra = empty_table();
        let mut g = GlobalConfig::load(Path::new("/nonexistent/config.toml")).unwrap();
        g.home = HomePaths::resolve(Some(dir.path())).unwrap();
        let ctx = InstallCtx {
            app_id: "demo",
            version: "1.0.0",
            cfg: &cfg,
            extra: &extra,
            download: None,
            filename: None,
            checkout: None,
            artifacts: &[],
            home: &g.home,
        };
        let err = ext.install(&ctx).await.unwrap_err().to_string();
        assert!(err.contains("'install' verb"), "{err}");
    }
}
