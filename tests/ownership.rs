use std::path::Path;
use updatium::engine::Engine;
use updatium::store::Store;

fn setup(home: &Path) {
    let apps = home.join("config/apps.d");
    std::fs::create_dir_all(&apps).unwrap();
    std::fs::write(apps.join("demo.toml"), r#"
id = "demo"
provider = "html-links"
url = "https://example.com/"
link_regex = 'href="(?P<href>[^\"]+\.AppImage)"'
version_capture = '(?P<version>\d+\.\d+\.\d+)'
[install]
type = "appimage"
"#).unwrap();
}

#[test]
fn untrack_retains_installation_state_and_history() {
    let home = tempfile::tempdir().unwrap();
    setup(home.path());
    let file = home.path().join("installed");
    std::fs::write(&file, b"user data").unwrap();
    let store = Store::open(&home.path().join("state/state.db")).unwrap();
    store.sync_apps(&[("demo", "html-links", true)]).unwrap();
    store.record_install("demo", "1.0.0", Some((&file, false))).unwrap();
    let engine = Engine::new(Some(home.path())).unwrap();
    let preview = engine.preview_untrack("demo").unwrap();
    assert!(home.path().join("config/apps.d/demo.toml").exists());
    engine.apply_untrack("demo", &preview).unwrap();
    assert!(!home.path().join("config/apps.d/demo.toml").exists());
    assert!(file.exists());
    assert!(!engine.history(Some("demo"), 20).unwrap().is_empty());
    assert!(store.install_target("demo").unwrap().is_some());
}

#[test]
fn legacy_installation_and_changed_confirmation_are_preserved() {
    let home = tempfile::tempdir().unwrap();
    setup(home.path());
    let file = home.path().join("legacy");
    std::fs::write(&file, b"adopted").unwrap();
    let store = Store::open(&home.path().join("state/state.db")).unwrap();
    store.sync_apps(&[("demo", "html-links", true)]).unwrap();
    store.record_install("demo", "1.0.0", Some((&file, false))).unwrap();
    let engine = Engine::new(Some(home.path())).unwrap();
    let preview = engine.preview_uninstall("demo").unwrap();
    assert!(preview.removable.is_empty());
    engine.apply_uninstall("demo", &preview).unwrap();
    assert!(file.exists());
    assert!(store.install_target("demo").unwrap().is_some());
    let mut incorrect = engine.preview_untrack("demo").unwrap();
    incorrect.removable.clear();
    assert!(engine.apply_untrack("demo", &incorrect).is_err());
    assert!(home.path().join("config/apps.d/demo.toml").exists());
}

#[test]
fn pending_operation_blocks_removal() {
    let home = tempfile::tempdir().unwrap();
    setup(home.path());
    let store = Store::open(&home.path().join("state/state.db")).unwrap();
    store.sync_apps(&[("demo", "html-links", true)]).unwrap();
    store.begin_update("demo", "2.0.0").unwrap();
    let engine = Engine::new(Some(home.path())).unwrap();
    assert!(engine.preview_untrack("demo").is_err());
    assert!(engine.preview_uninstall("demo").is_err());
}

#[test]
fn uninstall_removes_only_unchanged_manifest_entries() {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    setup(home.path());
    let root = home.path().join("state/installs/demo/current");
    std::fs::create_dir_all(&root).unwrap();
    let managed = root.join("managed");
    let modified = root.join("modified");
    let foreign = root.join("foreign");
    std::fs::write(&managed, b"owned").unwrap();
    std::fs::write(&modified, b"changed").unwrap();
    std::fs::write(&foreign, b"unknown").unwrap();
    let store = Store::open(&home.path().join("state/state.db")).unwrap();
    store.sync_apps(&[("demo", "html-links", true)]).unwrap();
    store.record_install("demo", "1.0.0", Some((&root, true))).unwrap();
    let mode = std::fs::metadata(&managed).unwrap().permissions().mode() & 0o7777;
    let fingerprint = format!("file:{mode:o}:{}", hex::encode(Sha256::digest(b"owned")));
    let manifest = serde_json::json!([
        {"path": managed, "fingerprint": fingerprint, "installation": true},
        {"path": modified, "fingerprint": "file:644:wrong", "installation": true}
    ]);
    let db = rusqlite::Connection::open(home.path().join("state/state.db")).unwrap();
    db.execute("INSERT INTO ownership_manifests(app_id, manifest) VALUES('demo', ?1)", [manifest.to_string()]).unwrap();
    let engine = Engine::new(Some(home.path())).unwrap();
    let preview = engine.preview_uninstall("demo").unwrap();
    assert_eq!(preview.removable.len(), 1);
    engine.apply_uninstall("demo", &preview).unwrap();
    assert!(!managed.exists());
    assert!(modified.exists());
    assert!(foreign.exists());
    assert!(store.install_target("demo").unwrap().is_some());
}

#[tokio::test]
async fn installed_files_links_and_desktop_are_removed_without_unowned_files() {
    let repo = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git").args(args).current_dir(repo.path())
            .env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t")
            .status().unwrap();
        assert!(status.success());
    };
    git(&["init", "-q"]);
    std::fs::write(repo.path().join("content"), b"installed").unwrap();
    git(&["add", "."]); git(&["commit", "-qm", "one"]); git(&["tag", "b1000"]);
    let home = tempfile::tempdir().unwrap();
    let apps = home.path().join("config/apps.d");
    std::fs::create_dir_all(&apps).unwrap();
    std::fs::write(apps.join("demo.toml"), format!(r#"
id = "demo"
provider = "source-build"
repo_url = "{}"
ref_regex = '^refs/tags/b\d+$'
[build]
steps = [["cp", "content", "tool"]]
tracked_files = ["content"]
artifacts = ["tool"]
[install]
type = "dir"
link_bins = ["tool"]
[install.desktop]
name = "Demo"
"#, repo.path().display())).unwrap();
    let engine = Engine::new(Some(home.path())).unwrap();
    let report = engine.update("demo", &Default::default()).await.unwrap();
    assert!(report.installed && report.recorded && report.integration_failures.is_empty(), "{report:?}");
    let root = home.path().join("state/installs/demo/current");
    let link = home.path().join("state/bin/tool");
    let desktop = home.path().join("state/applications/updatium.demo.desktop");
    assert!(root.join("tool").exists() && link.is_symlink() && desktop.exists());
    std::fs::write(root.join("user-data"), b"keep").unwrap();
    let preview = engine.preview_uninstall("demo").unwrap();
    assert_eq!(preview.removable.len(), 3, "{preview:?}");
    engine.apply_uninstall("demo", &preview).unwrap();
    assert!(!root.join("tool").exists() && !link.is_symlink() && !desktop.exists());
    assert_eq!(std::fs::read(root.join("user-data")).unwrap(), b"keep");
    assert!(apps.join("demo.toml").exists());
    assert!(!engine.history(Some("demo"), 20).unwrap().is_empty());
    std::fs::remove_file(root.join("user-data")).unwrap();
    let preview = engine.preview_uninstall("demo").unwrap();
    engine.apply_uninstall("demo", &preview).unwrap();
    assert!(!root.exists());
    assert!(engine.installed_version("demo").await.unwrap().is_none());
}
