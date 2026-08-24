//! command + docker-registry provider tests.
mod common;

use updatium::config::{AppSource, CommandCfg, DockerRegistryCfg, SourceKind};
use updatium::net;
use updatium::provider::Provider;
use updatium::provider::command::Command;
use updatium::provider::docker_registry::DockerRegistry;

fn app(kind: SourceKind) -> AppSource {
    AppSource {
        allow_insecure_http: true,
        kind,
    }
}

#[tokio::test]
async fn command_provider_lists_versions_in_order() {
    let cfg = CommandCfg {
        check_cmd: vec![
            "bash".into(),
            "-c".into(),
            "printf '10.5\\n9.1.0\\nv2.0\\n\\n'".into(),
        ],
        apply_cmd: None,
    };
    let p = Command;
    let rels = p
        .check(
            &app(SourceKind::Command(cfg)),
            &net::build_client().unwrap(),
        )
        .await
        .unwrap();
    let versions: Vec<&str> = rels.iter().map(|r| r.version.as_str()).collect();
    assert_eq!(
        versions,
        vec!["10.5", "9.1.0", "v2.0"],
        "order preserved, blanks skipped"
    );
    assert!(rels.iter().all(|r| r.assets.is_empty()));
}

#[tokio::test]
async fn command_provider_surfaces_failure() {
    let cfg = CommandCfg {
        check_cmd: vec!["bash".into(), "-c".into(), "echo boom >&2; exit 3".into()],
        apply_cmd: None,
    };
    let p = Command;
    let err = p
        .check(
            &app(SourceKind::Command(cfg)),
            &net::build_client().unwrap(),
        )
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("boom"), "{err:#}");
}

#[tokio::test]
async fn docker_hub_flow_follows_next_pages() {
    let (base, _guard) = common::spawn().await;
    let cfg = DockerRegistryCfg {
        image: "library/nginx".into(),
        tag_regex: None,
        pull_cmd: vec!["docker".into(), "pull".into(), "{image}:{version}".into()],
        redeploy_cmd: None,
        api_base: base.clone(),
    };
    let p = DockerRegistry;
    let rels = p
        .check(
            &app(SourceKind::DockerRegistry(cfg)),
            &net::build_client().unwrap(),
        )
        .await
        .unwrap();
    let mut versions: Vec<&str> = rels.iter().map(|r| r.version.as_str()).collect();
    versions.sort();
    // Both `next` pages consumed.
    assert_eq!(versions, vec!["1.26.3", "1.27.3", "1.27.4", "latest"]);
}

#[tokio::test]
async fn docker_ghcr_flow_token_and_link_pagination() {
    let (base, _guard) = common::spawn().await;
    let cfg = DockerRegistryCfg {
        image: "ghcr.io/open-webui/open-webui".into(),
        tag_regex: Some(r"^v\d".into()),
        pull_cmd: vec!["docker".into(), "pull".into(), "{image}:{version}".into()],
        redeploy_cmd: None,
        api_base: base.clone(),
    };
    let p = DockerRegistry;
    let rels = p
        .check(
            &app(SourceKind::DockerRegistry(cfg.clone())),
            &net::build_client().unwrap(),
        )
        .await
        .unwrap();
    let mut versions: Vec<&str> = rels.iter().map(|r| r.version.as_str()).collect();
    versions.sort();
    // Link-header pagination delivered all three v-tags; `main` filtered by regex.
    assert_eq!(versions, vec!["v0.6.3", "v0.6.4", "v0.6.5"]);

    // Without regex the untagged-style names come through too.
    let cfg_all = DockerRegistryCfg {
        tag_regex: None,
        ..cfg
    };
    let rels = DockerRegistry
        .check(
            &app(SourceKind::DockerRegistry(cfg_all)),
            &net::build_client().unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(rels.len(), 4);
}
