//! Engine orchestration tests: full appimage install, digest gate, dry-run,
//! source-build approval gate — all under a relocated UPDATIUM_HOME.
mod common;

use std::path::Path;
use updatium::engine::{Engine, Status, UpdateOpts};

fn write_app(home: &Path, id: &str, body: &str) {
    let dir = home.join("config").join("apps.d");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{id}.toml")), body).unwrap();
}

fn engine(home: &Path) -> Engine {
    Engine::new(Some(home)).unwrap()
}

const GH_APP_TMPL: &str = r#"
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
name = "Demo App"
"#;

#[tokio::test]
async fn full_appimage_lifecycle() {
    let (base, _guard) = common::spawn().await;
    let home = tempfile::tempdir().unwrap();
    write_app(home.path(), "demo", &GH_APP_TMPL.replace("{base}", &base));
    let e = engine(home.path());

    // First check: never installed -> NeverChecked with latest known.
    let reports = e.check(None).await.unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].latest.as_deref(), Some("2.1.0"));
    assert!(matches!(reports[0].status, Status::NeverChecked));

    // Dry run prints a plan and mutates nothing.
    let rep = e
        .update(
            "demo",
            &UpdateOpts {
                dry_run: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(
        rep.actions.iter().any(|a| a.contains("would download")),
        "{rep:?}"
    );
    assert!(
        !home
            .path()
            .join("state/Applications")
            .join("freecad-demo-2.1.0-x86_64.AppImage")
            .exists(),
        "dry run must not install"
    );
    assert!(e.installed_version("demo").unwrap().is_none());

    // Real update installs the appimage + desktop + state.
    let rep = e.update("demo", &UpdateOpts::default()).await.unwrap();
    assert!(rep.failed.is_none(), "{rep:?}");
    let installed_path = home
        .path()
        .join("state/Applications/freecad-demo-2.1.0-x86_64.AppImage");
    assert!(installed_path.exists(), "{:?}", home.path().join("state"));
    assert_eq!(
        e.installed_version("demo").unwrap().as_deref(),
        Some("2.1.0")
    );

    let desktop = home.path().join("state/applications/updatium.demo.desktop");
    assert!(desktop.exists());
    let content = std::fs::read_to_string(&desktop).unwrap();
    // Values are Desktop-Entry-escaped; space becomes \s.
    assert!(content.contains("Name=Demo\\sApp"), "{content}");
    assert!(
        content.starts_with(installed_path.to_str().unwrap())
            || content.contains(&installed_path.display().to_string())
    );

    // Re-update without force: nothing to do.
    let rep = e.update("demo", &UpdateOpts::default()).await.unwrap();
    assert!(rep.actions.iter().any(|a| a == "nothing to do"), "{rep:?}");

    // Force re-runs even when equal.
    let rep = e
        .update(
            "demo",
            &UpdateOpts {
                force: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(rep.failed.is_none(), "{rep:?}");
}

#[tokio::test]
async fn poisoned_digest_hard_stops_with_verify_failed_event() {
    let (base, _guard) = common::spawn().await;
    let home = tempfile::tempdir().unwrap();
    write_app(
        home.path(),
        "poison",
        &format!(
            r#"
id = "poison"
provider = "html-links"
url = "{base}/site/"
link_regex = 'href="(?P<href>[^"]+\.AppImage)"'
version_capture = '(?P<version>\d+\.\d+(\.\d+)?)'
digest_suffix = ".sha256"
digest_policy = "required"
allow_insecure_http = true

[install]
type = "none"
"#
        ),
    );
    let e = engine(home.path());

    // The 2025.99.99 release is the highest version; its sidecar carries a
    // decoy hash. required policy must hard-stop BEFORE installing anything.
    let rep = e.update("poison", &UpdateOpts::default()).await.unwrap();
    let failed = match rep.failed.clone() {
        Some(f) => f,
        None => panic!("must fail: {rep:?}"),
    };
    assert!(failed.contains("digest mismatch"), "{failed}");

    // verify_failed landed in history.
    let events = e.history(Some("poison"), 100).unwrap();
    assert!(
        events.iter().any(|ev| ev.kind == "verify_failed"),
        "{events:?}"
    );
    // And no state was recorded.
    assert!(e.installed_version("poison").unwrap().is_none());
}

const SRC_APP_TMPL: &str = r#"
id = "srcdemo"
provider = "source-build"
repo_url = "{repo}"
ref_regex = '^refs/tags/b\d+$'

[build]
steps = [["cp", "README.md", "out.txt"]]
tracked_files = ["README.md"]
artifacts = ["out.txt"]

[install]
type = "dir"
"#;

fn make_git_repo_with_tags(path: &Path) {
    let git = |args: &[&str]| {
        let st = std::process::Command::new("git")
            .args(args)
            .current_dir(path)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()
            .unwrap();
        assert!(st.success(), "git {args:?}");
    };
    git(&["init", "-q"]);
    std::fs::write(path.join("README.md"), b"build me v1\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "one"]);
    git(&["tag", "b1000"]);
    git(&["tag", "b2000"]);
}

#[tokio::test]
async fn source_build_gate_blocks_changed_scripts_until_approved() {
    let (base, _guard) = common::spawn().await;
    let _ = base; // fixture unused here but keeps parity with other suites

    let repo_dir = tempfile::tempdir().unwrap();
    make_git_repo_with_tags(repo_dir.path());

    let home = tempfile::tempdir().unwrap();
    write_app(
        home.path(),
        "srcdemo",
        &SRC_APP_TMPL.replace("{repo}", &repo_dir.path().to_string_lossy()),
    );
    let e = engine(home.path());

    // First build passes the gate (hash recorded) and publishes out.txt.
    let rep = e.update("srcdemo", &UpdateOpts::default()).await.unwrap();
    assert!(rep.failed.is_none(), "{rep:?}");
    let out = home.path().join("state/installs/srcdemo/current/out.txt");
    assert_eq!(std::fs::read(&out).unwrap(), b"build me v1\n");
    assert_eq!(
        e.installed_version("srcdemo").unwrap().as_deref(),
        Some("b2000")
    );
    let old_hash = e.script_hash_of("srcdemo").unwrap();

    // Scripts change upstream: new commit, new tag.
    std::fs::write(repo_dir.path().join("README.md"), b"build me EVIL v2\n").unwrap();
    let st = std::process::Command::new("git")
        .args(["commit", "-qam", "evil"])
        .current_dir(repo_dir.path())
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .status()
        .unwrap();
    assert!(st.success());
    let st = std::process::Command::new("git")
        .args(["tag", "b3000"])
        .current_dir(repo_dir.path())
        .status()
        .unwrap();
    assert!(st.success());

    // Update now stops at the gate with script_changed pending.
    let rep = e.update("srcdemo", &UpdateOpts::default()).await.unwrap();
    let failed = match rep.failed.clone() {
        Some(f) => f,
        None => panic!("gate must stop: {rep:?}"),
    };
    assert!(
        failed.contains("changed") && failed.contains("approve"),
        "{failed}"
    );
    let events = e.history(Some("srcdemo"), 100).unwrap();
    assert!(
        events.iter().any(|ev| ev.kind == "script_changed"),
        "{events:?}"
    );
    assert!(
        e.pending_approvals()
            .unwrap()
            .contains(&"srcdemo".to_string())
    );
    // Nothing built/published for the new tag.
    assert_eq!(std::fs::read(&out).unwrap(), b"build me v1\n");

    // Snapshots exist for --show-diff (old + new).
    let snaps = e.newest_snapshots("srcdemo").unwrap();
    assert_eq!(snaps.len(), 2);

    // Approve recomputes + clears; update then succeeds with new content.
    let (_old, new_hash) = e.approve("srcdemo").unwrap();
    assert_ne!(old_hash, Some(new_hash.clone()));
    assert!(
        !e.pending_approvals()
            .unwrap()
            .contains(&"srcdemo".to_string())
    );

    let rep = e.update("srcdemo", &UpdateOpts::default()).await.unwrap();
    assert!(rep.failed.is_none(), "{rep:?}");
    assert_eq!(
        e.installed_version("srcdemo").unwrap().as_deref(),
        Some("b3000")
    );
    assert_eq!(std::fs::read(&out).unwrap(), b"build me EVIL v2\n");
}
