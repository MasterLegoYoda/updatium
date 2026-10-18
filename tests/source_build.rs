//! source-build provider against a local git repository fixture.
use std::path::Path;
use std::process::Command as StdCommand;
use updatium::config::{AppSource, SourceBuildCfg, SourceKind};
use updatium::net;
use updatium::provider::{Provider, source_build::SourceBuild};

fn app(cfg: SourceBuildCfg) -> AppSource {
    AppSource {
        allow_insecure_http: false,
        kind: SourceKind::SourceBuild(cfg),
    }
}

fn git(dir: &Path, args: &[&str]) {
    let st = StdCommand::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .status()
        .expect("spawning git");
    assert!(st.success(), "git {args:?} failed");
}

/// Local repo with tags: b1000, b2000 (matching), v1.2.3 (filtered out).
fn make_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    std::fs::write(dir.path().join("README.md"), b"v1").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-qm", "one"]);
    git(dir.path(), &["tag", "b1000"]);
    std::fs::write(dir.path().join("README.md"), b"v2").unwrap();
    git(dir.path(), &["commit", "-qam", "two"]);
    git(dir.path(), &["tag", "b2000"]);
    git(dir.path(), &["tag", "v1.2.3"]);
    dir
}

#[tokio::test]
async fn lists_and_filters_tags_from_local_repo() {
    let repo = make_repo();
    let cfg = SourceBuildCfg {
        repo_url: repo.path().to_string_lossy().into_owned(),
        ref_regex: r"^refs/tags/b\d+$".into(),
        api_base: None,
        source_dir: None,
        install_dir: None,
        build: crate_build_cfg(),
    };
    let rels = SourceBuild
        .check(&app(cfg), &net::build_client().unwrap())
        .await
        .unwrap();
    let mut versions: Vec<&str> = rels.iter().map(|r| r.version.as_str()).collect();
    versions.sort_unstable();
    assert_eq!(
        versions,
        vec!["b1000", "b2000"],
        "refs/tags/ stripped, regex applied"
    );
    assert!(rels.iter().all(|r| r.assets.is_empty()));
}

#[tokio::test]
async fn no_matching_refs_is_empty_not_error() {
    let repo = make_repo();
    let cfg = SourceBuildCfg {
        repo_url: repo.path().to_string_lossy().into_owned(),
        ref_regex: r"^refs/tags/nope-\d+$".into(),
        api_base: None,
        source_dir: None,
        install_dir: None,
        build: crate_build_cfg(),
    };
    let rels = SourceBuild
        .check(&app(cfg), &net::build_client().unwrap())
        .await
        .unwrap();
    assert!(rels.is_empty());
}

fn crate_build_cfg() -> updatium::config::BuildCfg {
    toml::from_str(
        r#"
        steps = [["true"]]
        tracked_files = ["CMakeLists.txt"]
        artifacts = []
        install_steps = []
        "#,
    )
    .unwrap()
}
