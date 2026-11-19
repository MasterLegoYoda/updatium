//! Pre-install checks. Discovery may perform network requests or execute a provider's
//! check command; a configured installed-version probe is also executed. No build,
//! installer, or apply operation is run, and no installation files are written.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use crate::config::{AppFile, HomePaths, InstallType, SourceKind, expand_tilde};
use crate::provider::{Asset, Provider, Release};

/// Observed configuration preview, not a guarantee that installation will succeed.
#[derive(Debug, Clone)]
pub struct ValidationReport {
    pub releases: Vec<Release>,
    pub selected_release: Release,
    /// The first matching asset, as selected by the update engine.
    pub selected_asset: Option<Asset>,
    /// All assets matching the configured selection on the selected release.
    pub matching_assets: Vec<Asset>,
    pub destination: Option<PathBuf>,
    pub installed_version: Option<String>,
    pub warnings: Vec<String>,
}

/// Discover and validate an app without executing its build, installer, or apply
/// commands. The caller supplies the resolved provider and HTTP client, including
/// any desired network/proxy policy. Provider checks and the configured probe can
/// execute external commands and should only be used with trusted configuration.
pub async fn validate(
    app: &AppFile,
    home: &HomePaths,
    provider: &dyn Provider,
    http: &reqwest::Client,
) -> Result<ValidationReport> {
    let destination = destination(app, home)?;
    let releases = provider.check(&app.app_source(), http).await.context("release discovery failed")?;
    let first = releases.first().context("release discovery returned no releases")?;
    for release in &releases {
        if release.version.trim().is_empty() {
            bail!("release discovery returned an empty version");
        }
        crate::engine::ensure_same_version_class(&app.source_kind, &first.version, &release.version)?;
    }
    let selected_release = releases.iter().reduce(|best, candidate| {
        if crate::engine::compare_app_versions(&app.source_kind, &candidate.version, &best.version).is_gt() {
            candidate
        } else {
            best
        }
    }).expect("nonempty releases").clone();

    let wants_assets = !matches!(app.install.kind, InstallType::None)
        && matches!(app.source_kind, SourceKind::GithubReleases(_) | SourceKind::HtmlLinks(_) | SourceKind::External { .. });
    let mut matching_assets = Vec::new();
    if wants_assets {
        let regex = match &app.source_kind {
            SourceKind::GithubReleases(cfg) => cfg.asset_regex.as_deref()
                .map(regex::Regex::new).transpose().context("invalid asset_regex")?,
            _ => None,
        };
        matching_assets.extend(selected_release.assets.iter().filter(|asset| {
            regex.as_ref().map_or(true, |re| re.is_match(&asset.name))
        }).cloned());
        if matching_assets.is_empty() {
            bail!("no asset of release '{}' matches the configured selection", selected_release.version);
        }
    }
    let selected_asset = matching_assets.first().cloned();
    let mut warnings = Vec::new();
    if matching_assets.len() > 1 {
        warnings.push(format!("{} assets match; the first will be selected", matching_assets.len()));
    }
    if matches!(app.install.kind, InstallType::External(_)) {
        warnings.push("external installer destination and behavior cannot be validated without running it".into());
    }
    let installed_version = if let Some(cmd) = &app.installed_version_cmd {
        let output = crate::exec::run_probe(cmd).await.context("installed-version probe failed")?;
        let version = output.lines().map(str::trim).find(|line| !line.is_empty())
            .context("installed-version probe returned no nonempty stdout line")?.to_owned();
        crate::engine::ensure_same_version_class(&app.source_kind, &selected_release.version, &version)
            .context("installed-version probe is incompatible with discovered releases")?;
        Some(version)
    } else {
        None
    };
    Ok(ValidationReport { releases, selected_release, selected_asset, matching_assets, destination, installed_version, warnings })
}

fn destination(app: &AppFile, home: &HomePaths) -> Result<Option<PathBuf>> {
    let id = Path::new(&app.id);
    if app.id.is_empty() || id.components().count() != 1 || !matches!(id.components().next(), Some(Component::Normal(_))) {
        bail!("app id must be a single path component");
    }
    let dest = match &app.source_kind {
        SourceKind::SourceBuild(cfg) if cfg.install_dir.is_some() => {
            if cfg.build.install_steps.is_empty() {
                bail!("install_dir requires at least one build.install_steps command");
            }
            Some(expand_tilde(cfg.install_dir.as_deref().unwrap()))
        }
        _ => match &app.install.kind {
            InstallType::Appimage => Some(app.install.target.as_deref().map(expand_tilde)
                .unwrap_or_else(|| home.default_appimage_target())),
            InstallType::Archive => {
                if app.install.bin_glob.is_empty() {
                    bail!("archive install requires at least one bin_glob pattern");
                }
                Some(home.install_current(&app.id))
            }
            InstallType::Dir => Some(home.install_current(&app.id)),
            InstallType::None | InstallType::External(_) | InstallType::Deb => None,
        },
    };
    if let Some(path) = &dest {
        if path.as_os_str().is_empty() || path.components().any(|c| matches!(c, Component::ParentDir)) {
            bail!("unsafe or empty installation destination: {}", path.display());
        }
        // Check existing ancestors without creating anything. A missing destination
        // is expected before the first install.
        for ancestor in path.ancestors() {
            if ancestor.as_os_str().is_empty() { continue; }
            match std::fs::metadata(ancestor) {
                Ok(meta) if !meta.is_dir() => bail!("installation path ancestor is not a directory: {}", ancestor.display()),
                Ok(_) => break,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e).with_context(|| format!("checking installation path {}", ancestor.display())),
            }
        }
    }
    for name in &app.install.link_bins {
        let path = Path::new(name);
        if path.components().count() != 1 || !matches!(path.components().next(), Some(Component::Normal(_))) {
            bail!("link_bins entry must be a single filename: {name:?}");
        }
    }
    Ok(dest)
}

#[cfg(test)]
#[path = "onboarding_validation_tests.rs"]
mod tests;
