//! provider = "flatpak": Flatpak remotes (track-and-apply).
//! `check` runs `flatpak remote-ls --user|--system <remote> --app
//! --columns=application,version,branch` and emits one release per ref row of
//! the configured app (the whole listing is parsed and filtered by app id;
//! flatpak rejects app args on `remote-ls`). `apply_cmd` is executed by the
//! engine, not here.

use super::{Provider, Release};
use crate::config::{AppSource, SourceKind};
use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use regex::Regex;
use std::collections::HashSet;
use std::time::Duration;

pub struct Flatpak;

#[async_trait]
impl Provider for Flatpak {
    fn id(&self) -> &'static str {
        "flatpak"
    }

    async fn check(&self, app: &AppSource, _http: &reqwest::Client) -> Result<Vec<Release>> {
        let cfg = match &app.kind {
            SourceKind::Flatpak(c) => c.clone(),
            _ => bail!("provider mismatch: flatpak got another app kind"),
        };
        let scope = if cfg.user { "--user" } else { "--system" };
        // Full stdout required: the whole remote listing (~3500 rows) exceeds
        // exec's 200-line capture tail, and flatpak 1.14 rejects app args on
        // `remote-ls`, so the app filter happens after capture.
        let out = crate::exec::run_full(
            &[
                "flatpak".into(),
                "remote-ls".into(),
                scope.into(),
                cfg.remote.clone(),
                "--app".into(),
                "--columns=application,version,branch".into(),
            ],
            None,
            Duration::from_secs(120),
        )
        .await
        .context("running flatpak remote-ls")?;
        anyhow::ensure!(
            out.status,
            "flatpak remote-ls failed:\n{}",
            out.stderr_tail
        );
        parse_remote_ls(
            &out.stdout_tail,
            &cfg.app_id,
            &cfg.version_field,
            cfg.branch_regex.as_deref(),
        )
    }
}

/// Parse `flatpak remote-ls --columns=application,version,branch` stdout
/// (tab-separated rows). Returns one release per surviving row of `app_id`.
pub fn parse_remote_ls(
    stdout: &str,
    app_id: &str,
    version_field: &str,
    branch_regex: Option<&str>,
) -> Result<Vec<Release>> {
    match version_field {
        "version" | "branch" | "auto" => {}
        other => bail!(
            "flatpak: version_field must be one of 'version', 'branch', 'auto' (got '{other}')"
        ),
    }
    let re = match branch_regex {
        Some(r) => Some(
            Regex::new(r).with_context(|| format!("flatpak: bad branch_regex '{r}'"))?,
        ),
        None => None,
    };
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut out = Vec::new();
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut fields = line.split('\t');
        let (Some(app), Some(version), Some(branch)) = (fields.next(), fields.next(), fields.next())
        else {
            continue; // malformed row: skip
        };
        if app.trim() != app_id {
            continue; // other app's row: skip
        }
        let version = version.trim();
        let branch = branch.trim();
        if let Some(re) = &re {
            if !re.is_match(branch) {
                continue;
            }
        }
        let resolved = match version_field {
            "version" => version,
            "branch" => branch,
            _ => {
                if version.is_empty() {
                    branch
                } else {
                    version
                }
            }
        };
        if resolved.is_empty() {
            continue;
        }
        if !seen.insert((resolved.to_string(), branch.to_string())) {
            continue; // duplicate ref row (e.g. multi-arch)
        }
        out.push(Release {
            version: resolved.to_string(),
            published_at: None,
            assets: vec![],
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::parse_remote_ls;

    const QBITTORRENT: &str = "org.qbittorrent.qBittorrent";

    fn versions(stdout: &str, version_field: &str, branch_regex: Option<&str>) -> Vec<String> {
        parse_remote_ls(stdout, QBITTORRENT, version_field, branch_regex)
            .unwrap()
            .into_iter()
            .map(|r| r.version)
            .collect()
    }

    #[test]
    fn auto_prefers_version_column() {
        assert_eq!(
            versions("org.qbittorrent.qBittorrent\t5.2.3\tstable", "auto", None),
            ["5.2.3"]
        );
    }

    #[test]
    fn auto_falls_back_to_branch() {
        assert_eq!(
            versions("org.qbittorrent.qBittorrent\t\t44", "auto", None),
            ["44"]
        );
    }

    #[test]
    fn branch_field_uses_branch() {
        assert_eq!(
            versions("org.qbittorrent.qBittorrent\t5.2.3\tstable", "branch", None),
            ["stable"]
        );
    }

    #[test]
    fn branch_regex_filters() {
        let rows = "org.qbittorrent.qBittorrent\t5.2.3\tstable\norg.qbittorrent.qBittorrent\t6.0b1\tbeta";
        assert_eq!(versions(rows, "auto", Some("^stable$")), ["5.2.3"]);
    }

    #[test]
    fn other_apps_skipped() {
        let rows = "org.mozilla.firefox\t142.0.1\t142.0\norg.qbittorrent.qBittorrent\t5.2.3\tstable";
        assert_eq!(versions(rows, "auto", None), ["5.2.3"]);
    }

    #[test]
    fn duplicate_rows_deduped() {
        let rows = "org.qbittorrent.qBittorrent\t5.2.3\tstable\norg.qbittorrent.qBittorrent\t5.2.3\tstable";
        assert_eq!(versions(rows, "auto", None), ["5.2.3"]);
    }

    #[test]
    fn malformed_row_skipped() {
        let rows = "only-two\tfields\norg.qbittorrent.qBittorrent\t\t44";
        assert_eq!(versions(rows, "auto", None), ["44"]);
    }

    #[test]
    fn invalid_version_field_bails() {
        let err =
            parse_remote_ls("org.qbittorrent.qBittorrent\t1.2.3\tstable", QBITTORRENT, "bogus", None)
                .unwrap_err();
        assert!(err.to_string().contains("version_field"), "{err}");
    }

    #[test]
    fn invalid_branch_regex_bails() {
        let err = parse_remote_ls(
            "org.qbittorrent.qBittorrent\t1.2.3\tstable",
            QBITTORRENT,
            "auto",
            Some("["),
        )
        .unwrap_err();
        assert!(err.to_string().contains("branch_regex"), "{err}");
    }

    #[test]
    fn blank_branch_with_version_still_releases() {
        assert_eq!(versions("org.qbittorrent.qBittorrent\t1.2.3\t", "auto", None), ["1.2.3"]);
    }
}
