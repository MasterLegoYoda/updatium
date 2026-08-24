//! Shared helper: spawn the fixture-server example and hand out its base URL.
#![allow(dead_code)]

use std::path::PathBuf;
use std::time::Duration;

pub struct FixtureGuard(pub tokio::process::Child);

impl Drop for FixtureGuard {
    fn drop(&mut self) {
        let _ = self.0.start_kill();
    }
}

/// Locate `<target>/<profile>/examples` next to the running test binary.
fn fixture_binary() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    // .../target/<profile>/deps/test-xxx -> .../target/<profile>/examples
    let deps = exe.parent().expect("exe parent");
    let profile_dir = deps.parent().expect("profile dir");
    profile_dir.join("examples").join("fixture-server")
}

/// Spawn `fixture-server`, wait for its `BASE_URL=...` banner.
pub async fn spawn() -> (String, FixtureGuard) {
    let bin = fixture_binary();
    // Always (re)build: no-op when fresh, guarantees the binary matches source.
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let st = std::process::Command::new(cargo)
        .args(["build", "--example", "fixture-server", "-q"])
        .status()
        .expect("spawning cargo build");
    assert!(st.success(), "building fixture-server example failed");

    let mut child = tokio::process::Command::new(&bin)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .expect("spawning fixture-server");

    let stdout = child.stdout.take().expect("piped stdout");
    let mut reader = tokio::io::BufReader::new(stdout);
    use tokio::io::AsyncBufReadExt;
    let mut line = String::new();
    let n = tokio::time::timeout(Duration::from_secs(30), reader.read_line(&mut line))
        .await
        .expect("timed out waiting for fixture server banner")
        .expect("reading fixture server stdout");
    assert!(n > 0, "fixture-server exited before printing BASE_URL");
    let base = line
        .trim()
        .strip_prefix("BASE_URL=")
        .expect("first fixture line must be BASE_URL=<url>")
        .to_string();

    drop(reader);
    (base, FixtureGuard(child))
}
