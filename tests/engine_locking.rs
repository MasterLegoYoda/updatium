use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};
use updatium::engine::{Engine, UpdateOpts};

fn git(dir: &Path, args: &[&str]) {
    assert!(Command::new("git").args(args).current_dir(dir)
        .env("GIT_AUTHOR_NAME", "test").env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test").env("GIT_COMMITTER_EMAIL", "test@example.com")
        .status().unwrap().success());
}

fn write_app(home: &Path, id: &str, config: &str) {
    let dir = home.join("config/apps.d");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{id}.toml")), config).unwrap();
}

struct RunningUpdate {
    child: std::process::Child,
    release: std::path::PathBuf,
}

impl Drop for RunningUpdate {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.release, "go");
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn approval_and_second_update_cannot_disturb_active_update() {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    std::fs::write(repo.join("README.md"), "tracked\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "initial"]);
    git(&repo, &["tag", "b1000"]);
    let home = root.path().join("home");
    let ready = root.path().join("ready");
    let release = root.path().join("release");
    write_app(&home, "demo", &format!(r#"
id = "demo"
provider = "source-build"
repo_url = '{}'
ref_regex = '^refs/tags/b\d+$'
[build]
steps = [["sh", "-c", "touch '{}'; while [ ! -e '{}' ]; do sleep 0.05; done; cp README.md out.txt"]]
tracked_files = ["README.md"]
artifacts = ["out.txt"]
[install]
type = "dir"
"#, repo.display(), ready.display(), release.display()));
    let child = Command::new(env!("CARGO_BIN_EXE_updatium"))
        .arg("--home").arg(&home).args(["update", "demo"]).spawn().unwrap();
    let mut child = RunningUpdate { child, release: release.clone() };
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready.exists() {
        assert!(child.child.try_wait().unwrap().is_none(), "update exited before build started");
        assert!(Instant::now() < deadline, "timed out waiting for build");
        std::thread::sleep(Duration::from_millis(20));
    }
    let engine = Engine::new(Some(&home)).unwrap();
    let err = engine.approve("demo").unwrap_err().to_string();
    assert!(err.contains("another update or approval of 'demo' is running"), "{err}");
    let rt = tokio::runtime::Runtime::new().unwrap();
    let err = rt.block_on(engine.update("demo", &UpdateOpts::default())).unwrap_err().to_string();
    assert!(err.contains("another update or approval of 'demo' is running"), "{err}");
    std::fs::write(&release, "go").unwrap();
    assert!(child.child.wait().unwrap().success());
    assert!(engine.approve("demo").is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn distinct_apps_can_verify_same_basename_concurrently() {
    use axum::{extract::Path as AxumPath, routing::get, Router};
    use sha2::{Digest, Sha256};
    let router = Router::new()
        .route("/{id}/", get(|| async { r#"<a href="asset-1.0.0.AppImage">asset</a>"# }))
        .route("/{id}/asset-1.0.0.AppImage.sha256", get(|AxumPath(id): AxumPath<String>| async move { hex::encode(Sha256::digest(id.as_bytes())) }))
        .route("/{id}/asset-1.0.0.AppImage", get(|AxumPath(id): AxumPath<String>| async move { id }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let home = tempfile::tempdir().unwrap();
    for id in ["first", "second"] {
        write_app(home.path(), id, &format!(r#"
id = "{id}"
provider = "html-links"
url = "http://{address}/{id}/"
link_regex = 'href="(?P<href>[^"]+\.AppImage)"'
version_capture = '(?P<version>\d+\.\d+\.\d+)'
digest_policy = "required"
digest_suffix = ".sha256"
allow_insecure_http = true
[install]
type = "appimage"
target = "{}/applications-{id}"
"#, home.path().display()));
    }
    let engine = Engine::new(Some(home.path())).unwrap();
    let (start_tx, start_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
    let home_path = home.path().to_owned();
    let worker = std::thread::spawn(move || {
        start_rx.recv().unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let second = Engine::new(Some(&home_path)).unwrap();
        let report = runtime.block_on(second.update("second", &UpdateOpts::default())).unwrap();
        done_tx.send(report).unwrap();
    });
    let done_rx = std::sync::Mutex::new(done_rx);
    let report = engine.update_with_progress("first", &UpdateOpts::default(), &|stage| {
        if matches!(stage, updatium::engine::UpdateProgress::Stage("Verifying")) {
            start_tx.send(()).unwrap();
            let second = done_rx.lock().unwrap().recv_timeout(Duration::from_secs(30)).unwrap();
            assert!(second.installed && second.failed.is_none(), "{second:?}");
        }
    }).await.unwrap();
    assert!(report.installed && report.failed.is_none(), "{report:?}");
    worker.join().unwrap();
    for id in ["first", "second"] {
        let path = home.path().join(format!("applications-{id}/asset-1.0.0.AppImage"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), id);
    }
    server.abort();
}
