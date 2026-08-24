//! Provider tests against the local fixture server.
mod common;

use sha2::{Digest as _, Sha256};
use updatium::config::{AppSource, GithubReleasesCfg, HtmlLinksCfg, SourceKind};
use updatium::net;
use updatium::provider::github_releases::GithubReleases;
use updatium::provider::html_links::HtmlLinks;
use updatium::provider::{Provider, Release};

fn app(kind: SourceKind, insecure_http: bool) -> AppSource {
    AppSource {
        allow_insecure_http: insecure_http,
        kind,
    }
}

fn sha(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

fn gh_cfg(base: &str, include_prereleases: bool) -> GithubReleasesCfg {
    GithubReleasesCfg {
        repo: "demo/app".into(),
        tag_regex: None,
        asset_regex: Some(r"\.AppImage$".into()),
        include_prereleases,
        api_base: format!("{base}/gh"),
    }
}

async fn gh_check(base: &str, include_prereleases: bool) -> anyhow::Result<Vec<Release>> {
    let http = net::build_client()?;
    let p = GithubReleases::new(vec![]);
    p.check(
        &app(
            SourceKind::GithubReleases(gh_cfg(base, include_prereleases)),
            false,
        ),
        &http,
    )
    .await
}

#[tokio::test]
async fn github_pagination_filters_and_digests() {
    let (base, _guard) = common::spawn().await;

    let rels = gh_check(&base, false).await.unwrap();
    let mut versions: Vec<&str> = rels.iter().map(|r| r.version.as_str()).collect();
    versions.sort();
    assert_eq!(
        versions,
        vec!["1.8.0", "2.1.0"],
        "draft+prerelease filtered"
    );

    // Prerelease included when flagged; draft still excluded.
    let rels_pre = gh_check(&base, true).await.unwrap();
    let mut versions: Vec<&str> = rels_pre.iter().map(|r| r.version.as_str()).collect();
    versions.sort();
    assert_eq!(versions, vec!["1.8.0", "2.0.0", "2.1.0"]);

    // Digest from inline `digest` field.
    let r210 = rels.iter().find(|r| r.version == "2.1.0").unwrap();
    assert_eq!(r210.assets.len(), 1, "asset_regex keeps only AppImages");
    assert_eq!(
        r210.assets[0].digest_hex(),
        Some(sha(b"fixture-appimage-bytes-2.1.0").as_str())
    );
    assert_eq!(
        r210.published_at.map(|d| d.to_rfc3339()).as_deref(),
        Some("2026-01-03T10:00:00+00:00")
    );

    // Digest from sibling SHA256SUMS asset (no inline digest).
    let r180 = rels.iter().find(|r| r.version == "1.8.0").unwrap();
    assert_eq!(
        r180.assets[0].digest_hex(),
        Some(sha(b"fixture-appimage-bytes-1.8.0").as_str())
    );

    // Pagination: page 2 exists and is empty; provider must terminate cleanly
    // (implicit) and page-2-only tags never appear (explicit).
    assert!(!rels.iter().any(|r| r.version.contains("page2")));
}

#[tokio::test]
async fn github_regex_no_match_yields_empty() {
    let (base, _guard) = common::spawn().await;
    let cfg = GithubReleasesCfg {
        tag_regex: Some(r"^does-not-match-anything$".into()),
        ..gh_cfg(&base, false)
    };
    let http = net::build_client().unwrap();
    let p = GithubReleases::new(vec![]);
    let rels = p
        .check(&app(SourceKind::GithubReleases(cfg), false), &http)
        .await
        .unwrap();
    assert!(
        rels.is_empty(),
        "regex-no-match must be empty vec, not error"
    );
}

#[tokio::test]
async fn html_links_scrapes_and_resolves() {
    let (base, _guard) = common::spawn().await;
    let cfg = HtmlLinksCfg {
        url: format!("{base}/site/"),
        link_regex: r#"href="(?P<href>[^"]+\.AppImage)""#.into(),
        version_capture: r"(?P<version>\d+\.\d+(\.\d+)?)".into(),
        base_url: None,
        digest_suffix: Some(".sha256".into()),
    };
    let http = net::build_client().unwrap();
    let p = HtmlLinks;
    // Fixture serves plain http; the app opts in via allow_insecure_http.
    let rels = p
        .check(&app(SourceKind::HtmlLinks(cfg.clone()), true), &http)
        .await
        .unwrap();

    assert_eq!(rels.len(), 4, "four AppImage hrefs found");
    let by_v = |v: &str| rels.iter().find(|r| r.version == v).unwrap();

    // Relative href resolved against page URL.
    let newest = by_v("2024.06.12");
    assert!(
        newest.assets[0]
            .url
            .as_str()
            .starts_with(&format!("{base}/site/files/")),
        "relative href must resolve under /site/: {}",
        newest.assets[0].url
    );
    assert_eq!(
        newest.assets[0].digest_hex(),
        Some(sha(b"foo-site-download-2024.06.12").as_str())
    );

    // Root-absolute href + sidecar digest.
    let prev = by_v("2024.06.11");
    assert_eq!(
        prev.assets[0].digest_hex(),
        Some(sha(b"foo-site-download-2024.06.11").as_str())
    );

    // Poisoned sidecar parses (format-valid) but carries the decoy hash.
    let poisoned = by_v("2025.99.99");
    assert_eq!(
        poisoned.assets[0].digest_hex(),
        Some(sha(b"totally-different-bytes").as_str())
    );

    // Missing sidecar = no digest, not an error.
    let old = by_v("2024.06.02");
    assert!(old.assets[0].digest.is_none());

    // Non-matching version capture links skipped silently (docs/readme.md absent).
    assert!(!rels.iter().any(|r| r.version.is_empty()));
}

#[tokio::test]
async fn html_links_requires_https_unless_flagged() {
    let (base, _guard) = common::spawn().await;
    let cfg = HtmlLinksCfg {
        url: format!("{base}/site/"),
        link_regex: r#"href="(?P<href>[^"]+)""#.into(),
        version_capture: r"(?P<version>\d+\.\d+)".into(),
        base_url: None,
        digest_suffix: None,
    };
    let http = net::build_client().unwrap();
    let p = HtmlLinks;

    // Fixture serves plain http -> refused without the flag...
    let res = p
        .check(&app(SourceKind::HtmlLinks(cfg.clone()), false), &http)
        .await;
    assert!(res.is_err());
    assert!(format!("{:#}", res.unwrap_err()).contains("allow_insecure_http"));

    // ...allowed with it.
    let rels = p
        .check(&app(SourceKind::HtmlLinks(cfg), true), &http)
        .await
        .unwrap();
    assert!(!rels.is_empty());
}
