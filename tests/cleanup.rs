use sha2::{Digest, Sha256};
use updatium::{cleanup::{self, CleanupCategory, CleanupOptions}, config::HomePaths, store::Store};

#[test]
fn cleanup_previews_owned_artifacts_and_preserves_unknown_and_modified_files() {
    let temp = tempfile::tempdir().unwrap();
    let home = HomePaths::resolve(Some(temp.path())).unwrap();
    let store = Store::open(&home.db_path()).unwrap();
    store.sync_apps(&[("demo", "command", true)]).unwrap();
    let key = hex::encode(Sha256::digest(b"demo"));
    let downloads = home.downloads().join(key);
    std::fs::create_dir_all(&downloads).unwrap();
    let owned = downloads.join("owned.tar");
    let changed = downloads.join("changed.tar");
    let foreign = downloads.join("foreign.tar");
    std::fs::write(&owned, b"original").unwrap();
    std::fs::write(&changed, b"user replacement").unwrap();
    std::fs::write(&foreign, b"foreign").unwrap();
    for path in [&owned, &changed] {
        let detail = serde_json::json!({"path": path, "sha256": hex::encode(Sha256::digest(b"original"))}).to_string();
        store.event("download_done", Some("demo"), Some(&detail)).unwrap();
    }
    let app = home.installs().join("demo");
    let stage = app.join(".updatium-staging-aB12cd");
    let unmarked = app.join(".updatium-staging-zY98xy");
    std::fs::create_dir_all(&stage).unwrap();
    std::fs::create_dir_all(&unmarked).unwrap();
    std::fs::write(stage.with_extension("owner"), b"updatium-staging-v1\n").unwrap();
    let snapshot = app.join("retained/snapshot-aB12cd");
    let snapshot_stage = app.join("retained/.updatium-staging-Cd34Ef");
    std::fs::create_dir_all(&snapshot_stage).unwrap();
    std::fs::write(snapshot_stage.with_extension("owner"), b"updatium-staging-v1\n").unwrap();
    std::fs::create_dir_all(snapshot.join("tree")).unwrap();
    std::fs::write(snapshot.join("tree/tool"), b"old").unwrap();
    std::fs::write(snapshot.join("manifest.json"), br#"{"format":1,"version":"1.0","created_nanos":1}"#).unwrap();
    let current = home.install_current("demo");
    std::fs::create_dir_all(&current).unwrap();
    std::fs::write(current.join("tool"), b"current").unwrap();
    let opts = CleanupOptions { downloads: true, staging: true, snapshots: true };
    let preview = cleanup::preview(&home, "demo", &opts).unwrap();
    assert_eq!(preview.len(), 4, "{preview:?}");
    assert!(preview.iter().any(|i| i.category == CleanupCategory::Download && i.path == owned));
    assert!(preview.iter().any(|i| i.category == CleanupCategory::Staging && i.path == stage));
    assert!(preview.iter().any(|i| i.category == CleanupCategory::Staging && i.path == snapshot_stage));
    assert!(preview.iter().any(|i| i.category == CleanupCategory::Snapshot && i.path == snapshot));
    assert!(owned.exists() && stage.exists() && snapshot.exists());
    std::fs::write(&owned, b"changed after preview").unwrap();
    assert!(cleanup::apply(&home, "demo", &opts, &preview).is_err());
    assert!(stage.exists() && snapshot.exists());
    std::fs::write(&owned, b"original").unwrap();
    cleanup::apply(&home, "demo", &opts, &preview).unwrap();
    assert!(!owned.exists() && !stage.exists() && !snapshot.exists() && !snapshot_stage.exists());
    assert!(changed.exists() && foreign.exists() && unmarked.exists());
    assert_eq!(std::fs::read(current.join("tool")).unwrap(), b"current");
}

#[test]
fn cleanup_refuses_active_and_pending_operations() {
    let temp = tempfile::tempdir().unwrap();
    let home = HomePaths::resolve(Some(temp.path())).unwrap();
    let store = Store::open(&home.db_path()).unwrap();
    store.sync_apps(&[("demo", "command", true)]).unwrap();
    let opts = CleanupOptions { downloads: true, ..Default::default() };
    store.begin_update("demo", "2.0").unwrap();
    assert!(cleanup::preview(&home, "demo", &opts).is_err());
    store.finish_update("demo").unwrap();
    let locks = home.state_dir.join("update-locks");
    std::fs::create_dir_all(&locks).unwrap();
    let key = hex::encode(Sha256::digest(b"demo"));
    let lock = std::fs::OpenOptions::new().create(true).write(true).open(locks.join(key)).unwrap();
    lock.try_lock().unwrap();
    assert!(cleanup::preview(&home, "demo", &opts).is_err());
}
