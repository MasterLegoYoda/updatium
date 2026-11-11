//! These fixtures demonstrate repeatable side effects, not an idempotence guarantee
//! for arbitrary commands or plugins. They exercise a bookkeeping failure after
//! the external action has completed, not every possible interruption point.
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use updatium::engine::{Engine, Status, UpdateOpts};

fn write_app(home: &Path, body: &str) {
    let dir = home.join("config/apps.d");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("demo.toml"), body).unwrap();
}

fn executable(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

async fn exercise_recovery(home: &Path, effects: &Path, fail: &Path) {
    let engine = Engine::new(Some(home)).unwrap();
    let db = rusqlite::Connection::open(home.join("state/state.db")).unwrap();
    // A failed action may already have changed state; ordinary retry must stop.
    std::fs::write(fail, b"fail").unwrap();
    let failed = engine.update("demo", &UpdateOpts::default()).await.unwrap();
    assert!(failed.failed.as_deref().unwrap().contains("no rollback"), "{failed:?}");
    assert_eq!(std::fs::read_to_string(effects).unwrap(), "effect\n");
    assert_eq!(engine.pending_update("demo").unwrap().as_deref(), Some("2.0.0"));
    assert!(engine.installed_version("demo").await.unwrap().is_none());
    let blocked = engine.update("demo", &UpdateOpts::default()).await.unwrap();
    assert!(blocked.failed.as_deref().unwrap().contains("--force"));
    assert_eq!(std::fs::read_to_string(effects).unwrap(), "effect\n");
    let force = UpdateOpts { force: true, ..Default::default() };
    let failed = engine.update("demo", &force).await.unwrap();
    assert!(failed.failed.as_deref().unwrap().contains("no rollback"), "{failed:?}");
    assert_eq!(std::fs::read_to_string(effects).unwrap(), "effect\neffect\n");
    assert_eq!(engine.pending_update("demo").unwrap().as_deref(), Some("2.0.0"));
    std::fs::remove_file(fail).unwrap();
    db.execute_batch("CREATE TRIGGER reject_install BEFORE INSERT ON app_state BEGIN SELECT RAISE(FAIL, 'fixture commit failure'); END;").unwrap();

    let partial = engine.update("demo", &force).await.unwrap();
    assert!(partial.installed && !partial.recorded, "{partial:?}");
    assert!(partial.integration_failures.iter().any(|s| s.contains("recording installed version")), "{partial:?}");
    assert_eq!(std::fs::read_to_string(effects).unwrap(), "effect\neffect\neffect\n");
    assert_eq!(engine.pending_update("demo").unwrap().as_deref(), Some("2.0.0"));
    assert!(engine.installed_version("demo").await.unwrap().is_none());
    assert!(matches!(&engine.check(None).await.unwrap()[0].status, Status::Error(s) if s.contains("incomplete")));

    let blocked = engine.update("demo", &UpdateOpts::default()).await.unwrap();
    assert!(blocked.failed.as_deref().unwrap().contains("interrupted"));
    assert_eq!(std::fs::read_to_string(effects).unwrap(), "effect\neffect\neffect\n");

    db.execute_batch("DROP TRIGGER reject_install;").unwrap();
    std::fs::write(fail, b"fail").unwrap();
    let failed = engine.update("demo", &force).await.unwrap();
    assert!(failed.failed.as_deref().unwrap().contains("no rollback"), "{failed:?}");
    assert_eq!(std::fs::read_to_string(effects).unwrap(), "effect\neffect\neffect\neffect\n");
    assert_eq!(engine.pending_update("demo").unwrap().as_deref(), Some("2.0.0"));
    assert!(engine.installed_version("demo").await.unwrap().is_none());

    std::fs::remove_file(fail).unwrap();
    let done = engine.update("demo", &force).await.unwrap();
    assert!(done.installed && done.recorded && done.failed.is_none(), "{done:?}");
    assert_eq!(std::fs::read_to_string(effects).unwrap(), "effect\neffect\neffect\neffect\neffect\n");
    assert!(engine.pending_update("demo").unwrap().is_none());
    assert_eq!(engine.installed_version("demo").await.unwrap().as_deref(), Some("2.0.0"));
    let events = engine.history(Some("demo"), 100).unwrap();
    assert!(events.iter().any(|e| e.kind == "install_failed"));
    assert!(events.iter().any(|e| e.kind == "install_ok"));
}

#[tokio::test]
async fn failed_apply_descendant_can_outlive_the_app_lock() {
    let home = tempfile::tempdir().unwrap();
    let late = home.path().join("late-effect");
    let gate = home.path().join("release");
    let script = home.path().join("apply");
    executable(&script, &format!(
        "#!/bin/sh\n(n=0; while [ ! -f '{}' ] && [ $n -lt 100 ]; do sleep .05; n=$((n+1)); done; [ -f '{}' ] && echo changed > '{}') </dev/null >/dev/null 2>&1 &\nexit 1\n",
        gate.display(), gate.display(), late.display()
    ));
    write_app(home.path(), &format!(r#"
id = "demo"
provider = "command"
check_cmd = ["sh", "-c", "echo 2.0.0"]
apply_cmd = ["{}"]
[install]
type = "none"
"#, script.display()));
    let engine = Engine::new(Some(home.path())).unwrap();
    let result = engine.update("demo", &UpdateOpts::default()).await.unwrap();
    assert!(result.failed.is_some());
    assert_eq!(engine.pending_update("demo").unwrap().as_deref(), Some("2.0.0"));
    assert!(!late.exists());
    std::fs::write(&gate, b"release").unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !late.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }).await.unwrap();
    assert_eq!(std::fs::read_to_string(late).unwrap(), "changed\n");
}

#[tokio::test]
async fn configured_command_can_repeat_side_effects_on_recovery() {
    let home = tempfile::tempdir().unwrap();
    let effects = home.path().join("effects");
    let fail = home.path().join("fail");
    let script = home.path().join("apply");
    executable(&script, &format!("#!/bin/sh\necho effect >> '{}'\nif test -f '{}'; then echo fixture-failure >&2; exit 1; fi\n", effects.display(), fail.display()));
    write_app(home.path(), &format!(r#"
id = "demo"
provider = "command"
check_cmd = ["sh", "-c", "echo 2.0.0"]
apply_cmd = ["{}"]
[install]
type = "none"
"#, script.display()));
    exercise_recovery(home.path(), &effects, &fail).await;
}

#[tokio::test]
async fn external_installer_can_repeat_side_effects_on_recovery() {
    use axum::{routing::get, Router};
    let app = Router::new()
        .route("/", get(|| async { r#"<a href="/tool-2.0.0.bin">download</a>"# }))
        .route("/tool-2.0.0.bin", get(|| async { "fixture payload" }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let home = tempfile::tempdir().unwrap();
    let effects = home.path().join("effects");
    let fail = home.path().join("fail");
    let plugin = home.path().join("config/installers.d/repeat");
    executable(&plugin, &format!(r##"#!/bin/sh
case "$1" in
 describe) echo '{{"protocol":2,"id":"repeat","verbs":["install"]}}' ;;
 install)
   cat >/dev/null
   echo effect >> '{}'
   if test -f '{}'; then echo fixture-failure >&2; exit 1; fi
   echo '{{"actions":["fixture installed"],"exec":null,"installed":[]}}' ;;
 *) exit 1 ;;
esac
"##, effects.display(), fail.display()));
    write_app(home.path(), &format!(r#"
id = "demo"
provider = "html-links"
url = "http://{addr}/"
link_regex = 'href="(?P<href>[^"]*tool-[^"]+\.bin)"'
version_capture = '(?P<version>\d+\.\d+\.\d+)'
allow_insecure_http = true
[install]
type = "external:repeat"
"#));
    exercise_recovery(home.path(), &effects, &fail).await;
    server.abort();
}
