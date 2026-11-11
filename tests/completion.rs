mod common;

use std::path::Path;
use updatium::engine::{Engine, UpdateOpts};
use updatium::store::Store;

fn write_app(home: &Path, id: &str, body: &str) {
    let apps = home.join("config/apps.d");
    std::fs::create_dir_all(&apps).unwrap();
    std::fs::write(apps.join(format!("{id}.toml")), body).unwrap();
}

fn cli_update(home: &Path, id: &str) -> (i32, serde_json::Value) {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_updatium"))
        .arg("--home").arg(home).arg("--json").args(["update", id])
        .output().unwrap();
    let result = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&output.stderr)));
    (output.status.code().unwrap(), result)
}

fn assert_partial(home: &Path, id: &str, report: &serde_json::Value) {
    assert_eq!(report[0]["installed"], true);
    assert!(report[0]["failed"].is_null());
    assert!(!report[0]["integration_failures"].as_array().unwrap().is_empty());
    let engine = Engine::new(Some(home)).unwrap();
    let events = engine.history(Some(id), 100).unwrap();
    assert!(events.iter().any(|event| event.kind == "install_ok"));
    assert!(events.iter().any(|event| event.kind == "integration_failed"));
    assert!(!events.iter().any(|event| event.kind == "install_failed"));
}

#[tokio::test]
async fn desktop_failure_is_partial_after_appimage_installation() {
    let (base, _server) = common::spawn().await;
    let home = tempfile::tempdir().unwrap();
    write_app(home.path(), "demo", &format!(r#"
id = "demo"
provider = "github-releases"
allow_insecure_http = true
repo = "demo/app"
asset_regex = '\.AppImage$'
digest_policy = "required"
api_base = "{base}/gh"
[install]
type = "appimage"
[install.desktop]
name = "Demo"
"#));
    std::fs::create_dir_all(home.path().join("state")).unwrap();
    std::fs::write(home.path().join("state/applications"), b"blocks directory").unwrap();
    let (exit, report) = cli_update(home.path(), "demo");
    assert_eq!(exit, 2, "{report}");
    assert!(report[0]["integration_failures"].to_string().contains("desktop"));
    assert!(home.path().join("state/Applications/freecad-demo-2.1.0-x86_64.AppImage").is_file());
    assert_partial(home.path(), "demo", &report);
    assert_eq!(Engine::new(Some(home.path())).unwrap().installed_version("demo").unwrap().as_deref(), Some("2.1.0"));
}

#[tokio::test]
async fn foreign_link_is_preserved_and_reported_as_partial() {
    let repo = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git").args(args).current_dir(repo.path())
            .env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t")
            .status().unwrap();
        assert!(status.success());
    };
    git(&["init", "-q"]);
    std::fs::write(repo.path().join("README.md"), b"installed tool").unwrap();
    git(&["add", "."]); git(&["commit", "-qm", "one"]); git(&["tag", "b1000"]);
    let home = tempfile::tempdir().unwrap();
    write_app(home.path(), "demo", &format!(r#"
id = "demo"
provider = "source-build"
repo_url = "{}"
ref_regex = '^refs/tags/b\d+$'
[build]
steps = [["cp", "README.md", "tool"]]
tracked_files = ["README.md"]
artifacts = ["tool"]
[install]
type = "dir"
link_bins = ["tool"]
"#, repo.path().display()));
    let foreign = home.path().join("state/bin/tool");
    std::fs::create_dir_all(foreign.parent().unwrap()).unwrap();
    std::fs::write(&foreign, b"foreign").unwrap();
    let (exit, report) = cli_update(home.path(), "demo");
    assert_eq!(exit, 2, "{report}");
    assert!(report[0]["integration_failures"].to_string().contains("foreign"));
    assert_eq!(std::fs::read(&foreign).unwrap(), b"foreign");
    assert_eq!(std::fs::read(home.path().join("state/installs/demo/current/tool")).unwrap(), b"installed tool");
    assert_partial(home.path(), "demo", &report);
    assert_eq!(Engine::new(Some(home.path())).unwrap().installed_version("demo").unwrap().as_deref(), Some("b1000"));
}

#[tokio::test]
async fn interrupted_update_requires_explicit_retry_and_preserves_pending_on_failed_retry() {
    let home = tempfile::tempdir().unwrap();
    let marker = home.path().join("applied");
    let configure = |command: &str| write_app(home.path(), "demo", &format!(r#"
id = "demo"
provider = "command"
check_cmd = ["sh", "-c", "echo 2.0.0"]
apply_cmd = ["sh", "-c", '{command}']
[install]
type = "none"
"#));
    configure("kill -KILL $PPID");
    let killed = std::process::Command::new(env!("CARGO_BIN_EXE_updatium"))
        .arg("--home").arg(home.path()).args(["update", "demo"])
        .output().unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(killed.status.signal(), Some(9));
    configure("exit 1");
    let engine = Engine::new(Some(home.path())).unwrap();
    let store = Store::open(&home.path().join("state/state.db")).unwrap();
    let check = engine.check(None).await.unwrap();
    assert!(matches!(&check[0].status, updatium::engine::Status::Error(message) if message.contains("incomplete")));
    assert_eq!(engine.pending_update("demo").unwrap().as_deref(), Some("2.0.0"));
    let (exit, report) = cli_update(home.path(), "demo");
    assert_eq!(exit, 2);
    assert!(report[0]["failed"].as_str().unwrap().contains("interrupted"));
    let list = std::process::Command::new(env!("CARGO_BIN_EXE_updatium"))
        .arg("--home").arg(home.path()).args(["--json", "list"]).output().unwrap();
    let list: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(list[0]["update_pending"], "2.0.0");
    let blocked = engine.update("demo", &UpdateOpts::default()).await.unwrap();
    assert!(blocked.failed.unwrap().contains("interrupted"));
    let force = UpdateOpts { force: true, ..Default::default() };
    assert!(engine.update("demo", &force).await.unwrap().failed.is_some());
    assert!(engine.update("demo", &UpdateOpts::default()).await.unwrap().failed.unwrap().contains("interrupted"));
    configure(&format!("echo applied > {}", marker.display()));
    let done = engine.update("demo", &force).await.unwrap();
    assert!(done.installed && done.failed.is_none(), "{done:?}");
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "applied\n");
    assert!(store.pending_update("demo").unwrap().is_none());
    assert_eq!(engine.installed_version("demo").unwrap().as_deref(), Some("2.0.0"));
}

#[tokio::test]
async fn state_commit_failure_keeps_completed_installation_pending() {
    let home = tempfile::tempdir().unwrap();
    let marker = home.path().join("applied");
    write_app(home.path(), "demo", &format!(r#"
id = "demo"
provider = "command"
check_cmd = ["sh", "-c", "echo 2.0.0"]
apply_cmd = ["sh", "-c", "echo installed > {}"]
[install]
type = "none"
"#, marker.display()));
    let engine = Engine::new(Some(home.path())).unwrap();
    let db = rusqlite::Connection::open(home.path().join("state/state.db")).unwrap();
    db.execute_batch("CREATE TRIGGER reject_install BEFORE INSERT ON app_state BEGIN SELECT RAISE(FAIL, 'disk failure'); END;").unwrap();
    let report = engine.update("demo", &UpdateOpts::default()).await.unwrap();
    assert!(report.installed && !report.recorded, "{report:?}");
    assert!(report.integration_failures.iter().any(|x| x.contains("recording installed version")));
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "installed\n");
    assert_eq!(engine.pending_update("demo").unwrap().as_deref(), Some("2.0.0"));
    assert!(!engine.history(Some("demo"), 100).unwrap().iter().any(|e| e.kind == "install_ok"));
    db.execute_batch("DROP TRIGGER reject_install;").unwrap();
    let report = engine.update("demo", &UpdateOpts { force: true, ..Default::default() }).await.unwrap();
    assert!(report.installed && report.recorded, "{report:?}");
    assert!(engine.pending_update("demo").unwrap().is_none());
}
