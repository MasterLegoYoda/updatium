use updatium::engine::{Engine, Status, UpdateOpts};

#[tokio::test]
async fn prefixed_tags_use_semver_precedence_without_changing_the_applied_tag() {
    let home = tempfile::tempdir().unwrap();
    let apps = home.path().join("config/apps.d");
    std::fs::create_dir_all(&apps).unwrap();
    let applied = home.path().join("applied");
    let config = apps.join("demo.toml");
    let write = |versions: &str| {
        std::fs::write(&config, format!(r#"
id = "demo"
provider = "command"
check_cmd = ["sh", "-c", "printf '%s\\n' {versions}"]
apply_cmd = ["sh", "-c", "printf '%s' \"$1\" > '{path}'", "--", "{{version}}"]
installed_version_cmd = ["sh", "-c", "if test -f '{path}'; then cat '{path}'; else echo v1.0.0; fi"]
[install]
type = "none"
"#, path = applied.display())).unwrap();
    };
    let engine = Engine::new(Some(home.path())).unwrap();
    write("v1.0.0-rc.1 1.0.0+build.2");
    let check = engine.check(None).await.unwrap();
    assert!(matches!(check[0].status, Status::Ok));
    assert_eq!(check[0].latest.as_deref(), Some("1.0.0+build.2"));
    let update = engine.update("demo", &UpdateOpts::default()).await.unwrap();
    assert!(update.actions.iter().any(|action| action == "nothing to do"));
    assert!(!applied.exists());

    write("v1.0.0 1.0.0+build.3 v1.1.0-rc.1");
    let check = engine.check(None).await.unwrap();
    assert!(matches!(check[0].status, Status::UpdateAvailable));
    assert_eq!(check[0].latest.as_deref(), Some("v1.1.0-rc.1"));
    let update = engine.update("demo", &UpdateOpts::default()).await.unwrap();
    assert!(update.failed.is_none(), "{:?}", update.failed);
    assert_eq!(update.to.as_deref(), Some("v1.1.0-rc.1"));
    assert_eq!(std::fs::read_to_string(&applied).unwrap(), "v1.1.0-rc.1");
}
