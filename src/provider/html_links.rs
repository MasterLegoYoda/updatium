//! provider = "html-links": scrape AppImage (or any file) links from a vendor page.

use super::{Asset, Provider, Release};
use crate::config::{AppSource, SourceKind};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use std::collections::HashMap;
use std::time::Duration;

pub struct HtmlLinks;

#[async_trait]
impl Provider for HtmlLinks {
    fn id(&self) -> &'static str {
        "html-links"
    }

    async fn check(&self, app: &AppSource, http: &reqwest::Client) -> Result<Vec<Release>> {
        let cfg = match &app.kind {
            SourceKind::HtmlLinks(c) => c.clone(),
            _ => bail!("provider mismatch: html-links got another app kind"),
        };

        let page = url::Url::parse(&cfg.url).context("bad url in config")?;
        match page.scheme() {
            "https" => {}
            "http" if app.allow_insecure_http => {}
            other => bail!(
                "refusing non-HTTPS url ({other}:) — set allow_insecure_http = true on the app"
            ),
        }

        let link_re = regex::Regex::new(&cfg.link_regex).context("bad link_regex")?;
        if !link_re.capture_names().any(|n| n == Some("href")) {
            bail!("link_regex must define named group 'href'");
        }
        let ver_re = regex::Regex::new(&cfg.version_capture).context("bad version_capture")?;
        if !ver_re.capture_names().any(|n| n == Some("version")) {
            bail!("version_capture must define named group 'version'");
        }

        let base: url::Url = match &cfg.base_url {
            Some(b) => url::Url::parse(b).context("bad base_url")?,
            None => page.clone(),
        };

        let fut = http.get(page.clone()).send();
        let resp = tokio::time::timeout(Duration::from_secs(60), fut)
            .await
            .map_err(|_| anyhow::anyhow!("fetching {} timed out", cfg.url))?
            .with_context(|| format!("GET {}", cfg.url))?;
        let status = resp.status();
        if !status.is_success() {
            bail!("GET {} failed: HTTP {status}", cfg.url);
        }
        let text = resp.text().await.context("reading page body")?;

        // version -> lexicographically-largest asset URL
        let mut by_version: HashMap<String, String> = HashMap::new();
        for caps in link_re.captures_iter(&text) {
            let Some(href) = caps.name("href") else {
                continue;
            };
            let href = href.as_str();
            let absolute = base
                .join(href)
                .with_context(|| format!("resolving href {href:?}"))?;
            let Some(vcaps) = ver_re.captures(href) else {
                continue;
            };
            let Some(vm) = vcaps.name("version") else {
                continue;
            };
            let version = vm.as_str().to_string();
            let entry = by_version.entry(version).or_default();
            if entry.as_str() < absolute.as_str() {
                *entry = absolute.as_str().to_string();
            }
        }

        let mut out = Vec::new();
        for (version, link) in by_version {
            let asset_url = url::Url::parse(&link)?;
            let name = asset_url
                .path_segments()
                .and_then(|s| s.rev().next())
                .unwrap_or_default()
                .to_string();
            let digest = match &cfg.digest_suffix {
                Some(suffix) => probe_digest(http, &asset_url, suffix).await,
                None => None,
            };
            out.push(Release {
                version,
                published_at: None,
                assets: vec![Asset {
                    name,
                    url: asset_url,
                    size: None,
                    digest,
                }],
            });
        }
        Ok(out)
    }
}

/// Try `<asset-url><suffix>`; a missing file (404 / fetch error) is simply no
/// digest — never an error.
async fn probe_digest(
    http: &reqwest::Client,
    asset_url: &url::Url,
    suffix: &str,
) -> Option<String> {
    let mut candidate = asset_url.clone();
    candidate.set_path(&format!("{}{}", candidate.path(), suffix));
    let resp = http.get(candidate).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let text = resp.text().await.ok()?;
    let first = text.lines().next()?.trim_start();
    let hex = first.split_whitespace().next()?;
    let hex = hex.trim_start_matches('*');
    if hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(format!("sha256:{}", hex.to_ascii_lowercase()))
    } else {
        None
    }
}
