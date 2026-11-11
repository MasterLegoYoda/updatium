use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

fn executable(path: &Path, script: &str) {
    std::fs::write(path, script).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn cli(home: &Path, path: &str, args: &[&str]) -> serde_json::Value {
    let output = Command::new(env!("CARGO_BIN_EXE_updatium"))
        .arg("--home").arg(home).arg("--json").args(args)
        .env("PATH", path)
        .output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn track_only_preserves_probe_and_outstanding_updates_for_all_providers() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path();
    let apps = root.join("config/apps.d");
    let providers = root.join("config/providers.d");
    let bin = root.join("bin");
    for dir in [&apps, &providers, &bin] { std::fs::create_dir_all(dir).unwrap(); }
    executable(&bin.join("apt-cache"), "#!/bin/sh\necho 'Candidate: 2.0.0'\n");
    executable(&bin.join("flatpak"), "#!/bin/sh\nprintf 'org.example.App\\t2.0.0\\tstable\\n'\n");
    executable(&providers.join("stub"), "#!/bin/sh\ncase \"$1\" in\ndescribe) echo '{\"protocol\":1,\"id\":\"stub\"}' ;;\ncheck) echo '{\"releases\":[{\"version\":\"2.0.0\",\"assets\":[]}]}' ;;\nesac\n");
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    for (id, source) in [
        ("command", "provider = 'command'\ncheck_cmd = ['sh', '-c', 'echo 2.0.0']"),
        ("apt", "provider = 'apt'\npackage = 'example'"),
        ("flatpak", "provider = 'flatpak'\napp_id = 'org.example.App'\nremote = 'flathub'"),
        ("external", "provider = 'external:stub'"),
    ] {
        std::fs::write(apps.join(format!("{id}.toml")), format!("id = '{id}'\n{source}\ninstalled_version_cmd = ['sh', '-c', 'echo 1.0.0']\n[install]\ntype = 'none'\n")).unwrap();
        for extra in [vec![], vec!["--dry-run"], vec!["--force"]] {
            let mut args = vec!["update", id];
            args.extend(extra);
            let update = cli(root, &path, &args);
            assert_eq!(update[0]["from"], "1.0.0", "{id}");
            assert_eq!(update[0]["to"], "2.0.0", "{id}");
            assert_eq!(update[0]["track_only"], true, "{id}");
            assert!(update[0]["actions"].to_string().contains("no installation performed"));
        }
        let check = cli(root, &path, &["check", id]);
        assert_eq!(check[0]["installed"], "1.0.0", "{id}");
        assert_eq!(check[0]["installed_source"], "observed", "{id}");
        assert_eq!(check[0]["latest"], "2.0.0", "{id}");
        assert_eq!(check[0]["status"], "update_available", "{id}");
        let history = cli(root, &path, &["history", id]);
        assert!(!history.to_string().contains("install_started"), "{id}");
    }

    // Switching to a real apply still records the installed target.
    std::fs::write(apps.join("command.toml"), "id = 'command'\nprovider = 'command'\ncheck_cmd = ['sh', '-c', 'echo 2.0.0']\napply_cmd = ['sh', '-c', 'exit 0']\n[install]\ntype = 'none'\n").unwrap();
    let update = cli(root, &path, &["update", "command"]);
    assert_eq!(update[0]["track_only"], false);
    let check = cli(root, &path, &["check", "command"]);
    assert_eq!(check[0]["installed"], "2.0.0");
    assert_eq!(check[0]["installed_source"], "recorded");
    assert_eq!(check[0]["status"], "ok");

    // A later track-only observation must also preserve recorded state.
    std::fs::write(apps.join("command.toml"), "id = 'command'\nprovider = 'command'\ncheck_cmd = ['sh', '-c', 'echo 3.0.0']\n[install]\ntype = 'none'\n").unwrap();
    let update = cli(root, &path, &["update", "command"]);
    assert_eq!(update[0]["from"], "2.0.0");
    assert_eq!(update[0]["track_only"], true);
    let check = cli(root, &path, &["check", "command"]);
    assert_eq!(check[0]["installed"], "2.0.0");
    assert_eq!(check[0]["installed_source"], "recorded");
    assert_eq!(check[0]["status"], "update_available");

    // An unavailable probe does not block an observation-only update.
    std::fs::write(apps.join("command.toml"), "id = 'command'\nprovider = 'command'\ncheck_cmd = ['sh', '-c', 'echo 3.0.0']\ninstalled_version_cmd = ['sh', '-c', 'exit 1']\n[install]\ntype = 'none'\n").unwrap();
    let update = cli(root, &path, &["update", "command"]);
    assert!(update[0]["from"].is_null());
    assert_eq!(update[0]["track_only"], true);
    assert_eq!(update[0]["to"], "3.0.0");
    assert!(update[0]["warnings"].to_string().contains("installed-version probe failed"));
    let dry = cli(root, &path, &["update", "command", "--dry-run"]);
    assert!(dry[0]["warnings"].to_string().contains("installed-version probe failed"));
}
