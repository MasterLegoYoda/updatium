//! Interactive GitHub release onboarding. No artifacts are downloaded or installed.

use anyhow::{Context, Result, ensure};
use crate::config::{AppFile, GlobalConfig, HomePaths};
use crate::provider::{Provider, github_releases::GithubReleases};
use std::io::{self, Write};
use std::path::Path;

fn prompt(message: &str) -> Result<String> {
    print!("{message}");
    io::stdout().flush()?;
    let mut line = String::new();
    ensure!(io::stdin().read_line(&mut line)? != 0, "input ended; no configuration was written");
    Ok(line.trim().to_owned())
}

fn repository(input: &str) -> Result<String> {
    let url = url::Url::parse(input).context("expected a GitHub repository URL")?;
    ensure!(url.scheme() == "https" && url.host_str() == Some("github.com") && url.port().is_none() && url.username().is_empty() && url.password().is_none() && url.query().is_none() && url.fragment().is_none(), "expected an HTTPS github.com repository URL without credentials, query, or fragment");
    let parts: Vec<_> = url.path().trim_matches('/').split('/').collect();
    ensure!(parts.len() == 2 && parts.iter().all(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))), "expected https://github.com/owner/repository");
    let name = parts[1].strip_suffix(".git").unwrap_or(parts[1]);
    ensure!(!name.is_empty() && name != "." && name != ".." && parts[0] != "." && parts[0] != "..", "invalid repository URL");
    Ok(format!("{}/{name}", parts[0]))
}

/// Configure an AppImage from a GitHub repository; the exact filename is pinned.
pub(crate) async fn github(home: Option<&Path>, id: &str, url: &str) -> Result<()> {
    ensure!(!id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'), "app id must be [a-zA-Z0-9_-]");
    let recipe = url.strip_prefix("recipe:").map(|name| crate::recipes::find(name).with_context(|| format!("unknown recipe {name:?}"))).transpose()?;
    if recipe.is_some() { ensure!(std::env::consts::OS == "linux", "recipes support Linux only"); }
    let repo = match recipe { Some(r) => r.repo.to_owned(), None => repository(url)? };
    let paths = HomePaths::resolve(home)?;
    let dir = paths.apps_d();
    let path = dir.join(format!("{id}.toml"));
    for ext in ["toml", "yaml", "yml", "json"] {
        ensure!(!dir.join(format!("{id}.{ext}")).exists(), "configuration already exists for {id}");
    }
    let global = GlobalConfig::load(&paths.config_file())?;
    let mut config = if let Some(recipe) = recipe {
        let target = recipe.target(std::env::consts::ARCH).context("recipe does not support this architecture")?;
        serde_json::json!({"id": id, "provider": "github-releases", "repo": repo, "tag_regex": r"^v?[0-9]+\.[0-9]+\.[0-9]+$", "asset_regex": target.asset_regex, "install": {"type":"archive", "bin_glob":[recipe.executable], "link_bins":[recipe.executable]}})
    } else {
        serde_json::json!({"id": id, "provider": "github-releases", "repo": repo, "install": {"type": "appimage"}})
    };
    let mut preview = config.clone();
    preview.as_object_mut().unwrap().remove("asset_regex");
    let app = AppFile::parse_value(preview, "onboarding.toml")?;
    let http = crate::net::build_client_with_proxy(global.proxy.as_deref())?;
    let mut source = app.app_source();
    source.proxy = global.proxy.clone();
    let provider = GithubReleases::new(global.github.token_cmd.clone());
    let mut releases = provider.check(&source, &http).await.context("discovering releases")?;
    releases.sort_by(|a,b| crate::version::cmp_version(&b.version, &a.version));
    ensure!(!releases.is_empty(), "no stable releases found");
    println!("Stable releases (newest first):");
    for release in releases.iter().take(5) {
        println!("  {}", release.version);
        for asset in &release.assets { println!("    {} ({})", asset.name, asset.size.map(|s| format!("{s} bytes")).unwrap_or_else(|| "size unknown".into())); }
    }
    let latest = &releases[0];
    ensure!(!latest.assets.is_empty(), "latest release {} has no assets", latest.version);
    println!("\nSelect an artifact from latest release {}:", latest.version);
    for (i, asset) in latest.assets.iter().enumerate() {
        let name = asset.name.to_ascii_lowercase();
        let arch = std::env::consts::ARCH;
        let suggested = if let Some(recipe) = recipe {
            regex::Regex::new(recipe.target(arch).unwrap().asset_regex)?.is_match(&asset.name)
        } else {
            name.ends_with(".appimage") && (name.contains(arch) || (arch == "x86_64" && name.contains("amd64")) || (arch == "aarch64" && name.contains("arm64")))
        };
        println!("  {}. {}{}", i + 1, asset.name, if suggested { " (matches host architecture; verify platform)" } else { "" });
    }
    let choice: usize = prompt("Artifact number: ")?.parse().context("enter an artifact number")?;
    let asset = latest.assets.get(choice.checked_sub(1).context("artifact number starts at 1")?).context("invalid artifact number")?;
    if let Some(recipe) = recipe {
        let filter = regex::Regex::new(recipe.target(std::env::consts::ARCH).unwrap().asset_regex)?;
        let matches: Vec<_> = latest.assets.iter().filter(|a| filter.is_match(&a.name)).collect();
        ensure!(matches.len() == 1 && matches[0].name == asset.name, "selected artifact must be the recipe's unique matching asset");
    } else {
        ensure!(asset.name.to_ascii_lowercase().ends_with(".appimage"), "only AppImage installation is supported without a recipe");
    }
    ensure!(asset.url.scheme() == "https", "artifact URL must use HTTPS");
    ensure!(std::path::Path::new(&asset.name).file_name().and_then(|s| s.to_str()) == Some(asset.name.as_str()) && !asset.name.contains('\0'), "artifact name is not a safe filename");
    let destination = if let Some(recipe) = recipe {
        paths.install_current(id).join(recipe.executable)
    } else {
        config["asset_regex"] = format!("^{}$", regex::escape(&asset.name)).into();
        paths.default_appimage_target().join(&asset.name)
    };
    println!("Selected: {}\nURL: {}\nDestination: {}", asset.name, asset.url, destination.display());
    if recipe.is_none() {
        println!("The exact filename is pinned. If a later release changes it, selection will fail until the configuration is updated; it will not silently select another artifact.");
    }
    let probe = prompt("Existing executable path to adopt (blank for none; runs PATH --version): ")?;
    if !probe.is_empty() {
        let argv = vec![probe, "--version".to_string()];
        println!("Running installed-version probe: {:?}", argv);
        let output = crate::exec::run_probe(&argv).await.context("installed-version probe failed; no configuration was written")?;
        let version = output.lines().map(str::trim).find(|l| !l.is_empty()).context("probe returned no version")?;
        println!("Observed version: {version:?}; latest release: {:?}", latest.version);
        ensure!(crate::version::same_version_class(version, &latest.version), "probe output is incompatible with the release version format; configure a suitable probe manually");
        ensure!(prompt("Use this exact probe output as the installed version? [y/N] ")?.eq_ignore_ascii_case("y"), "cancelled; no configuration was written");
        config["installed_version_cmd"] = serde_json::json!(argv);
    }
    let app = AppFile::parse_value(config.clone(), "onboarding.toml")?;
    let validation = crate::onboarding_validation::validate(&app, &paths, &provider, &http).await?;
    ensure!(validation.selected_release.version == latest.version && validation.matching_assets.len() == 1 && validation.selected_asset.as_ref().map(|a| &a.name) == Some(&asset.name), "release or artifact selection changed during validation; restart onboarding");
    for warning in &validation.warnings { println!("Warning: {warning}"); }
    println!("Configuration validated against release {}. Build, apply, and installation were not executed.", validation.selected_release.version);
    ensure!(prompt("Write configuration? No installation will run. [y/N] ")?.eq_ignore_ascii_case("y"), "cancelled; no configuration was written");
    let body = crate::config::ConfigFormat::Toml.serialize(&config)?;
    std::fs::create_dir_all(&dir)?;
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&path).with_context(|| format!("creating {}", path.display()))?;
    file.write_all(body.as_bytes())?;
    println!("Wrote {}. No build, apply, download, or installation was executed.", path.display());
    Ok(())
}

pub(crate) async fn validate_config(home: Option<&Path>, id: &str) -> Result<()> {
    let paths = HomePaths::resolve(home)?;
    let mut global = GlobalConfig::load(&paths.config_file())?;
    global.home = paths.clone();
    let app = crate::config::load_apps(&paths.apps_d())?.into_iter().find(|app| app.id == id).with_context(|| format!("unknown app {id:?}"))?;
    let providers = crate::provider::registry(&global);
    let provider = crate::provider::lookup(&providers, &app.provider)?;
    let http = crate::net::build_client_with_proxy(app.proxy.as_deref().or(global.proxy.as_deref()))?;
    println!("Validating trusted configuration: provider checks and the installed-version probe may execute commands; build, apply and installation commands will not run.");
    let report = crate::onboarding_validation::validate(&app, &paths, provider.as_ref(), &http).await?;
    println!("Release: {}", report.selected_release.version);
    if let Some(asset) = &report.selected_asset { println!("Artifact: {} ({})", asset.name, asset.url); }
    if let Some(dest) = &report.destination { println!("Destination: {}", dest.display()); }
    if let Some(version) = &report.installed_version { println!("Observed installed version: {version}"); }
    for warning in &report.warnings { println!("Warning: {warning}"); }
    println!("Configuration validation complete; no build, apply or installation was executed. Actual installation behavior is not verified.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::repository;

    #[test]
    fn accepts_only_repository_urls() {
        assert_eq!(repository("https://github.com/owner/project.git").unwrap(), "owner/project");
        assert!(repository("https://github.com/owner/project/releases").is_err());
        assert!(repository("https://github.com.evil.example/owner/project").is_err());
        assert!(repository("https://github.com/owner/project?x=1").is_err());
        assert!(repository("http://github.com/owner/project").is_err());
    }
}
