use super::*;
use async_trait::async_trait;
use crate::config::AppSource;

struct TestProvider(Vec<Release>);

#[async_trait]
impl Provider for TestProvider {
    fn id(&self) -> &'static str { "test" }
    async fn check(&self, _: &AppSource, _: &reqwest::Client) -> Result<Vec<Release>> {
        Ok(self.0.clone())
    }
}

fn app(extra: &str) -> AppFile {
    AppFile::parse(&format!("id = 'demo'\nprovider = 'github-releases'\nrepo = 'owner/repo'\n{extra}"), "demo.toml").unwrap()
}

fn release(version: &str, names: &[&str]) -> Release {
    Release {
        version: version.into(), published_at: None,
        assets: names.iter().map(|name| Asset {
            name: (*name).into(), url: url::Url::parse(&format!("https://example.com/{name}")).unwrap(),
            size: None, digest: None,
        }).collect(),
    }
}

#[tokio::test]
async fn previews_latest_matching_asset_and_probe_without_installing() {
    let temp = tempfile::tempdir().unwrap();
    let home = HomePaths::resolve(Some(temp.path())).unwrap();
    let app = app("asset_regex = 'linux'\ninstalled_version_cmd = ['sh', '-c', 'printf \"\\n1.0.0\\n\"']\n[install]\ntype = 'appimage'\n");
    let provider = TestProvider(vec![release("1.0.0", &["old-linux"]), release("2.0.0", &["windows", "linux-a", "linux-b"])]);
    let report = validate(&app, &home, &provider, &reqwest::Client::new()).await.unwrap();
    assert_eq!(report.selected_release.version, "2.0.0");
    assert_eq!(report.selected_asset.unwrap().name, "linux-a");
    assert_eq!(report.matching_assets.len(), 2);
    assert_eq!(report.installed_version.as_deref(), Some("1.0.0"));
    assert_eq!(report.destination, Some(home.default_appimage_target()));
    assert_eq!(report.warnings.len(), 1);
    assert!(!home.state_dir.exists());
}

#[tokio::test]
async fn rejects_missing_releases_assets_and_bad_probe() {
    let temp = tempfile::tempdir().unwrap();
    let home = HomePaths::resolve(Some(temp.path())).unwrap();
    let client = reqwest::Client::new();
    let configured = app("asset_regex = 'linux'\n[install]\ntype = 'appimage'\n");
    assert!(validate(&configured, &home, &TestProvider(vec![]), &client).await.unwrap_err().to_string().contains("no releases"));
    assert!(validate(&configured, &home, &TestProvider(vec![release("1.0.0", &["windows"])]), &client).await.unwrap_err().to_string().contains("no asset"));
    let bad_probe = app("installed_version_cmd = ['sh', '-c', 'exit 1']\n[install]\ntype = 'appimage'\n");
    assert!(validate(&bad_probe, &home, &TestProvider(vec![release("1.0.0", &["linux"])]), &client).await.unwrap_err().to_string().contains("probe failed"));
}

#[tokio::test]
async fn validates_paths_and_never_runs_apply_or_build() {
    let temp = tempfile::tempdir().unwrap();
    let home = HomePaths::resolve(Some(temp.path())).unwrap();
    let client = reqwest::Client::new();
    let invalid = app("[install]\ntype = 'archive'\n");
    assert!(validate(&invalid, &home, &TestProvider(vec![]), &client).await.unwrap_err().to_string().contains("bin_glob"));
    let marker = temp.path().join("applied");
    let config = format!("id = 'demo'\nprovider = 'command'\ncheck_cmd = ['true']\napply_cmd = ['touch', '{}']\n[install]\ntype = 'none'\n", marker.display());
    let command = AppFile::parse(&config, "demo.toml").unwrap();
    let report = validate(&command, &home, &TestProvider(vec![release("1.0.0", &[])]), &client).await.unwrap();
    assert!(report.destination.is_none());
    assert!(!marker.exists());
}

#[tokio::test]
async fn rejects_empty_probe_and_does_not_execute_build_steps() {
    let temp = tempfile::tempdir().unwrap();
    let home = HomePaths::resolve(Some(temp.path())).unwrap();
    let client = reqwest::Client::new();
    let marker = temp.path().join("built");
    let config = format!("id = 'demo'\nprovider = 'source-build'\nrepo_url = 'https://example.com/repo.git'\nref_regex = '.*'\ninstalled_version_cmd = ['sh', '-c', 'echo']\n[build]\nsteps = [['touch', '{}']]\ntracked_files = []\nartifacts = []\n[install]\ntype = 'dir'\n", marker.display());
    let source = AppFile::parse(&config, "demo.toml").unwrap();
    let result = validate(&source, &home, &TestProvider(vec![release("1.0.0", &[])]), &client).await;
    assert!(result.unwrap_err().to_string().contains("no nonempty stdout"));
    assert!(!marker.exists());
}
