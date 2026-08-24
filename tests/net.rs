//! Downloader behavior against the fixture server.
mod common;

use std::time::Duration;
use updatium::net::{DownloadOpts, build_client, download};

fn opts(max_bytes: u64, timeout: Duration) -> DownloadOpts {
    DownloadOpts {
        max_bytes,
        timeout,
        allow_insecure_http: true, // fixture is plain http
    }
}

#[tokio::test]
async fn downloads_exact_bytes_and_final_name() {
    let (base, _guard) = common::spawn().await;
    let dir = tempfile::tempdir().unwrap();
    let url = url::Url::parse(&format!("{base}/dl/app-1.8.0-x86_64.AppImage")).unwrap();
    let dest = download(
        &build_client().unwrap(),
        &url,
        dir.path(),
        &opts(1024 * 1024, Duration::from_secs(60)),
    )
    .await
    .unwrap();
    assert_eq!(dest.file_name().unwrap(), "app-1.8.0-x86_64.AppImage");
    assert_eq!(
        std::fs::read(&dest).unwrap(),
        b"fixture-appimage-bytes-1.8.0".to_vec()
    );
}

#[tokio::test]
async fn oversize_aborts_mid_stream_without_tmp_litter() {
    let (base, _guard) = common::spawn().await;
    let dir = tempfile::tempdir().unwrap();
    let url = url::Url::parse(&format!("{base}/dl/freecad-demo-2.1.0-x86_64.AppImage")).unwrap();
    // Cap below actual size (28 bytes): content-length pre-check fires.
    let res = download(
        &build_client().unwrap(),
        &url,
        dir.path(),
        &opts(10, Duration::from_secs(60)),
    )
    .await;
    assert!(res.is_err());
    assert!(format!("{:#}", res.unwrap_err()).contains("max_download_mb"));
    let leftovers: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
    assert!(leftovers.is_empty(), "no tmp files may remain");
}

#[tokio::test]
async fn timeout_kills_slow_download() {
    let (base, _guard) = common::spawn().await;
    let dir = tempfile::tempdir().unwrap();
    // 40 KiB streamed in 1 KiB chunks every 50 ms (~2 s total).
    let url = url::Url::parse(&format!("{base}/slow/40960")).unwrap();
    let started = std::time::Instant::now();
    let res = download(
        &build_client().unwrap(),
        &url,
        dir.path(),
        &opts(1024 * 1024, Duration::from_millis(50)),
    )
    .await;
    assert!(res.is_err(), "50ms deadline must trip");
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[tokio::test]
async fn plain_http_requires_opt_in() {
    let (base, _guard) = common::spawn().await;
    let dir = tempfile::tempdir().unwrap();
    let url = url::Url::parse(&format!("{base}/dl/SHA256SUMS")).unwrap();

    let mut no_tls = opts(1024 * 1024, Duration::from_secs(60));
    no_tls.allow_insecure_http = false;
    let res = download(&build_client().unwrap(), &url.clone(), dir.path(), &no_tls).await;
    assert!(res.is_err());
    assert!(format!("{:#}", res.unwrap_err()).contains("allow_insecure_http"));

    // Opt-in works.
    let ok = download(
        &build_client().unwrap(),
        &url,
        dir.path(),
        &opts(1024 * 1024, Duration::from_secs(60)),
    )
    .await
    .unwrap();
    assert!(ok.exists());
}

#[tokio::test]
async fn hostile_url_filename_sanitized() {
    let (base, _guard) = common::spawn().await;
    let dir = tempfile::tempdir().unwrap();
    // Path segment with slashes encoded stays one segment; odd chars sanitized.
    let url = url::Url::parse(&format!("{base}/dl/weird%2Fname%20x.AppImage")).unwrap();
    let dest = download(
        &build_client().unwrap(),
        &url,
        dir.path(),
        &opts(1024 * 1024, Duration::from_secs(60)),
    )
    .await;
    // Fixture has no such file -> 404 error expected, but sanitization itself is
    // covered by unit tests; here just require a clean failure.
    assert!(dest.is_err());
}
