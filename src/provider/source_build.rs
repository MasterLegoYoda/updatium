//! provider = "source-build": git tag tracking for build-from-source repos.
//! Versions come from `git ls-remote --tags <repo_url>` filtered by `ref_regex`;
//! `refs/tags/` and one leading `v` are stripped. When `api_base` is set, GitHub
//! releases are queried as well for richer metadata (union of both sources).

use super::{Provider, Release, version_from_tag};
use crate::config::{AppSource, SourceKind};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use std::time::Duration;

pub struct SourceBuild;

#[async_trait]
impl Provider for SourceBuild {
    fn id(&self) -> &'static str {
        "source-build"
    }

    async fn check(&self, app: &AppSource, http: &reqwest::Client) -> Result<Vec<Release>> {
        let cfg = match &app.kind {
            SourceKind::SourceBuild(c) => c.clone(),
            _ => bail!("provider mismatch: source-build got another app kind"),
        };
        let re = regex::Regex::new(&cfg.ref_regex)
            .with_context(|| format!("bad ref_regex {:?}", cfg.ref_regex))?;

        // argv-direct git invocation: no shell, no injection surface.
        // Full stdout is required: large repos (llama.cpp has >8000 tag refs)
        // exceed exec's 200-line capture tail, and the matching tags may all
        // sit before that tail (e.g. `b1234` tags sort before the `v*` tags).
        let out = crate::exec::run_full(
            &[
                "git".to_string(),
                "ls-remote".to_string(),
                "--tags".to_string(),
                cfg.repo_url.clone(),
            ],
            None,
            Duration::from_secs(120),
        )
        .await
        .context("running git ls-remote --tags")?;
        anyhow::ensure!(out.status, "git ls-remote failed:\n{}", out.stderr_tail);

        let mut versions: Vec<String> = Vec::new();
        for line in out.stdout_tail.lines() {
            // Format: "<sha>\trefs/tags/<name>" (^{} peeled lines share the ref).
            let Some((_, r)) = line.split_once('\t') else {
                continue;
            };
            if r.ends_with("^{}") {
                continue;
            }
            if !re.is_match(r) {
                continue;
            }
            let tag = r.strip_prefix("refs/tags/").unwrap_or(r);
            versions.push(version_from_tag(tag));
        }

        let mut releases: Vec<Release> = versions
            .into_iter()
            .map(|version| Release {
                version,
                published_at: None,
                assets: vec![],
            })
            .collect();

        // Optional acceleration: also ask the GitHub API when configured.
        if let Some(api_base) = &cfg.api_base {
            if let Some(repo) = repo_from_url(&cfg.repo_url) {
                let gh_cfg = crate::config::GithubReleasesCfg {
                    repo,
                    tag_regex: None,
                    asset_regex: None,
                    include_prereleases: true,
                    api_base: api_base.clone(),
                };
                let gh = super::github_releases::GithubReleases::new(Vec::new());
                if let Ok(mut extra) = gh
                    .check(
                        &AppSource {
                            allow_insecure_http: app.allow_insecure_http,
                            kind: SourceKind::GithubReleases(gh_cfg),
                        },
                        http,
                    )
                    .await
                {
                    extra.extend(releases);
                    releases = extra;
                }
            }
        }

        Ok(releases)
    }
}

/// Best-effort "owner/name" extraction from an https git URL's last two segments.
fn repo_from_url(url: &str) -> Option<String> {
    if !url.contains("://") {
        return None;
    }
    let trimmed = url.trim_end_matches('/');
    let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    let mut segs: Vec<&str> = trimmed.rsplit('/').take(2).collect();
    segs.reverse();
    if segs.len() == 2 && !segs.iter().any(|s| s.contains(':') || s.is_empty()) {
        Some(format!("{}/{}", segs[0], segs[1]))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_extraction() {
        assert_eq!(
            repo_from_url("https://github.com/ggml-org/llama.cpp"),
            Some("ggml-org/llama.cpp".into())
        );
        assert_eq!(
            repo_from_url("https://github.com/ggml-org/llama.cpp.git"),
            Some("ggml-org/llama.cpp".into())
        );
        assert_eq!(repo_from_url("/local/path"), None);
    }
}
