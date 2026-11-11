use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

fn executable(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn cli(home: &Path, path: &str, args: &[&str]) -> (i32, serde_json::Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_updatium"))
        .arg("--home").arg(home).arg("--json").args(args).env("PATH", path)
        .output().unwrap();
    let json = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|_| panic!("stdout: {}; stderr: {}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr)));
    (output.status.code().unwrap(), json)
}

#[test]
fn apt_selection_respects_epoch_and_debian_revision() {
    let home = tempfile::tempdir().unwrap();
    let apps = home.path().join("config/apps.d");
    let bin = home.path().join("bin");
    std::fs::create_dir_all(&apps).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    let candidate = home.path().join("candidate");
    let applied = home.path().join("applied");
    executable(&bin.join("apt-cache"), &format!("#!/bin/sh\nprintf 'Candidate: '; cat '{}'\n", candidate.display()));
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    std::fs::write(apps.join("apt.toml"), format!(r#"
id = "apt"
provider = "apt"
package = "fixture"
installed_version_cmd = ["sh", "-c", "echo '1:1.0-1'"]
apply_cmd = ["sh", "-c", "printf '%s' \"$1\" > '{}'", "--", "{{version}}"]
[install]
type = "none"
"#, applied.display())).unwrap();
    std::fs::write(&candidate, "2.0-9\n").unwrap();
    let (code, check) = cli(home.path(), &path, &["check", "apt"]);
    assert_eq!(code, 0, "{check}");
    assert_eq!(check[0]["status"], "ok");
    let (_, update) = cli(home.path(), &path, &["update", "apt"]);
    assert!(!update[0]["installed"].as_bool().unwrap());
    assert!(!applied.exists());
    std::fs::write(&candidate, "1:1.0-2\n").unwrap();
    let (code, check) = cli(home.path(), &path, &["check", "apt"]);
    assert_eq!(code, 0, "{check}");
    assert_eq!(check[0]["status"], "update_available");
    let (code, update) = cli(home.path(), &path, &["update", "apt"]);
    assert_eq!(code, 0, "{update}");
    assert_eq!(std::fs::read_to_string(applied).unwrap(), "1:1.0-2");
}

#[test]
fn mixed_release_formats_are_rejected_before_applying_even_when_forced() {
    let home = tempfile::tempdir().unwrap();
    let apps = home.path().join("config/apps.d");
    std::fs::create_dir_all(&apps).unwrap();
    let applied = home.path().join("applied");
    std::fs::write(apps.join("demo.toml"), format!(r#"
id = "demo"
provider = "command"
check_cmd = ["sh", "-c", "printf '%s\\n' 1.2.3 b999"]
apply_cmd = ["sh", "-c", "touch '{}'"]
[install]
type = "none"
"#, applied.display())).unwrap();
    let path = std::env::var("PATH").unwrap_or_default();
    let (code, check) = cli(home.path(), &path, &["check", "demo"]);
    assert_eq!(code, 2, "{check}");
    assert!(check[0]["error"].as_str().unwrap().contains("incompatible version formats"));
    let (code, update) = cli(home.path(), &path, &["update", "demo", "--force"]);
    assert_eq!(code, 2, "{update}");
    assert!(update[0]["failed"].as_str().unwrap().contains("incompatible version formats"));
    assert!(!applied.exists());
}

#[test]
fn flatpak_requires_selecting_one_branch_before_update() {
    let home = tempfile::tempdir().unwrap();
    let apps = home.path().join("config/apps.d");
    let bin = home.path().join("bin");
    std::fs::create_dir_all(&apps).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    executable(&bin.join("flatpak"), "#!/bin/sh\nprintf 'org.example.App\\t1.0.0\\tstable\\norg.example.App\\t2.0.0\\tbeta\\n'\n");
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let config = apps.join("demo.toml");
    let body = r#"id = "demo"
provider = "flatpak"
app_id = "org.example.App"
remote = "example"
installed_version_cmd = ["sh", "-c", "echo 0.9.0"]
"#;
    std::fs::write(&config, format!("{body}[install]\ntype = 'none'\n")).unwrap();
    let (code, check) = cli(home.path(), &path, &["check", "demo"]);
    assert_eq!(code, 2, "{check}");
    assert!(check[0]["error"].as_str().unwrap().contains("branch_regex"));
    let (code, update) = cli(home.path(), &path, &["update", "demo", "--force"]);
    assert_eq!(code, 2, "{update}");
    assert!(update[0]["failed"].as_str().unwrap().contains("branch_regex"));
    std::fs::write(&config, format!("{body}branch_regex = '^stable$'\n[install]\ntype = 'none'\n")).unwrap();
    let (code, check) = cli(home.path(), &path, &["check", "demo"]);
    assert_eq!(code, 0, "{check}");
    assert_eq!(check[0]["latest"], "1.0.0");
    assert_eq!(check[0]["status"], "update_available");
}

#[test]
fn installed_and_release_formats_must_agree() {
    let home = tempfile::tempdir().unwrap();
    let apps = home.path().join("config/apps.d");
    std::fs::create_dir_all(&apps).unwrap();
    std::fs::write(apps.join("demo.toml"), r#"id = "demo"
provider = "command"
check_cmd = ["sh", "-c", "echo 1.2.3"]
installed_version_cmd = ["sh", "-c", "echo b999"]
[install]
type = "none"
"#).unwrap();
    let path = std::env::var("PATH").unwrap_or_default();
    let (code, check) = cli(home.path(), &path, &["check", "demo"]);
    assert_eq!(code, 2, "{check}");
    assert!(check[0]["error"].as_str().unwrap().contains("installed_version_cmd"));
    let (code, update) = cli(home.path(), &path, &["update", "demo"]);
    assert_eq!(code, 2, "{update}");
    assert!(update[0]["failed"].as_str().unwrap().contains("incompatible version formats"));
}
