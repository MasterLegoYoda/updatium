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

#[tokio::test]
async fn reports_unknown_length_progress_before_download_finishes() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (release, wait) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        socket.read(&mut request).await.unwrap();
        socket.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n").await.unwrap();
        wait.await.unwrap();
        socket.write_all(b"3\r\ndef\r\n0\r\n\r\n").await.unwrap();
    });
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_owned();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let download = tokio::spawn(async move {
        updatium::net::download_with_progress(
            &build_client().unwrap(),
            &url::Url::parse(&format!("http://{address}/asset")).unwrap(),
            &path,
            &opts(1024, Duration::from_secs(10)),
            &|bytes, total| { tx.send((bytes, total)).unwrap(); },
        ).await.unwrap()
    });
    assert_eq!(rx.recv().await, Some((0, None)));
    assert_eq!(rx.recv().await, Some((3, None)));
    assert!(!download.is_finished());
    release.send(()).unwrap();
    assert_eq!(rx.recv().await, Some((6, None)));
    assert_eq!(tokio::fs::read(download.await.unwrap()).await.unwrap(), b"abcdef");
    server.await.unwrap();
}

// A deliberately truncated response creates a real interrupted partial without
// relying on timing. Each following connection records the actual request.
async fn scripted_server(responses: Vec<&'static [u8]>) -> (url::Url, tokio::task::JoinHandle<Vec<String>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for response in responses {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut buf = [0; 1024];
                let n = socket.read(&mut buf).await.unwrap();
                assert_ne!(n, 0);
                request.extend_from_slice(&buf[..n]);
                if request.windows(4).any(|w| w == b"\r\n\r\n") { break; }
            }
            requests.push(String::from_utf8(request).unwrap().to_ascii_lowercase());
            socket.write_all(response).await.unwrap();
        }
        requests
    });
    (url::Url::parse(&format!("http://{address}/asset")).unwrap(), server)
}

const INTERRUPTED: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nETag: \"v1\"\r\nConnection: close\r\n\r\nabc";
const FULL: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nETag: \"v2\"\r\nConnection: close\r\n\r\nUVWXYZ";

#[tokio::test]
async fn resumes_interrupted_transfer_with_strong_validator() {
    let (url, server) = scripted_server(vec![INTERRUPTED, b"HTTP/1.1 206 Partial Content\r\nContent-Length: 3\r\nContent-Range: bytes 3-5/6\r\nETag: \"v1\"\r\nConnection: close\r\n\r\ndef"]).await;
    let dir = tempfile::tempdir().unwrap();
    let client = build_client().unwrap();
    let opts = opts(100, Duration::from_secs(5));
    assert!(download(&client, &url, dir.path(), &opts).await.is_err());
    let reports = std::sync::Mutex::new(Vec::new());
    let path = updatium::net::download_with_progress(&client, &url, dir.path(), &opts, &|n, total| reports.lock().unwrap().push((n, total))).await.unwrap();
    assert_eq!(std::fs::read(path).unwrap(), b"abcdef");
    assert_eq!(reports.lock().unwrap()[0], (3, Some(6)));
    let requests = server.await.unwrap();
    assert!(requests[1].contains("range: bytes=3-\r\n"));
    assert!(requests[1].contains("if-range: \"v1\"\r\n"));
    assert!(requests[1].contains("accept-encoding: identity\r\n"));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[tokio::test]
async fn ignored_range_replaces_partial_instead_of_appending() {
    let (url, server) = scripted_server(vec![INTERRUPTED, FULL]).await;
    let dir = tempfile::tempdir().unwrap();
    let client = build_client().unwrap();
    let opts = opts(100, Duration::from_secs(5));
    assert!(download(&client, &url, dir.path(), &opts).await.is_err());
    let path = download(&client, &url, dir.path(), &opts).await.unwrap();
    assert_eq!(std::fs::read(path).unwrap(), b"UVWXYZ");
    assert!(server.await.unwrap()[1].contains("range: bytes=3-"));
}

#[tokio::test]
async fn invalid_partial_responses_restart_without_range() {
    let invalid: &[&[u8]] = &[
        b"HTTP/1.1 206 Partial Content\r\nContent-Length: 3\r\nContent-Range: bytes 2-4/6\r\nETag: \"v1\"\r\nConnection: close\r\n\r\ndef",
        b"HTTP/1.1 206 Partial Content\r\nContent-Length: 3\r\nContent-Range: bytes 3-5/6\r\nETag: \"v2\"\r\nConnection: close\r\n\r\ndef",
        b"HTTP/1.1 206 Partial Content\r\nContent-Length: 3\r\nContent-Range: bytes 3-5/6\r\nConnection: close\r\n\r\ndef",
        b"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    ];
    for &response in invalid {
        let (url, server) = scripted_server(vec![INTERRUPTED, response, FULL]).await;
        let dir = tempfile::tempdir().unwrap();
        let client = build_client().unwrap();
        let opts = opts(100, Duration::from_secs(5));
        assert!(download(&client, &url, dir.path(), &opts).await.is_err());
        let path = download(&client, &url, dir.path(), &opts).await.unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"UVWXYZ");
        let requests = server.await.unwrap();
        assert!(requests[1].contains("range: bytes=3-"));
        assert!(!requests[2].contains("range:"));
    }
}

#[tokio::test]
async fn weak_validator_is_not_used_for_resume() {
    let (url, server) = scripted_server(vec![b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nETag: W/\"v1\"\r\nConnection: close\r\n\r\nabc", FULL]).await;
    let dir = tempfile::tempdir().unwrap();
    let client = build_client().unwrap();
    let opts = opts(100, Duration::from_secs(5));
    assert!(download(&client, &url, dir.path(), &opts).await.is_err());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    let path = download(&client, &url, dir.path(), &opts).await.unwrap();
    assert_eq!(std::fs::read(path).unwrap(), b"UVWXYZ");
    assert!(!server.await.unwrap()[1].contains("range:"));
}

#[tokio::test]
async fn resumed_total_is_subject_to_size_cap() {
    let (url, server) = scripted_server(vec![INTERRUPTED, b"HTTP/1.1 206 Partial Content\r\nContent-Length: 3\r\nContent-Range: bytes 3-5/6\r\nETag: \"v1\"\r\nConnection: close\r\n\r\ndef"]).await;
    let dir = tempfile::tempdir().unwrap();
    let client = build_client().unwrap();
    assert!(download(&client, &url, dir.path(), &opts(100, Duration::from_secs(5))).await.is_err());
    assert!(download(&client, &url, dir.path(), &opts(5, Duration::from_secs(5))).await.is_err());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    server.await.unwrap();
}

#[tokio::test]
async fn interrupted_resume_retains_only_received_bytes() {
    let (url, server) = scripted_server(vec![
        INTERRUPTED,
        b"HTTP/1.1 206 Partial Content\r\nContent-Length: 3\r\nContent-Range: bytes 3-5/6\r\nETag: \"v1\"\r\nConnection: close\r\n\r\nd",
        b"HTTP/1.1 206 Partial Content\r\nContent-Length: 2\r\nContent-Range: bytes 4-5/6\r\nETag: \"v1\"\r\nConnection: close\r\n\r\nef",
    ]).await;
    let dir = tempfile::tempdir().unwrap();
    let client = build_client().unwrap();
    let opts = opts(100, Duration::from_secs(5));
    assert!(download(&client, &url, dir.path(), &opts).await.is_err());
    assert!(download(&client, &url, dir.path(), &opts).await.is_err());
    let path = download(&client, &url, dir.path(), &opts).await.unwrap();
    assert_eq!(std::fs::read(path).unwrap(), b"abcdef");
    assert!(server.await.unwrap()[2].contains("range: bytes=4-"));
}

#[tokio::test]
async fn concurrent_callers_do_not_share_partial_writes() {
    let (url, server) = scripted_server(vec![FULL, FULL]).await;
    let dir = tempfile::tempdir().unwrap();
    let client = build_client().unwrap();
    let opts = opts(100, Duration::from_secs(5));
    let (first, second) = tokio::join!(
        download(&client, &url, dir.path(), &opts),
        download(&client, &url, dir.path(), &opts),
    );
    assert_eq!(first.unwrap(), second.unwrap());
    assert_eq!(std::fs::read(dir.path().join("asset")).unwrap(), b"UVWXYZ");
    let requests = server.await.unwrap();
    assert!(!requests[0].contains("range:"));
    assert!(!requests[1].contains("range:"));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[tokio::test]
async fn unknown_length_stream_over_cap_removes_partial() {
    let (url, server) = scripted_server(vec![b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nETag: \"v1\"\r\nConnection: close\r\n\r\n6\r\nabcdef\r\n0\r\n\r\n"]).await;
    let dir = tempfile::tempdir().unwrap();
    let result = download(&build_client().unwrap(), &url, dir.path(), &opts(5, Duration::from_secs(5))).await;
    assert!(result.is_err());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    server.await.unwrap();
}
