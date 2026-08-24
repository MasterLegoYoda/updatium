//! provider = "github-releases": GitHub (or GHES) release assets.

use super::{Asset, Provider, Release, normalize_digest, parse_sums_line, version_from_tag};
use crate::config::{AppSource, SourceKind};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use chrono::DateTime;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::OnceCell;

pub struct GithubReleases {
    token_cmd: Vec<String>,
    token: OnceCell<Option<String>>,
}

const MAX_PAGES: usize = 5;

#[derive(Deserialize)]
struct GhRelease {
    #[serde(default)]
    tag_name: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    published_at: Option<DateTime<chrono::Utc>>,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: Option<u64>,
    /// Newer GitHub API field: "sha256:<hex>".
    #[serde(default)]
    digest: Option<String>,
}

impl GithubReleases {
    pub fn new(token_cmd: Vec<String>) -> Self {
        GithubReleases {
            token_cmd,
            token: OnceCell::new(),
        }
    }

    /// Resolve a PAT once per process: `token_cmd` argv first line, else
    /// GITHUB_TOKEN env. Never logged, never persisted.
    async fn bearer(&self) -> Option<String> {
        self.token
            .get_or_init(|| async {
                if !self.token_cmd.is_empty() {
                    match crate::exec::run(
                        &self.token_cmd,
                        None,
                        std::time::Duration::from_secs(30),
                        true,
                    )
                    .await
                    {
                        Ok(out) if out.status => {
                            let line = out.stdout_tail.lines().next().unwrap_or("").trim();
                            if !line.is_empty() {
                                return Some(line.to_string());
                            }
                        }
                        _ => {} // fall through to env; never fail check over token lookup
                    }
                }
                std::env::var("GITHUB_TOKEN").ok().filter(|t| !t.is_empty())
            })
            .await
            .clone()
    }
}

fn validate_repo(repo: &str) -> Result<(&str, &str)> {
    let parts: Vec<&str> = repo.split('/').collect();
    if parts.len() != 2
        || parts.iter().any(|p| {
            p.is_empty()
                || !p
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        })
    {
        bail!("repo must be 'owner/name' with safe characters, got {repo:?}");
    }
    Ok((parts[0], parts[1]))
}

#[async_trait]
impl Provider for GithubReleases {
    fn id(&self) -> &'static str {
        "github-releases"
    }

    async fn check(&self, app: &AppSource, http: &reqwest::Client) -> Result<Vec<Release>> {
        let cfg = match &app.kind {
            SourceKind::GithubReleases(c) => c.clone(),
            _ => bail!("provider mismatch: github-releases got another app kind"),
        };
        let (owner, repo) = validate_repo(&cfg.repo)?;

        let tag_re = match &cfg.tag_regex {
            Some(r) => Some(regex::Regex::new(r).with_context(|| format!("bad tag_regex {r:?}"))?),
            None => None,
        };
        let asset_re = match &cfg.asset_regex {
            Some(r) => {
                Some(regex::Regex::new(r).with_context(|| format!("bad asset_regex {r:?}"))?)
            }
            None => None,
        };

        let api = cfg.api_base.trim_end_matches('/');
        let auth = self.bearer().await;
        let mut out = Vec::new();

        for page in 1..=MAX_PAGES {
            let url = format!("{api}/repos/{owner}/{repo}/releases?per_page=100&page={page}");
            let mut req = http
                .get(&url)
                .header("Accept", "application/vnd.github+json");
            if let Some(tok) = &auth {
                req = req.header("Authorization", format!("Bearer {tok}"));
            }
            let resp = req.send().await.with_context(|| format!("GET {url}"))?;
            let status = resp.status();
            let body = resp.text().await.context("reading releases body")?;
            if !status.is_success() {
                bail!("GET {url} failed: HTTP {status}: {}", truncate(&body, 300));
            }
            let releases: Vec<GhRelease> = serde_json::from_str(&body)
                .with_context(|| format!("decoding releases JSON from {url}"))?;

            let empty = releases.is_empty();
            for r in releases {
                if r.draft {
                    continue;
                }
                if r.prerelease && !cfg.include_prereleases {
                    continue;
                }
                let tag = r.tag_name.clone().unwrap_or_default();
                if tag.is_empty() {
                    continue;
                }
                // Filter on the tag or the display name.
                if let Some(re) = &tag_re {
                    let hay_name = r.name.as_deref().unwrap_or("");
                    if !re.is_match(&tag) && !re.is_match(hay_name) {
                        continue;
                    }
                }
                let mut assets = Vec::new();
                for a in &r.assets {
                    if let Some(re) = &asset_re {
                        if !re.is_match(&a.name) {
                            continue;
                        }
                    }
                    let url = url::Url::parse(&a.browser_download_url)
                        .with_context(|| format!("bad asset URL {:?}", a.browser_download_url))?;
                    // Digest: inline field first, then sibling sums file.
                    let mut digest = a.digest.as_deref().and_then(normalize_digest);
                    if digest.is_none() {
                        digest = resolve_sums_digest(http, &r.assets, &a.name, app).await;
                    }
                    assets.push(Asset {
                        name: a.name.clone(),
                        url,
                        size: a.size,
                        digest,
                    });
                }
                out.push(Release {
                    version: version_from_tag(&tag),
                    published_at: r.published_at,
                    assets,
                });
            }
            if empty {
                break;
            }
        }
        Ok(out)
    }
}

/// Look for `<asset>.sha256` or a SHA256SUMS sibling among the release's other
/// assets and fetch it to extract this asset's digest. Network errors and 404s
/// degrade to None.
async fn resolve_sums_digest(
    http: &reqwest::Client,
    siblings: &[GhAsset],
    asset_name: &str,
    _app: &AppSource,
) -> Option<String> {
    let wanted = [format!("{asset_name}.sha256"), "SHA256SUMS".to_string()];
    for w in wanted {
        if let Some(sib) = siblings.iter().find(|a| a.name == w) {
            match http.get(&sib.browser_download_url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    let text = resp.text().await.ok()?;
                    for line in text.lines() {
                        if let Some((hex, file)) = parse_sums_line(line) {
                            if file == asset_name {
                                return Some(format!("sha256:{hex}"));
                            }
                        }
                    }
                }
                _ => continue,
            }
        }
    }
    None
}

fn truncate(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

// Keep Arc import used even as signatures evolve.
#[allow(unused)]
fn _assert_send_sync(_: Arc<dyn Provider>) {}
