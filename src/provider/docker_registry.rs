//! provider = "docker-registry": Docker Hub + generic OCI registry tag tracking.
//!
//! Two flows:
//! - Hub form (`docker.io/<owner>/<name>` or bare `owner/name`):
//!   `GET {api_base}/v2/repositories/<repo>/tags?page_size=100` following the
//!   JSON `next` URL.
//! - Other registries (ghcr.io, ...): bearer token dance
//!   (`auth.<registry>/token?service=...&scope=repository:<repo>:pull`), then
//!   `GET <registry>/v2/<repo>/tags/list` following Link headers.
//! When `api_base` is overridden, both endpoints live under it (fixture/test mode).

use super::{Provider, Release};
use crate::config::{AppSource, SourceKind};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;

pub struct DockerRegistry;

pub const DEFAULT_API_BASE: &str = "https://registry.hub.docker.com";

/// Split an image reference into (registry host, repository path).
pub fn split_image(image: &str) -> Result<(String, String)> {
    let ok = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._/-".contains(&b))
    };
    // Registry hosts may carry a port ("localhost:5000", "reg:8443").
    let ok_host = |s: &str| {
        ok(s)
            || s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".:_-".contains(&b))
    };
    let parts: Vec<&str> = image.split('/').collect();
    let hostish = |s: &str| s.contains('.') || s.contains(':') || s == "localhost";
    let (reg, repo) = match parts.as_slice() {
        [owner, name] if !hostish(owner) => ("docker.io", format!("{owner}/{name}")),
        [host, rest @ ..] if hostish(host) && !rest.is_empty() => (*host, rest.join("/")),
        _ => bail!("image must be 'owner/name' or '[registry/]owner/name', got {image:?}"),
    };
    if !ok(&repo) || !ok_host(&reg) {
        bail!("image contains unsafe characters: {image:?}");
    }
    Ok((reg.to_string(), repo))
}

#[derive(Deserialize)]
struct HubTags {
    #[serde(default)]
    next: Option<String>,
    #[serde(default)]
    results: Vec<HubTag>,
}

#[derive(Deserialize)]
struct HubTag {
    name: String,
}

#[derive(Deserialize)]
struct V2Tags {
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Deserialize)]
struct AuthToken {
    token: String,
}

fn extract_next_link(link_header: &str) -> Option<String> {
    // `<url>; rel="next"` — possibly among several comma-separated links.
    for part in link_header.split(',') {
        let mut seg = part.splitn(2, ';');
        let url = seg.next()?.trim();
        let params = seg.next().unwrap_or("");
        if params.split(';').any(|p| {
            p.trim()
                .trim_matches('"')
                .split('=')
                .map(str::trim)
                .eq(["rel", "next"].into_iter())
        }) || params.contains("rel=\"next\"")
        {
            let inner = url.trim_start_matches('<').trim_end_matches('>');
            return Some(inner.to_string());
        }
    }
    None
}

const MAX_PAGES: usize = 10;

#[async_trait]
impl Provider for DockerRegistry {
    fn id(&self) -> &'static str {
        "docker-registry"
    }

    async fn check(&self, app: &AppSource, http: &reqwest::Client) -> Result<Vec<Release>> {
        let cfg = match &app.kind {
            SourceKind::DockerRegistry(c) => c.clone(),
            _ => bail!("provider mismatch: docker-registry got another app kind"),
        };
        let tag_re = match &cfg.tag_regex {
            Some(r) => Some(regex::Regex::new(r).with_context(|| format!("bad tag_regex {r:?}"))?),
            None => None,
        };

        let (reg, repo) = split_image(&cfg.image)?;
        let hub_form = matches!(
            reg.as_str(),
            "docker.io" | "index.docker.io" | "registry-1.docker.io"
        );

        let mut tags: Vec<String> = Vec::new();
        if hub_form {
            let base = cfg.api_base.trim_end_matches('/');
            let mut url = format!("{base}/v2/repositories/{repo}/tags?page_size=100");
            for _ in 0..MAX_PAGES {
                let resp = http
                    .get(&url)
                    .send()
                    .await
                    .with_context(|| format!("GET {url}"))?;
                let status = resp.status();
                let body = resp.text().await?;
                anyhow::ensure!(status.is_success(), "GET {url} failed: HTTP {status}");
                let page: HubTags =
                    serde_json::from_str(&body).with_context(|| format!("decoding {url}"))?;
                tags.extend(page.results.into_iter().map(|t| t.name));
                match page.next {
                    Some(n) => url = n,
                    None => break,
                }
            }
        } else {
            let fixture_mode = cfg.api_base != DEFAULT_API_BASE;
            let reg_base = if fixture_mode {
                cfg.api_base.trim_end_matches('/').to_string()
            } else {
                format!("https://{reg}")
            };
            let auth_url = if fixture_mode {
                format!("{reg_base}/v2/auth")
            } else {
                format!("https://auth.{reg}/token")
            };
            let auth_url = format!("{auth_url}?service={reg}&scope=repository:{repo}:pull");
            let authed = http
                .get(auth_url)
                .send()
                .await
                .context("registry token request");
            let tok: AuthToken = match authed {
                Ok(r) if r.status().is_success() => r.json().await.unwrap_or(AuthToken {
                    token: String::new(),
                }),
                _ => AuthToken {
                    token: String::new(),
                },
            };

            let mut url = format!("{reg_base}/v2/{repo}/tags/list");
            for _ in 0..MAX_PAGES {
                let mut req = http.get(&url);
                if !tok.token.is_empty() {
                    req = req.header("Authorization", format!("Bearer {}", tok.token));
                }
                let resp = req.send().await.with_context(|| format!("GET {url}"))?;
                let status = resp.status();
                let link = resp
                    .headers()
                    .get(reqwest::header::LINK)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                let body = resp.text().await?;
                anyhow::ensure!(status.is_success(), "GET {url} failed: HTTP {status}");
                let page: V2Tags =
                    serde_json::from_str(&body).with_context(|| format!("decoding {url}"))?;
                tags.extend(page.tags);
                match link.as_deref().and_then(extract_next_link) {
                    Some(next) => url = next,
                    None => break,
                }
            }
        }

        Ok(tags
            .into_iter()
            .filter(|t| match &tag_re {
                Some(re) => re.is_match(t),
                None => true,
            })
            .map(|version| Release {
                version,
                published_at: None,
                assets: vec![],
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_splitting() {
        assert_eq!(
            split_image("nginx").is_err(),
            true,
            "bare name without owner rejected"
        );
        assert_eq!(
            split_image("library/nginx").unwrap(),
            ("docker.io".into(), "library/nginx".into())
        );
        assert_eq!(
            split_image("ghcr.io/open-webui/open-webui").unwrap(),
            ("ghcr.io".into(), "open-webui/open-webui".into())
        );
        assert_eq!(
            split_image("localhost:5000/my/app").unwrap(),
            ("localhost:5000".into(), "my/app".into())
        );
        assert!(split_image("evil/$(x)").is_err());
    }

    #[test]
    fn link_parsing() {
        assert_eq!(
            extract_next_link(r#"<http://x/v2/a/tags/list?last=b>; rel="next""#),
            Some("http://x/v2/a/tags/list?last=b".to_string())
        );
        assert_eq!(
            extract_next_link(r#"<http://x/prev>; rel="previous", <http://x/next>; rel="next""#),
            Some("http://x/next".to_string())
        );
        assert_eq!(extract_next_link(r#"<http://x>; rel="last""#), None);
    }
}
