//! End-to-end proxy routing through configured engines, without external traffic.
use std::path::Path;
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use updatium::engine::{Engine, Status, UpdateOpts};

const ASSET: &[u8] = b"proxy integration test appimage";

#[derive(Clone, Copy)]
enum Protocol {
    Http,
    Socks5,
}

struct Proxy {
    url: String,
    requests: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Proxy {
    async fn start(protocol: Protocol) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let task = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let captured = captured.clone();
                tokio::spawn(async move {
                    serve(stream, protocol, captured).await;
                });
            }
        });
        let scheme = match protocol {
            Protocol::Http => "http",
            Protocol::Socks5 => "socks5",
        };
        Self { url: format!("{scheme}://{address}"), requests, task }
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

async fn serve(mut stream: TcpStream, protocol: Protocol, requests: Arc<Mutex<Vec<String>>>) {
    if matches!(protocol, Protocol::Socks5) {
        let mut greeting = [0; 2];
        stream.read_exact(&mut greeting).await.unwrap();
        assert_eq!(greeting[0], 5);
        let mut methods = vec![0; greeting[1] as usize];
        stream.read_exact(&mut methods).await.unwrap();
        assert!(methods.contains(&0));
        stream.write_all(&[5, 0]).await.unwrap();
        let mut request = [0; 4];
        stream.read_exact(&mut request).await.unwrap();
        assert_eq!(&request[..3], &[5, 1, 0]);
        let address_len = match request[3] {
            1 => 4,
            4 => 16,
            3 => stream.read_u8().await.unwrap() as usize,
            other => panic!("unexpected SOCKS address type {other}"),
        };
        let mut address = vec![0; address_len + 2];
        stream.read_exact(&mut address).await.unwrap();
        stream.write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0]).await.unwrap();
    }

    let mut request = Vec::new();
    loop {
        let byte = stream.read_u8().await.unwrap();
        request.push(byte);
        if request.ends_with(b"\r\n\r\n") { break; }
        assert!(request.len() < 65536);
    }
    let first = String::from_utf8(request).unwrap().lines().next().unwrap().to_owned();
    requests.lock().unwrap().push(first.clone());
    let body = if first.contains("/demo.AppImage") {
        ASSET.to_vec()
    } else if first.contains("page=1") {
        let digest = hex::encode(Sha256::digest(ASSET));
        format!(r#"[{{"tag_name":"v1.2.3","assets":[{{"name":"demo.AppImage","browser_download_url":"http://127.0.0.1:9/demo.AppImage","size":{},"digest":"sha256:{}"}}]}}]"#, ASSET.len(), digest).into_bytes()
    } else {
        b"[]".to_vec()
    };
    let header = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
    stream.write_all(header.as_bytes()).await.unwrap();
    stream.write_all(&body).await.unwrap();
}

fn write_config(home: &Path, global: &str, app_proxy: Option<&str>) {
    let config = home.join("config");
    std::fs::create_dir_all(config.join("apps.d")).unwrap();
    std::fs::write(config.join("config.toml"), format!("proxy = {global:?}\n")).unwrap();
    let proxy = app_proxy.map(|p| format!("proxy = {p:?}\n")).unwrap_or_default();
    std::fs::write(config.join("apps.d/demo.toml"), format!(r#"
id = "demo"
provider = "github-releases"
allow_insecure_http = true
repo = "demo/app"
asset_regex = '\.AppImage$'
digest_policy = "required"
api_base = "http://127.0.0.1:9"
{proxy}
[install]
type = "appimage"
[install.desktop]
name = "Demo"
"#)).unwrap();
}

async fn check_and_update(home: &Path) {
    let engine = Engine::new(Some(home)).unwrap();
    let reports = engine.check(None).await.unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].latest.as_deref(), Some("1.2.3"), "{:?}", reports[0]);
    let report = engine.update("demo", &UpdateOpts::default()).await.unwrap();
    assert!(report.failed.is_none(), "{report:?}");
    assert_eq!(std::fs::read(home.join("state/Applications/demo.AppImage")).unwrap(), ASSET);
}

fn assert_check_and_download(requests: &[String]) {
    assert!(requests.iter().any(|r| r.contains("/repos/demo/app/releases")), "{requests:?}");
    assert!(requests.iter().any(|r| r.contains("/demo.AppImage")), "{requests:?}");
}

#[tokio::test]
async fn http_global_fallback_and_socks_app_override_route_checks_and_downloads() {
    let global = Proxy::start(Protocol::Http).await;
    let app = Proxy::start(Protocol::Socks5).await;
    let fallback_home = tempfile::tempdir().unwrap();
    write_config(fallback_home.path(), &global.url, None);
    check_and_update(fallback_home.path()).await;
    assert_check_and_download(&global.requests());

    let global_before = global.requests();
    let override_home = tempfile::tempdir().unwrap();
    write_config(override_home.path(), &global.url, Some(&app.url));
    check_and_update(override_home.path()).await;
    assert_check_and_download(&app.requests());
    assert_eq!(global.requests(), global_before, "app proxy must override global proxy");
}

#[tokio::test]
async fn socks_global_fallback_and_http_app_override_route_checks_and_downloads() {
    let global = Proxy::start(Protocol::Socks5).await;
    let app = Proxy::start(Protocol::Http).await;
    let fallback_home = tempfile::tempdir().unwrap();
    write_config(fallback_home.path(), &global.url, None);
    check_and_update(fallback_home.path()).await;
    assert_check_and_download(&global.requests());

    let global_before = global.requests();
    let override_home = tempfile::tempdir().unwrap();
    write_config(override_home.path(), &global.url, Some(&app.url));
    check_and_update(override_home.path()).await;
    assert_check_and_download(&app.requests());
    assert_eq!(global.requests(), global_before, "app proxy must override global proxy");
}

#[tokio::test]
async fn invalid_proxy_is_reported_for_global_and_app_configuration() {
    for app_specific in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let invalid = "http://[invalid";
        let valid = "http://127.0.0.1:9";
        write_config(home.path(), if app_specific { valid } else { invalid }, app_specific.then_some(invalid));
        match Engine::new(Some(home.path())) {
            Err(error) => assert!(format!("{error:#}").to_lowercase().contains("proxy"), "{error:#}"),
            Ok(engine) => match engine.check(None).await {
                Err(error) => assert!(format!("{error:#}").to_lowercase().contains("proxy"), "{error:#}"),
                Ok(reports) => {
                    assert_eq!(reports.len(), 1);
                    assert!(matches!(&reports[0].status, Status::Error(message) if message.to_lowercase().contains("proxy")), "{:?}", reports[0]);
                }
            },
        }
    }
}
