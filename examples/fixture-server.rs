//! Deterministic HTTP fixture for provider/installer/e2e tests.
//!
//! Run standalone (`cargo run --example fixture-server`): binds 127.0.0.1:0 and
//! prints `BASE_URL=http://127.0.0.1:<port>` as the FIRST stdout line, which
//! tests/common/mod.rs parses. Serves:
//!
//!   GET /gh/repos/{owner}/{repo}/releases?page=N   GitHub-style JSON (page 2 empty;
//!                                                  one draft, one prerelease,
//!                                                  inline digests + a SHA256SUMS asset)
//!   GET /dl/<file>                                 arbitrary known bytes (+ .sha256 sidecars)
//!   GET /site/                                     HTML page with three AppImage hrefs
//!   GET /site/files/<file>                         site-relative downloads + sidecars
//!   GET /v2/auth                                   docker token endpoint
//!   GET /v2/{*repo}/tags/list                      docker tag list

use axum::Router;
use axum::extract::{Path, Query};
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use sha2::{Digest as _, Sha256};
use std::collections::HashMap;
use std::sync::OnceLock;

static BASE: OnceLock<String> = OnceLock::new();

// Deterministic payload bytes.
const APP_210: &[u8] = b"fixture-appimage-bytes-2.1.0";
const APP_200: &[u8] = b"fixture-appimage-bytes-2.0.0-prerelease";
const APP_180: &[u8] = b"fixture-appimage-bytes-1.8.0";
const FOO_12: &[u8] = b"foo-site-download-2024.06.12";
const FOO_11: &[u8] = b"foo-site-download-2024.06.11";

fn sha(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

struct Files {
    sums: String,
    h210: String,
    h200: String,
    foo12_sidecar: String,
    foo11_sidecar: String,
    /// Valid-format sidecar carrying a WRONG hash: digest-gate tests.
    poisoned_sidecar: String,
}

const POISONED_APP: &[u8] = b"poisoned-appimage-bytes-2025.99.99";
const DECOY: &[u8] = b"totally-different-bytes";

fn files() -> Files {
    Files {
        h210: sha(APP_210),
        h200: sha(APP_200),
        sums: format!("{}  app-1.8.0-x86_64.AppImage\n", sha(APP_180)),
        foo12_sidecar: format!("{}  Foo-2024.06.12-x86_64.AppImage\n", sha(FOO_12)),
        foo11_sidecar: format!("{}  Foo-2024.06.11-x86_64.AppImage\n", sha(FOO_11)),
        poisoned_sidecar: format!("{}  Foo-2025.99.99-poisoned-x86_64.AppImage\n", sha(DECOY)),
    }
}

fn dl_url(name: &str) -> String {
    format!("{}/dl/{name}", BASE.get().unwrap())
}

async fn gh_releases(Query(q): Query<HashMap<String, String>>) -> Response {
    let f = files();
    let page = q.get("page").map(|s| s.as_str()).unwrap_or("1");
    let body = if page == "1" {
        serde_json::json!([
            {
                "tag_name": "v2.1.0",
                "name": "Demo 2.1.0",
                "draft": false,
                "prerelease": false,
                "published_at": "2026-01-03T10:00:00Z",
                "assets": [
                    {
                        "name": "freecad-demo-2.1.0-x86_64.AppImage",
                        "browser_download_url": dl_url("freecad-demo-2.1.0-x86_64.AppImage"),
                        "size": APP_210.len(),
                        "digest": format!("sha256:{}", f.h210)
                    },
                    {
                        "name": "SHA256SUMS",
                        "browser_download_url": dl_url("SHA256SUMS"),
                        "size": f.sums.len()
                    }
                ]
            },
            {
                "tag_name": "v2.0.0",
                "name": "Demo 2.0.0 (prerelease)",
                "draft": false,
                "prerelease": true,
                "published_at": "2025-12-01T10:00:00Z",
                "assets": [
                    {
                        "name": "app-2.0.0-x86_64.AppImage",
                        "browser_download_url": dl_url("app-2.0.0-x86_64.AppImage"),
                        "size": APP_200.len(),
                        "digest": format!("sha256:{}", f.h200)
                    }
                ]
            },
            {
                "tag_name": "v1.9.0",
                "name": null,
                "draft": true,
                "prerelease": false,
                "published_at": null,
                "assets": [
                    {
                        "name": "app-1.9.0-draft.AppImage",
                        "browser_download_url": dl_url("app-1.9.0-draft.AppImage"),
                        "size": 10
                    }
                ]
            },
            {
                "tag_name": "v1.8.0",
                "name": "Demo 1.8.0",
                "draft": false,
                "prerelease": false,
                "published_at": "2025-06-15T09:30:00Z",
                "assets": [
                    {
                        "name": "app-1.8.0-x86_64.AppImage",
                        "browser_download_url": dl_url("app-1.8.0-x86_64.AppImage"),
                        "size": APP_180.len()
                    },
                    {
                        "name": "app-1.8.0-unfiltered.txt",
                        "browser_download_url": dl_url("app-1.8.0-unfiltered.txt"),
                        "size": 5
                    },
                    {
                        "name": "SHA256SUMS",
                        "browser_download_url": dl_url("SHA256SUMS"),
                        "size": f.sums.len()
                    }
                ]
            }
        ])
        .to_string()
    } else {
        "[]".to_string()
    };
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

async fn dl(Path(name): Path<String>) -> Response {
    let f = files();
    let body: Vec<u8> = match name.as_str() {
        "freecad-demo-2.1.0-x86_64.AppImage" => APP_210.to_vec(),
        "app-2.0.0-x86_64.AppImage" => APP_200.to_vec(),
        "app-1.8.0-x86_64.AppImage" => APP_180.to_vec(),
        "Foo-2024.06.11-x86_64.AppImage" => FOO_11.to_vec(),
        "SHA256SUMS" => f.sums.into_bytes(),
        "Foo-2024.06.11-x86_64.AppImage.sha256" => f.foo11_sidecar.into_bytes(),
        _ => return (StatusCode::NOT_FOUND, format!("no fixture file {name:?}")).into_response(),
    };
    (
        [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
        body,
    )
        .into_response()
}

async fn site_files(Path(name): Path<String>) -> Response {
    let f = files();
    let body: Vec<u8> = match name.as_str() {
        "Foo-2024.06.12-x86_64.AppImage" => FOO_12.to_vec(),
        "Foo-2024.06.12-x86_64.AppImage.sha256" => f.foo12_sidecar.into_bytes(),
        "Foo-2025.99.99-poisoned-x86_64.AppImage" => POISONED_APP.to_vec(),
        "Foo-2025.99.99-poisoned-x86_64.AppImage.sha256" => f.poisoned_sidecar.into_bytes(),
        _ => return (StatusCode::NOT_FOUND, format!("no site file {name:?}")).into_response(),
    };
    (
        [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
        body,
    )
        .into_response()
}

/// Streams `bytes` count in 1 KiB chunks, 50 ms apart: for deadline tests.
async fn slow(Path(bytes): Path<String>) -> Response {
    use futures::StreamExt as _;
    let mut total: usize = bytes.parse().unwrap_or(0);
    let stream = futures::stream::repeat_with(|| vec![b'x'; 1024]).take_while(move |_| {
        let done = total == 0;
        total = total.saturating_sub(1024);
        async move { !done }
    });
    // Sleep between chunks by zipping through an interval stream.
    let paced = stream.then(|chunk| async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        Ok::<_, std::io::Error>(chunk)
    });
    axum::body::Body::from_stream(paced).into_response()
}

async fn site() -> Response {
    // Three hrefs: one path-relative (resolves under /site/), one root-absolute,
    // one bare-relative. Sidecars exist for the first two only.
    let html = r##"<html><head><title>Downloads</title></head><body>
<h1>Foo downloads</h1>
<a href="files/Foo-2024.06.12-x86_64.AppImage">Foo latest</a>
<a href="/dl/Foo-2024.06.11-x86_64.AppImage">Foo previous</a>
<a href="Foo-2024.06.02-x86_64.AppImage">Foo ancient</a>
<a href="files/Foo-2025.99.99-poisoned-x86_64.AppImage">Foo future (bad checksum)</a>
<a href="docs/readme.md">not an appimage</a>
</body></html>
"##;
    ([(axum::http::header::CONTENT_TYPE, "text/html")], html).into_response()
}

async fn v2_auth() -> Response {
    let body = serde_json::json!({"token": "fixture-token", "expires_in": 300});
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

/// Docker Registry v2 tag list. Path shape: /v2/<repo>/tags/list
/// Supports `n` + `last` query params with a Link header for pagination.
/// axum requires catch-alls at the end of a route, so `/v2/<repo>/tags/list`
/// is dispatched from here.
async fn v2_catchall(Path(rest): Path<String>, uri: Uri) -> Response {
    // Hub-style: /v2/repositories/<repo>/tags?page=N
    if let Some(repo) = rest
        .strip_prefix("repositories/")
        .and_then(|r| r.strip_suffix("/tags"))
    {
        return hub_tags(repo.to_string(), uri).await;
    }
    // OCI-style: /v2/<repo>/tags/list
    let Some(repo) = rest.strip_suffix("/tags/list") else {
        return (
            StatusCode::NOT_FOUND,
            format!("unknown fixture path {rest:?}"),
        )
            .into_response();
    };
    tags_list(repo.to_string(), uri).await
}

/// Docker Hub style listing: JSON `results` + `next` URL, paginated by `page`.
/// Two pages for library/nginx to exercise next-following.
async fn hub_tags(repo: String, uri: Uri) -> Response {
    let q: HashMap<String, String> = uri.query().map(url_query).unwrap_or_default();
    let page: u32 = q.get("page").and_then(|v| v.parse().ok()).unwrap_or(1);

    let (names, next): (Vec<&str>, Option<String>) = match repo.as_str() {
        "library/nginx" => match page {
            1 => (
                vec!["1.27.4", "1.27.3"],
                Some(format!(
                    "{}/v2/repositories/library/nginx/tags?page_size=100&page=2",
                    BASE.get().unwrap()
                )),
            ),
            _ => (vec!["1.26.3", "latest"], None),
        },
        other => (
            vec![format!("no-hub-tags-for-{other}").leak() as &str],
            None,
        ),
    };

    let body = serde_json::json!({
        "next": next,
        "results": names.into_iter().map(|n| serde_json::json!({"name": n})).collect::<Vec<_>>()
    })
    .to_string();
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

async fn tags_list(repo: String, uri: Uri) -> Response {
    let tags_for = |name: &str| -> Vec<String> {
        match name {
            "library/nginx" => vec![
                "1.27.4".into(),
                "1.27.3".into(),
                "1.27.2".into(),
                "1.26.3".into(),
                "1.25.5".into(),
                "latest".into(),
            ],
            "open-webui/open-webui" => vec![
                "v0.6.5".into(),
                "v0.6.4".into(),
                "v0.6.3".into(),
                "main".into(),
            ],
            other => vec![format!("no-tags-for-{other}")],
        }
    };

    let repo_name = repo.trim_end_matches('/');
    let all = tags_for(repo_name);

    let q: HashMap<String, String> = uri.query().map(url_query).unwrap_or_default();
    let chunk = if repo_name == "open-webui/open-webui" {
        2
    } else {
        usize::MAX
    };
    let n: usize = q.get("n").and_then(|v| v.parse().ok()).unwrap_or(chunk);
    let last_idx = match q.get("last") {
        Some(l) => all.iter().position(|t| t == l).map(|i| i + 1).unwrap_or(0),
        None => 0,
    };
    let slice: Vec<String> = all.iter().skip(last_idx).take(n.max(1)).cloned().collect();

    let end = last_idx + slice.len();
    let body = serde_json::json!({"name": repo_name, "tags": slice}).to_string();
    let mut builder = axum::http::Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "application/json");
    if end < all.len() {
        let next = format!(
            "<{}/v2/{}/tags/list?n={}&last={}>; rel=\"next\"",
            BASE.get().unwrap(),
            repo_name,
            slice.len().max(1),
            slice.last().unwrap_or(&String::new())
        );
        builder = builder.header(axum::http::header::LINK, next);
    }
    builder.body(body).unwrap().into_response()
}

fn url_query(qs: &str) -> HashMap<String, String> {
    qs.split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/gh/repos/{owner}/{repo}/releases", get(gh_releases))
        .route("/dl/{*name}", get(dl))
        .route("/slow/{bytes}", get(slow))
        .route("/site/", get(site))
        .route("/site/files/{*name}", get(site_files))
        .route("/v2/auth", get(v2_auth))
        .route("/v2/{*rest}", get(v2_catchall));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    BASE.set(format!("http://{addr}")).unwrap();
    println!("BASE_URL=http://{addr}");
    use std::io::Write;
    std::io::stdout().flush().unwrap();

    axum::serve(listener, app).await.unwrap();
}
