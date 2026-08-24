//! External plugin protocol tests: bash stub providers in a temp providers.d.
use std::path::PathBuf;
use updatium::config::{GlobalConfig, HomePaths, SourceKind};
use updatium::net;
use updatium::provider::{Provider, registry};

fn write_stub(dir: &PathBuf, name: &str, body: &str) {
    std::fs::create_dir_all(dir).unwrap();
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn home_with_providers(dir: &std::path::Path) -> GlobalConfig {
    let mut g = GlobalConfig::default();
    g.home = HomePaths::resolve(Some(dir)).unwrap();
    g
}

const STUB_OK: &str = r#"#!/usr/bin/env bash
case "$1" in
  describe) echo '{"protocol":1,"id":"stub"}' ;;
  check)
    cfg=$(cat)
    VER=$(printf '%s' "$cfg" | python3 -c 'import json,sys; print(json.load(sys.stdin)["version"])')
    printf '{"releases":[{"version":"%s","published_at":null,"assets":[{"name":"f.bin","url":"https://example.invalid/f.bin","size":3,"digest":"sha256:%s"}]}]}' "$VER" "$(printf 'a%.0s' $(seq 64))"
    ;;
  *) exit 9 ;;
esac
"#;

#[tokio::test]
async fn stub_provider_round_trips_config() {
    let home = tempfile::tempdir().unwrap();
    let g = home_with_providers(home.path());
    write_stub(&g.home.providers_d(), "stub", STUB_OK);

    let reg = registry(&g);
    let prov = registry_lookup_ok(&reg, "external:stub");

    let raw: toml::Value = toml::from_str(r#"version = "9.9.9""#).unwrap();
    let src = updatium::config::AppSource {
        allow_insecure_http: false,
        kind: SourceKind::External {
            id: "stub".into(),
            raw,
        },
    };
    let rels = prov
        .check(&src, &net::build_client().unwrap())
        .await
        .unwrap();
    assert_eq!(rels.len(), 1);
    assert_eq!(
        rels[0].version, "9.9.9",
        "stdin config must reach the plugin"
    );
    assert_eq!(rels[0].assets[0].digest_hex(), Some(&"a".repeat(64)[..]));
}

const STUB_BAD_PROTOCOL: &str = r#"#!/usr/bin/env bash
echo '{"protocol":2,"id":"future"}'
"#;

#[tokio::test]
async fn wrong_protocol_version_is_rejected() {
    let home = tempfile::tempdir().unwrap();
    let g = home_with_providers(home.path());
    write_stub(&g.home.providers_d(), "future", STUB_BAD_PROTOCOL);

    let reg = registry(&g);
    let prov = registry_lookup_ok(&reg, "external:future");
    let src = updatium::config::AppSource {
        allow_insecure_http: false,
        kind: SourceKind::External {
            id: "future".into(),
            raw: toml::Value::Table(Default::default()),
        },
    };
    let err = prov
        .check(&src, &net::build_client().unwrap())
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("protocol"), "{err:#}");
}

const STUB_FAILING: &str = r#"#!/usr/bin/env bash
echo "disk exploded" >&2
exit 4
"#;

#[tokio::test]
async fn nonzero_exit_surfaces_stderr() {
    let home = tempfile::tempdir().unwrap();
    let g = home_with_providers(home.path());
    write_stub(&g.home.providers_d(), "broken", STUB_FAILING);

    let reg = registry(&g);
    let prov = registry_lookup_ok(&reg, "external:broken");
    let src = updatium::config::AppSource {
        allow_insecure_http: false,
        kind: SourceKind::External {
            id: "broken".into(),
            raw: toml::Value::Table(Default::default()),
        },
    };
    let err = prov
        .check(&src, &net::build_client().unwrap())
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("disk exploded"), "{err:#}");
}

#[tokio::test]
async fn unknown_id_lists_valid_ones() {
    let home = tempfile::tempdir().unwrap();
    let g = home_with_providers(home.path());
    write_stub(&g.home.providers_d(), "stub", STUB_OK);
    let reg = registry(&g);
    let err = match updatium::provider::lookup(&reg, "external:nope") {
        Ok(_) => panic!("expected lookup failure"),
        Err(e) => e,
    };
    let msg = format!("{err:#}");
    assert!(
        msg.contains("external:nope") && msg.contains("external:stub"),
        "{msg}"
    );
}

fn registry_lookup_ok(
    reg: &std::collections::HashMap<String, std::sync::Arc<dyn Provider>>,
    id: &str,
) -> std::sync::Arc<dyn Provider> {
    updatium::provider::lookup(reg, id).unwrap()
}
