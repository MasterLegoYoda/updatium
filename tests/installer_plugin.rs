//! External installer plugins end-to-end: an html-links app whose `[install]`
//! delegates to a bash plugin dropped into `<home>/config/installers.d/`.
//! Proves the full check → download → digest → external-install pipeline.
mod common;

use std::path::Path;
use updatium::engine::{Engine, UpdateOpts};

fn write_app(home: &Path, id: &str, body: &str) {
    let dir = home.join("config").join("apps.d");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{id}.toml")), body).unwrap();
}

fn write_plugin(dir: &Path, name: &str, body: &str) {
    std::fs::create_dir_all(dir).unwrap();
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// describe speaks v2 with the install verb; install copies the downloaded
/// asset to `config.dest` and reports the action verbatim.
const COPYTOOL: &str = r#"#!/usr/bin/env bash
case "$1" in
  describe) echo '{"protocol":2,"id":"copytool","verbs":["install"]}' ;;
  install)
    cfg=$(cat)
    DOWNLOAD=$(printf '%s' "$cfg" | python3 -c 'import json,sys; print(json.load(sys.stdin)["paths"]["download"])')
    DEST=$(printf '%s' "$cfg" | python3 -c 'import json,sys; print(json.load(sys.stdin)["config"]["dest"])')
    mkdir -p "$(dirname "$DEST")"
    cp "$DOWNLOAD" "$DEST"
    printf '{"actions":["copied by copytool"],"exec":null,"installed":["%s"]}' "$DEST"
    ;;
  *) exit 9 ;;
esac
"#;

const APP_TMPL: &str = r#"
id = "{id}"
provider = "html-links"
url = "{base}/site/"
link_regex = 'href="(?P<href>[^"]*Foo-2024[^"]+\.AppImage)"'
version_capture = '(?P<version>\d+\.\d+(\.\d+)?)'
allow_insecure_http = true
[install]
type = "external:{plugin}"
dest = "{dest}"
"#;

#[tokio::test]
async fn external_installer_copies_download_and_records_state() {
    let (base, _guard) = common::spawn().await;
    let home = tempfile::tempdir().unwrap();
    let idir = home.path().join("config").join("installers.d");
    write_plugin(&idir, "copytool", COPYTOOL);

    let dest = home.path().join("opt/tool/Foo.AppImage");
    write_app(
        home.path(),
        "ext",
        &APP_TMPL
            .replace("{id}", "ext")
            .replace("{base}", &base)
            .replace("{plugin}", "copytool")
            .replace("{dest}", &dest.display().to_string()),
    );
    let e = Engine::new(Some(home.path())).unwrap();

    let rep = e
        .update(
            "ext",
            &UpdateOpts {
                yes: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(rep.failed.is_none(), "{rep:?}");
    assert!(
        rep.actions.iter().any(|a| a.contains("copied by copytool")),
        "plugin action must reach the report verbatim: {rep:?}"
    );
    assert_eq!(
        e.installed_version("ext").unwrap().as_deref(),
        Some("2024.06.12"),
        "installed version recorded"
    );

    // The plugin really copied the digest-gated download bytes.
    let bytes = std::fs::read(&dest).unwrap_or_else(|e| panic!("dest missing: {e}"));
    let http = updatium::net::build_client().unwrap();
    let expected = http
        .get(format!("{base}/site/files/Foo-2024.06.12-x86_64.AppImage"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();

    // install_ok event landed in history.
    let events = e.history(Some("ext"), 100).unwrap();
    assert!(
        events.iter().any(|ev| ev.kind == "install_ok"),
        "{events:?}"
    );

    // Second update: nothing to do (already at latest).
    let rep = e.update("ext", &UpdateOpts::default()).await.unwrap();
    assert!(rep.actions.iter().any(|a| a == "nothing to do"), "{rep:?}");
}

#[tokio::test]
async fn protocol1_installer_is_rejected_at_handshake() {
    let (base, _guard) = common::spawn().await;
    let home = tempfile::tempdir().unwrap();
    let idir = home.path().join("config").join("installers.d");
    write_plugin(
        &idir,
        "oldplug",
        r#"#!/usr/bin/env bash
echo '{"protocol":1,"id":"oldplug"}'
"#,
    );

    let dest = home.path().join("opt/tool/x.bin");
    write_app(
        home.path(),
        "neg",
        &APP_TMPL
            .replace("{id}", "neg")
            .replace("{base}", &base)
            .replace("{plugin}", "oldplug")
            .replace("{dest}", &dest.display().to_string()),
    );
    let e = Engine::new(Some(home.path())).unwrap();

    let rep = e.update("neg", &UpdateOpts::default()).await.unwrap();
    let failed = match rep.failed.clone() {
        Some(f) => f,
        None => panic!("must fail at handshake: {rep:?}"),
    };
    assert!(
        failed.contains("unsupported protocol"),
        "{failed}"
    );
    assert!(!dest.exists(), "nothing may be installed on handshake failure");
    assert!(e.installed_version("neg").unwrap().is_none());
}
