//! provider = "apt": Debian/Ubuntu apt repositories (track-and-apply).
//! `check` runs `apt-cache policy <package>`; the Candidate version becomes
//! the single release. `apply_cmd` is executed by the engine, not here.

use super::{Provider, Release};
use crate::config::{AppSource, SourceKind};
use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use std::time::Duration;

pub struct Apt;

#[async_trait]
impl Provider for Apt {
    fn id(&self) -> &'static str {
        "apt"
    }

    async fn check(&self, app: &AppSource, _http: &reqwest::Client) -> Result<Vec<Release>> {
        let cfg = match &app.kind {
            SourceKind::Apt(c) => c.clone(),
            _ => bail!("provider mismatch: apt got another app kind"),
        };
        let out = crate::exec::run(
            &["apt-cache".into(), "policy".into(), cfg.package.clone()],
            None,
            Duration::from_secs(120),
            true,
        )
        .await
        .context("running apt-cache policy")?;
        anyhow::ensure!(
            out.status,
            "apt-cache policy failed ({}):\n{}",
            cfg.package,
            out.stderr_tail
        );
        let Some(version) = parse_candidate(&out.stdout_tail) else {
            return Ok(vec![]);
        };
        Ok(vec![Release {
            version,
            published_at: None,
            assets: vec![],
        }])
    }
}

/// Extract the `Candidate:` version from `apt-cache policy` stdout.
/// None when the package has no candidate (`(none)`) or the line is missing.
pub fn parse_candidate(stdout: &str) -> Option<String> {
    stdout.lines().find_map(|l| {
        let l = l.trim();
        let Some(rest) = l.strip_prefix("Candidate:") else {
            return None;
        };
        let v = rest.trim();
        if v.is_empty() || v == "(none)" {
            None
        } else {
            Some(v.to_string())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::parse_candidate;

    const REAL_POLICY: &str = "htop:
  Installed: 3.0.5-7build2
  Candidate: 3.0.5-7build2
  Version table:
 *** 3.0.5-7build2 500
        500 http://apt.pop-os.org/ubuntu jammy/main amd64 Packages
        100 /var/lib/dpkg/status";

    #[test]
    fn candidate_from_real_policy_block() {
        assert_eq!(parse_candidate(REAL_POLICY).as_deref(), Some("3.0.5-7build2"));
    }

    #[test]
    fn candidate_none_means_no_release() {
        assert_eq!(parse_candidate("htop:\n  Installed: (none)\n  Candidate: (none)\n"), None);
    }

    #[test]
    fn missing_candidate_means_no_release() {
        assert_eq!(parse_candidate("not apt output at all\n"), None);
    }

    #[test]
    fn trailing_whitespace_trimmed() {
        assert_eq!(parse_candidate("  Candidate: 1.2.3  \n").as_deref(), Some("1.2.3"));
    }
}
