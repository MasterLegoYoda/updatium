use std::path::Path;
use std::time::{Duration, Instant};
use updatium::config::HomePaths;
use updatium::engine::{Engine, InstalledSource, Status};
use updatium::store::Store;

fn app(home: &Path, id: &str, probe: &str) {
    let dir = home.join("config/apps.d");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{id}.toml")), format!(
        "id = '{id}'\nprovider = 'command'\ncheck_cmd = ['sh', '-c', 'echo 2.0.0']\ninstalled_version_cmd = ['sh', '-c', {}]\n[install]\ntype = 'none'\n",
        serde_json::to_string(probe).unwrap()
    )).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn failed_probes_are_bounded_and_do_not_hide_other_apps() {
    let home = tempfile::tempdir().unwrap();
    let pid_file = home.path().join("probe.pid");
    app(home.path(), "hang", &format!("echo $$ > '{}'; exec sleep 30", pid_file.display()));
    app(home.path(), "stdout", "exec yes x");
    app(home.path(), "stderr", "exec sh -c 'yes x >&2'");
    app(home.path(), "failed", "echo broken >&2; exit 7");
    app(home.path(), "good", "printf '\\n 1.0.0 \\nignored\\n'");
    let paths = HomePaths::resolve(Some(home.path())).unwrap();
    let store = Store::open(&paths.db_path()).unwrap();
    store.sync_apps(&[("hang", "command", true)]).unwrap();
    store.set_installed("hang", "9.0.0").unwrap();
    let engine = Engine::new(Some(home.path())).unwrap();
    let start = Instant::now();
    let (reports, heartbeat) = tokio::join!(engine.check(None), async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        start.elapsed()
    });
    assert!(heartbeat < Duration::from_secs(2), "runtime stalled for {heartbeat:?}");
    let reports = reports.unwrap();
    let get = |id: &str| reports.iter().find(|r| r.app == id).unwrap();
    let good = get("good");
    assert_eq!(good.installed.as_deref(), Some("1.0.0"));
    assert!(matches!(good.status, Status::UpdateAvailable));
    for (id, expected) in [("hang", "timed out"), ("stdout", "stdout"), ("stderr", "stderr"), ("failed", "broken")] {
        let report = get(id);
        assert_eq!(report.installed, None, "{id}");
        assert_eq!(report.installed_source, InstalledSource::Unknown, "{id}");
        match &report.status {
            Status::Error(message) => assert!(message.contains(expected), "{id}: {message}"),
            status => panic!("{id}: {status:?}"),
        }
    }
    let pid = std::fs::read_to_string(pid_file).unwrap();
    assert!(!Path::new("/proc").join(pid.trim()).exists(), "timed-out direct child was not reaped");
}

#[test]
fn cli_reports_probe_failure_alongside_other_apps() {
    let home = tempfile::tempdir().unwrap();
    app(home.path(), "bad", "exec yes x");
    app(home.path(), "good", "echo 1.0.0");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_updatium"))
        .arg("--home").arg(home.path()).args(["--json", "check", "--all"])
        .output().unwrap();
    assert_eq!(output.status.code(), Some(2), "{}", String::from_utf8_lossy(&output.stderr));
    let reports: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let bad = reports.as_array().unwrap().iter().find(|r| r["app"] == "bad").unwrap();
    assert!(bad["installed"].is_null());
    assert_eq!(bad["installed_source"], "unknown");
    assert!(bad["error"].as_str().unwrap().contains("stdout"));
    let good = reports.as_array().unwrap().iter().find(|r| r["app"] == "good").unwrap();
    assert_eq!(good["installed"], "1.0.0");
    assert_eq!(good["status"], "update_available");
}
