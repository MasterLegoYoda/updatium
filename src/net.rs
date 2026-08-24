//! Shared HTTP client and streaming downloader with size caps.

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Build the process-wide reqwest client. rustls-only, no default features.
pub fn build_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("updatium/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(30))
        // Per-request timeouts are applied at the call site (download vs check).
        .build()
        .context("building HTTP client")
}

#[derive(Debug, Clone)]
pub struct DownloadOpts {
    /// hard mid-stream cap (max_download_mb from global config)
    pub max_bytes: u64,
    /// whole-download deadline (download_timeout_secs from global config)
    pub timeout: Duration,
    /// allow plain http:// URLs (app-level opt-in)
    pub allow_insecure_http: bool,
}

fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() || cleaned.chars().all(|c| c == '.') {
        "download.bin".into()
    } else {
        cleaned
    }
}

/// Unique-enough temp suffix without pulling a rand crate.
fn tmp_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "{:x}{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        n ^ (std::process::id() as u64) << 32
    )
}

/// Stream `url` into `<downloads>/tmp-<rand>`, then atomically rename to the
/// sanitized final filename derived from the URL. Enforces HTTPS unless
/// `allow_insecure_http`, aborts when the stream exceeds `max_bytes` mid-flight,
/// and applies `timeout` to the entire download.
///
/// Returns the final file path.
pub async fn download(
    http: &reqwest::Client,
    url: &url::Url,
    downloads_dir: &Path,
    opts: &DownloadOpts,
) -> Result<PathBuf> {
    match url.scheme() {
        "https" => {}
        "http" if opts.allow_insecure_http => {}
        other => bail!(
            "refusing non-HTTPS URL ({other}:) — set allow_insecure_http = true on the app to permit http"
        ),
    }

    tokio::time::timeout(
        opts.timeout,
        download_inner(http, url, downloads_dir, opts.max_bytes),
    )
    .await
    .map_err(|_| anyhow::anyhow!("download timed out after {:?}", opts.timeout))?
}

async fn download_inner(
    http: &reqwest::Client,
    url: &url::Url,
    downloads_dir: &Path,
    max_bytes: u64,
) -> Result<PathBuf> {
    tokio::fs::create_dir_all(downloads_dir)
        .await
        .with_context(|| format!("creating {}", downloads_dir.display()))?;

    let resp = http
        .get(url.clone())
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        bail!("GET {url} failed: HTTP {status}");
    }

    // Content-Length pre-check when available.
    if let Some(len) = resp.content_length() {
        if len > max_bytes {
            bail!(
                "download of {url} is {} bytes, exceeding max_download_mb cap of {} bytes",
                len,
                max_bytes
            );
        }
    }

    let tmp_path = downloads_dir.join(format!("tmp-{}", tmp_suffix()));
    let mut file = tokio::io::BufWriter::new(
        tokio::fs::File::create(&tmp_path)
            .await
            .with_context(|| format!("creating {}", tmp_path.display()))?,
    );
    use tokio::io::AsyncWriteExt;

    let mut stream = resp.bytes_stream();
    let mut total: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("reading stream from {url}"))?;
        total += chunk.len() as u64;
        if total > max_bytes {
            drop(file);
            let _ = tokio::fs::remove_file(&tmp_path).await;
            bail!(
                "download of {url} exceeded max_download_mb cap mid-stream (>{} bytes)",
                max_bytes
            );
        }
        file.write_all(&chunk)
            .await
            .with_context(|| format!("writing {}", tmp_path.display()))?;
    }
    file.flush().await.context("flushing download")?;
    drop(file);

    let final_name = url
        .path_segments()
        .and_then(|segs| segs.rev().find(|s| !s.is_empty()))
        .map(sanitize_filename)
        .unwrap_or_else(|| "download.bin".into());
    let dest = downloads_dir.join(final_name);
    tokio::fs::rename(&tmp_path, &dest)
        .await
        .with_context(|| format!("renaming into {}", dest.display()))?;
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_hostile_filenames() {
        assert_eq!(
            sanitize_filename("FreeCAD-1.0.AppImage"),
            "FreeCAD-1.0.AppImage"
        );
        assert_eq!(sanitize_filename("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(sanitize_filename(""), "download.bin");
        assert_eq!(sanitize_filename("..."), "download.bin");
        assert_eq!(sanitize_filename("a b/c"), "a_b_c");
    }
}
