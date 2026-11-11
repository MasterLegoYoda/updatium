//! Engine orchestration tests: full appimage install, digest gate, dry-run,
//! source-build approval gate — all under a relocated UPDATIUM_HOME.
mod common;

use std::path::Path;
use updatium::engine::{Engine, InstalledSource, Status, UpdateOpts, UpdateProgress};

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
    let progress = std::sync::Mutex::new(Vec::new());
    let rep = e.update_with_progress("demo", &UpdateOpts::default(), &|event| {
        progress.lock().unwrap().push(event);
    }).await.unwrap();
    let progress = progress.into_inner().unwrap();
    assert!(matches!(progress.first(), Some(UpdateProgress::Stage("Checking for updates"))));
    let stages: Vec<_> = progress.iter().filter_map(|event| match event {
        UpdateProgress::Stage(stage) => Some(*stage),
        _ => None,
    }).collect();
    assert_eq!(stages, ["Checking for updates", "Downloading", "Verifying", "Installing"]);
    assert!(progress.iter().any(|event| matches!(event, UpdateProgress::Download { downloaded, total: Some(total) } if *downloaded > 0 && downloaded == total)));
    assert!(rep.failed.is_none(), "{rep:?}");
    let installed_path = home
        .path()
        .join("state/Applications/freecad-demo-2.1.0-x86_64.AppImage");
    assert!(installed_path.exists(), "{:?}", home.path().join("state"));
    assert_eq!(
        e.installed_version("demo").unwrap().as_deref(),
        Some("2.1.0")
    );
    let reports = e.check(None).await.unwrap();
    assert_eq!(reports[0].installed_source, InstalledSource::Recorded);

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

    // A recorded version is not evidence that a removed managed artifact exists.
    std::fs::remove_file(&installed_path).unwrap();
    let reports = e.check(None).await.unwrap();
    assert_eq!(reports[0].installed, None);
    assert_eq!(reports[0].installed_source, InstalledSource::Unknown);
    let rep = e.update("demo", &UpdateOpts::default()).await.unwrap();
    assert!(rep.failed.is_none(), "{rep:?}");
    assert_eq!(rep.to.as_deref(), Some("2.1.0"));
    assert!(installed_path.exists());
}

#[tokio::test]
async fn latest_comes_from_current_result_not_stale_history() {
    let (base, _guard) = common::spawn().await;
    let home = tempfile::tempdir().unwrap();
    let write = |tag_regex: &str, prereleases: bool| {
        write_app(
            home.path(),
            "demo",
            &format!(
                r#"
id = "demo"
provider = "github-releases"
allow_insecure_http = true
repo = "demo/app"
tag_regex = '{tag_regex}'
include_prereleases = {prereleases}
api_base = "{base}/gh"

[install]
type = "none"
"#
            ),
        );
    };
    let e = engine(home.path());

    // Record the prerelease 2.0.0 as the newest known version.
    write(r"^v?2\.0\.0$", true);
    let reports = e.check(None).await.unwrap();
    assert_eq!(reports[0].latest.as_deref(), Some("2.0.0"));

    // Tighten the filter so 2.0.0 is no longer advertised. The report must
    // follow the current result set (1.8.0), not the all-time DB maximum.
    write(r"^v?1\.8\.0$", false);
    let reports = e.check(None).await.unwrap();
    assert_eq!(reports[0].latest.as_deref(), Some("1.8.0"));
}

#[tokio::test]
async fn installed_version_cmd_detects_unmanaged_install() {
    let (base, _guard) = common::spawn().await;
    let home = tempfile::tempdir().unwrap();
    write_app(
        home.path(),
        "demo",
        &format!(
            r#"
id = "demo"
provider = "github-releases"
allow_insecure_http = true
repo = "demo/app"
api_base = "{base}/gh"
installed_version_cmd = ["sh", "-c", "echo 2.0.0"]

[install]
type = "none"
"#
        ),
    );
    let e = engine(home.path());

    // No DB state: the configured command supplies the installed version.
    let reports = e.check(None).await.unwrap();
    assert_eq!(reports[0].installed.as_deref(), Some("2.0.0"));
    assert_eq!(reports[0].latest.as_deref(), Some("2.1.0"));
    assert!(matches!(reports[0].status, Status::UpdateAvailable));
    assert_eq!(
        e.installed_version("demo").unwrap().as_deref(),
        Some("2.0.0")
    );

    // Update only selects strictly newer releases, proving detection fed the
    // normal update path rather than forcing a reinstall of the same version.
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
    assert_eq!(rep.from.as_deref(), Some("2.0.0"));
    assert_eq!(rep.to.as_deref(), Some("2.1.0"));
}

#[tokio::test]
async fn explicit_probe_reconciles_after_apply_without_rewriting_history() {
    let home = tempfile::tempdir().unwrap();
    let version = home.path().join("installed");
    std::fs::write(&version, "1.0.0\n").unwrap();
    let config = |probe: &str| format!(r#"
id = "demo"
provider = "command"
check_cmd = ["sh", "-c", "echo 2.0.0"]
apply_cmd = ["sh", "-c", "printf '%s\\n' \"$1\" > '{path}'", "--", "{{version}}"]
installed_version_cmd = ["sh", "-c", "{probe}"]
[install]
type = "none"
"#, path = version.display());
    let probe = format!("cat '{}'", version.display());
    write_app(home.path(), "demo", &config(&probe));
    let e = engine(home.path());
    let applied = e.update("demo", &UpdateOpts::default()).await.unwrap();
    assert!(applied.failed.is_none(), "{applied:?}");
    assert_eq!(applied.to.as_deref(), Some("2.0.0"));

    for (observed, status_update, target) in [
        ("3.0.0\nignored\n", false, false),
        ("1.5.0\n", true, true),
    ] {
        std::fs::write(&version, observed).unwrap();
        let reports = e.check(None).await.unwrap();
        assert_eq!(reports[0].installed.as_deref(), observed.lines().next());
        assert_eq!(reports[0].installed_source, InstalledSource::Observed);
        assert_eq!(matches!(reports[0].status, Status::UpdateAvailable), status_update);
        let dry = e.update("demo", &UpdateOpts { dry_run: true, ..Default::default() }).await.unwrap();
        assert_eq!(dry.to.as_deref() == Some("2.0.0"), target);
    }

    for bad_probe in ["exit 1", "printf ''", &probe] {
        if bad_probe == probe { std::fs::remove_file(&version).unwrap(); }
        write_app(home.path(), "demo", &config(bad_probe));
        let reports = e.check(None).await.unwrap();
        assert_eq!(reports[0].installed, None);
        assert_eq!(reports[0].installed_source, InstalledSource::Unknown);
        let update = e.update("demo", &UpdateOpts::default()).await.unwrap();
        assert!(update.failed.is_some(), "{update:?}");
    }

    std::fs::write(&version, "1.0.0\n").unwrap();
    write_app(home.path(), "demo", &config(&probe));
    let reports = e.check(None).await.unwrap();
    assert_eq!(reports[0].installed.as_deref(), Some("1.0.0"));
    assert_eq!(reports[0].installed_source, InstalledSource::Observed);
    assert!(matches!(reports[0].status, Status::UpdateAvailable));

    // Removing the probe exposes the original successful apply, not an observation.
    write_app(home.path(), "demo", &config(&probe).lines().filter(|line| !line.starts_with("installed_version_cmd")).collect::<Vec<_>>().join("\n"));
    let reports = e.check(None).await.unwrap();
    assert_eq!(reports[0].installed.as_deref(), Some("2.0.0"));
    assert_eq!(reports[0].installed_source, InstalledSource::Recorded);
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

/// Two sequential commits tagged b1000 and b2000, ending detached at b1000.
fn make_git_repo_with_sequential_tags(path: &Path) {
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
    std::fs::write(path.join("README.md"), b"build me v2\n").unwrap();
    git(&["commit", "-qam", "two"]);
    git(&["tag", "b2000"]);
    git(&["checkout", "-q", "b1000"]);
}

#[tokio::test]
async fn check_detects_source_build_version_from_checkout() {
    let (base, _guard) = common::spawn().await;
    let _ = base; // fixture unused here but keeps parity with other suites

    let repo_dir = tempfile::tempdir().unwrap();
    make_git_repo_with_sequential_tags(repo_dir.path());

    let home = tempfile::tempdir().unwrap();
    let install_dir = home.path().join("llama-install");
    std::fs::create_dir_all(&install_dir).unwrap();
    write_app(
        home.path(),
        "srcdemo",
        &format!(
            r#"
id = "srcdemo"
provider = "source-build"
repo_url = "{repo}"
source_dir = "{repo}"
install_dir = "{install}"
ref_regex = '^refs/tags/b\d+$'

[build]
steps = [["true"]]
tracked_files = ["README.md"]
artifacts = []

[install]
type = "dir"
"#,
            repo = repo_dir.path().display(),
            install = install_dir.display(),
        ),
    );
    let e = engine(home.path());

    // No DB install state, but the checkout is at b1000 -> report it.
    let reports = e.check(None).await.unwrap();
    assert_eq!(reports[0].installed.as_deref(), Some("b1000"));
    assert_eq!(reports[0].installed_source, InstalledSource::Inferred);
    assert_eq!(reports[0].latest.as_deref(), Some("b2000"));
    assert!(matches!(reports[0].status, Status::UpdateAvailable));
    assert_eq!(
        e.installed_version("srcdemo").unwrap().as_deref(),
        Some("b1000")
    );
}

#[tokio::test]
async fn external_source_cutover_failure_preserves_the_previous_installation() {
    let repo = tempfile::tempdir().unwrap();
    make_git_repo_with_tags(repo.path());
    let home = tempfile::tempdir().unwrap();
    let destination = home.path().join("external-install");
    std::fs::create_dir(&destination).unwrap();
    std::fs::write(destination.join("tool"), b"original").unwrap();
    let configure = |install_steps: &str| write_app(home.path(), "srcdemo", &format!(r#"
id = "srcdemo"
provider = "source-build"
repo_url = "{repo}"
install_dir = "{destination}"
ref_regex = '^refs/tags/b\d+$'
[build]
steps = [["true"]]
tracked_files = ["README.md"]
artifacts = []
install_steps = {install_steps}
[install]
type = "dir"
"#, repo = repo.path().display(), destination = destination.display()));
    let e = engine(home.path());
    configure(r#"[["sh", "-c", "printf new > \"$1/tool\"", "--", "{install_dir}"]]"#);
    let first = e.update("srcdemo", &UpdateOpts::default()).await.unwrap();
    assert!(first.failed.is_none(), "{first:?}");
    assert_eq!(std::fs::read(destination.join("tool")).unwrap(), b"new");
    assert_eq!(e.installed_version("srcdemo").unwrap().as_deref(), Some("b2000"));

    // A successful install command that removes its staging directory forces
    // the subsequent atomic activation to fail, after the old install exists.
    configure(r#"[["sh", "-c", "rm -rf -- \"$1\"", "--", "{install_dir}"]]"#);
    let failed = e.update("srcdemo", &UpdateOpts { force: true, ..Default::default() }).await.unwrap();
    assert!(failed.failed.as_deref().unwrap().contains("activating"), "{failed:?}");
    assert_eq!(std::fs::read(destination.join("tool")).unwrap(), b"new");
    assert_eq!(e.installed_version("srcdemo").unwrap().as_deref(), Some("b2000"));

    std::fs::remove_dir_all(&destination).unwrap();
    let checked = e.check(None).await.unwrap();
    assert_eq!(checked[0].installed, None);
    assert_eq!(checked[0].installed_source, InstalledSource::Unknown);
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
    let progress = std::sync::Mutex::new(Vec::new());
    let rep = e.update_with_progress("srcdemo", &UpdateOpts::default(), &|event| {
        progress.lock().unwrap().push(event);
    }).await.unwrap();
    let stages: Vec<_> = progress.into_inner().unwrap().into_iter().filter_map(|event| match event {
        UpdateProgress::Stage(stage) => Some(stage),
        _ => None,
    }).collect();
    assert_eq!(stages, ["Checking for updates", "Fetching and checking out source", "Checking build scripts", "Building", "Installing"]);
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
