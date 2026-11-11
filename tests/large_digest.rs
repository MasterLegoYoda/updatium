use axum::{body::{Body, Bytes}, extract::Path, routing::get, Router};
use sha2::{Digest, Sha256};
use updatium::engine::{Engine, UpdateOpts};

const SIZE: usize = 32 * 1024 * 1024;
const CHUNK: [u8; 16 * 1024] = [0; 16 * 1024];
const EXPECTED: &str = "83ee47245398adee79bd9c0a8bc57b821e92aba10f5f9ade8a5d1fae4d8c4302";

#[tokio::test]
async fn large_asset_digest_gates_installation_and_records_mismatch() {
    let app = Router::new()
        .route("/{mode}/", get(|| async { r#"<a href="large-1.0.0.AppImage">download</a>"# }))
        .route("/{mode}/large-1.0.0.AppImage.sha256", get(|Path(mode): Path<String>| async move {
            if mode == "bad" { "f".repeat(64) } else { EXPECTED.to_owned() }
        }))
        .route("/{mode}/large-1.0.0.AppImage", get(|| async {
            let chunks = (0..SIZE / CHUNK.len()).map(|_| Ok::<_, std::io::Error>(Bytes::from_static(&CHUNK)));
            Body::from_stream(futures::stream::iter(chunks))
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let home = tempfile::tempdir().unwrap();
    let apps = home.path().join("config/apps.d");
    std::fs::create_dir_all(&apps).unwrap();
    for id in ["ok", "bad"] {
        std::fs::write(apps.join(format!("{id}.toml")), format!(r#"
id = "{id}"
provider = "html-links"
url = "http://{addr}/{id}/"
link_regex = 'href="(?P<href>[^"]+\.AppImage)"'
version_capture = '(?P<version>\d+\.\d+\.\d+)'
digest_suffix = ".sha256"
digest_policy = "required"
allow_insecure_http = true
[install]
type = "appimage"
"#)).unwrap();
    }
    let engine = Engine::new(Some(home.path())).unwrap();
    let ok = engine.update("ok", &UpdateOpts::default()).await.unwrap();
    assert!(ok.installed && ok.failed.is_none(), "{ok:?}");
    assert!(ok.actions.iter().any(|action| action == "digest verified large-1.0.0.AppImage"));
    let installed = home.path().join("state/Applications/large-1.0.0.AppImage");
    use std::io::Read as _;
    let mut installed_file = std::fs::File::open(&installed).unwrap();
    assert_eq!(installed_file.metadata().unwrap().len(), SIZE as u64);
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 32 * 1024];
    loop {
        let n = installed_file.read(&mut buffer).unwrap();
        if n == 0 { break; }
        hash.update(&buffer[..n]);
    }
    assert_eq!(hex::encode(hash.finalize()), EXPECTED);

    let bad = engine.update("bad", &UpdateOpts::default()).await.unwrap();
    let failure = bad.failed.as_deref().unwrap();
    assert!(failure.contains("digest mismatch"), "{failure}");
    assert!(!bad.installed);
    assert!(engine.installed_version("bad").await.unwrap().is_none());
    let events = engine.history(Some("bad"), 100).unwrap();
    let detail = events.iter().find(|event| event.kind == "verify_failed").unwrap().detail.as_ref().unwrap();
    let detail: serde_json::Value = serde_json::from_str(detail).unwrap();
    assert_eq!(detail["expected"], format!("sha256:{}", "f".repeat(64)));
    assert_eq!(detail["actual"], format!("sha256:{EXPECTED}"));
    assert_eq!(detail["asset"], "large-1.0.0.AppImage");
    assert_eq!(std::fs::metadata(&installed).unwrap().len(), SIZE as u64);
    server.abort();
}
