//! Shared HTTP client and streaming downloader with size caps.

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use sha2::{Digest, Sha256};
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

/// Downloads are serialized per directory, including across processes. The
/// directory itself is locked so an interrupted process cannot leave a stale lock.
async fn lock_downloads(dir: &Path) -> Result<std::fs::File> {
    let dir = dir.to_owned();
    tokio::task::spawn_blocking(move || -> Result<std::fs::File> {
        let file = std::fs::File::open(&dir)
            .with_context(|| format!("opening {}", dir.display()))?;
        file.lock().with_context(|| format!("locking {}", dir.display()))?;
        Ok(file)
    }).await.context("joining download lock task")?
}

fn strong_etag(headers: &reqwest::header::HeaderMap) -> Option<String> {
    let mut values = headers.get_all(reqwest::header::ETAG).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() || value.len() < 2 || !value.starts_with('"')
        || !value.ends_with('"') || value[1..value.len() - 1].bytes().any(|b| b == b'"' || b < 0x21 || b == 0x7f)
    {
        return None;
    }
    Some(value.to_owned())
}

fn content_range(headers: &reqwest::header::HeaderMap) -> Option<(u64, u64, u64)> {
    let mut values = headers.get_all(reqwest::header::CONTENT_RANGE).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() { return None; }
    let (range, total) = value.strip_prefix("bytes ")?.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let (start, end, total) = (start.parse().ok()?, end.parse().ok()?, total.parse().ok()?);
    if start > end || end >= total { return None; }
    Some((start, end, total))
}

struct PartialCleanup {
    data: PathBuf,
    metadata: PathBuf,
    retain: bool,
}

impl Drop for PartialCleanup {
    fn drop(&mut self) {
        if !self.retain {
            let _ = std::fs::remove_file(&self.data);
            let _ = std::fs::remove_file(&self.metadata);
        }
    }
}

/// Stream `url` to a partial file and atomically rename it to the sanitized
/// final filename. Interrupted transfers with a strong ETag can be resumed.
/// Enforces HTTPS unless `allow_insecure_http`, enforces `max_bytes`, and
/// applies `timeout` to the entire download, including waiting for the lock.
///
/// Returns the final file path.
pub async fn download(
    http: &reqwest::Client,
    url: &url::Url,
    downloads_dir: &Path,
    opts: &DownloadOpts,
) -> Result<PathBuf> {
    download_with_progress(http, url, downloads_dir, opts, &|_, _| {}).await
}

/// Download an asset, reporting bytes written and the HTTP content length when known.
pub async fn download_with_progress(
    http: &reqwest::Client,
    url: &url::Url,
    downloads_dir: &Path,
    opts: &DownloadOpts,
    progress: &(dyn Fn(u64, Option<u64>) + Send + Sync),
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
        download_inner(http, url, downloads_dir, opts.max_bytes, progress),
    )
    .await
    .map_err(|_| anyhow::anyhow!("download timed out after {:?}", opts.timeout))?
}

async fn download_inner(
    http: &reqwest::Client,
    url: &url::Url,
    downloads_dir: &Path,
    max_bytes: u64,
    progress: &(dyn Fn(u64, Option<u64>) + Send + Sync),
) -> Result<PathBuf> {
    tokio::fs::create_dir_all(downloads_dir)
        .await
        .with_context(|| format!("creating {}", downloads_dir.display()))?;

    let _lock = lock_downloads(downloads_dir).await?;
    // Include the whole URL, including its query, so unrelated resources never
    // share partial data even when their final filenames coincide.
    let key = hex::encode(Sha256::digest(url.as_str().as_bytes()));
    let tmp_path = downloads_dir.join(format!(".download-{key}.part"));
    let meta_path = downloads_dir.join(format!(".download-{key}.etag"));
    let mut cleanup = PartialCleanup { data: tmp_path.clone(), metadata: meta_path.clone(), retain: false };
    let saved = std::fs::read_to_string(&meta_path).ok().and_then(|etag| {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::ETAG, etag.parse().ok()?);
        let etag = strong_etag(&headers)?;
        let size = std::fs::metadata(&tmp_path).ok()?.len();
        (size > 0 && size <= max_bytes).then_some((etag, size))
    });
    if saved.is_none() {
        let _ = std::fs::remove_file(&tmp_path);
        let _ = std::fs::remove_file(&meta_path);
    } else {
        cleanup.retain = true;
    }

    let mut request = http.get(url.clone()).header(reqwest::header::ACCEPT_ENCODING, "identity");
    if let Some((etag, offset)) = &saved {
        request = request.header(reqwest::header::RANGE, format!("bytes={offset}-"))
            .header(reqwest::header::IF_RANGE, etag);
    }
    let mut resp = request.send().await.with_context(|| format!("GET {url}"))?;
    let mut offset = 0;
    let mut expected = None;
    if let Some((etag, size)) = &saved {
        if resp.status() == reqwest::StatusCode::PARTIAL_CONTENT
            && strong_etag(resp.headers()).as_ref() == Some(etag)
        {
            if let Some((start, end, total)) = content_range(resp.headers()) {
                if start == *size && end == total - 1
                    && resp.content_length().is_none_or(|len| len == total - start)
                {
                    offset = *size;
                    expected = Some(total);
                }
            }
        }
        if offset == 0 {
            cleanup.retain = false;
            // A 200 response already contains a complete replacement. Other
            // responses (including malformed 206 and 416) require a fresh GET.
            if resp.status() != reqwest::StatusCode::OK {
                resp = http.get(url.clone()).header(reqwest::header::ACCEPT_ENCODING, "identity")
                    .send().await.with_context(|| format!("GET {url}"))?;
            }
        }
    }
    if offset == 0 && resp.status() != reqwest::StatusCode::OK {
        bail!("GET {url} failed: HTTP {}", resp.status());
    }
    if resp.headers().get_all(reqwest::header::CONTENT_ENCODING).iter()
        .any(|value| value.as_bytes() != b"identity")
    {
        cleanup.retain = false;
        bail!("GET {url} returned a non-identity content encoding");
    }
    let total_length = expected.or_else(|| resp.content_length());
    progress(offset, total_length);
    if total_length.is_some_and(|len| len > max_bytes) {
        cleanup.retain = false;
        bail!("download of {url} exceeds max_download_mb cap of {max_bytes} bytes");
    }

    // Synchronous file operations cannot continue in the background after a
    // timed-out future releases the directory lock.
    use std::io::Write;
    let mut file = if offset > 0 {
        std::fs::OpenOptions::new().append(true).open(&tmp_path)
    } else {
        cleanup.retain = false;
        let file = std::fs::File::create(&tmp_path);
        // Truncate before updating metadata: a crash must not pair old bytes
        // with a new validator.
        let _ = std::fs::remove_file(&meta_path);
        if file.is_ok() {
            if let Some(etag) = strong_etag(resp.headers()) {
                std::fs::write(&meta_path, etag).context("writing download validator")?;
                cleanup.retain = true;
            }
        }
        file
    }.with_context(|| format!("opening {}", tmp_path.display()))?;
    let mut stream = resp.bytes_stream();
    let mut total = offset;
    let mut last_report = None;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("reading stream from {url}"))?;
        let next = total.checked_add(chunk.len() as u64).unwrap_or(u64::MAX);
        if next > max_bytes || total_length.is_some_and(|len| next > len) {
            cleanup.retain = false;
            bail!("download of {url} exceeded max_download_mb cap or declared length");
        }
        file.write_all(&chunk).with_context(|| format!("writing {}", tmp_path.display()))?;
        total = next;
        if last_report.is_none_or(|last: std::time::Instant| last.elapsed() >= Duration::from_millis(100)) {
            progress(total, total_length);
            last_report = Some(std::time::Instant::now());
        }
    }
    file.flush().context("flushing download")?;
    if total_length.is_some_and(|len| total != len) {
        bail!("incomplete download of {url}: received {total} bytes");
    }
    progress(total, total_length);
    drop(file);

    let final_name = url.path_segments().and_then(|segs| segs.rev().find(|s| !s.is_empty()))
        .map(sanitize_filename).unwrap_or_else(|| "download.bin".into());
    let dest = downloads_dir.join(final_name);
    std::fs::rename(&tmp_path, &dest)
        .with_context(|| format!("renaming into {}", dest.display()))?;
    cleanup.retain = false;
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
