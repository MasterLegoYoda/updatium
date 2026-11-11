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
        let out = crate::exec::run_proxy(
            &["apt-cache".into(), "policy".into(), cfg.package.clone()],
            None,
            Duration::from_secs(120),
            true,
            app.proxy.as_deref(),
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

/// Check the syntax required by Debian's version ordering policy.
pub fn valid_version(version: &str) -> bool {
    let (has_epoch, rest) = match version.split_once(':') {
        Some((epoch, rest)) => {
            if epoch.is_empty() || !epoch.bytes().all(|b| b.is_ascii_digit()) {
                return false;
            }
            (true, rest)
        }
        None => (false, version),
    };
    let (upstream, revision) = match rest.rsplit_once('-') {
        Some((upstream, revision)) => (upstream, Some(revision)),
        None => (rest, None),
    };
    upstream.bytes().next().is_some_and(|b| b.is_ascii_digit())
        && upstream.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'+' | b'~' | b'-') || (has_epoch && b == b':'))
        && revision.is_none_or(|r| !r.is_empty() && r.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'+' | b'~')))
}

/// Compare valid Debian package versions using dpkg's version ordering.
///
/// Inputs must be valid ASCII Debian versions, including a numeric epoch when
/// an epoch is present. This function does not validate version syntax.
pub fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    fn parts(version: &str) -> (&str, &str, &str) {
        let (epoch, rest) = version.split_once(':').unwrap_or(("0", version));
        let (upstream, revision) = rest.rsplit_once('-').unwrap_or((rest, "0"));
        (epoch, upstream, revision)
    }

    fn numeric(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
        let a = &a[a.iter().position(|&c| c != b'0').unwrap_or(a.len())..];
        let b = &b[b.iter().position(|&c| c != b'0').unwrap_or(b.len())..];
        a.len().cmp(&b.len()).then_with(|| a.cmp(b))
    }

    fn order(c: Option<u8>) -> i32 {
        match c {
            Some(b'~') => -1,
            None | Some(b'0'..=b'9') => 0,
            Some(c) if c.is_ascii_alphabetic() => i32::from(c),
            Some(c) => i32::from(c) + 256,
        }
    }

    fn component(a: &str, b: &str) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
        while !a.is_empty() || !b.is_empty() {
            while a.first().is_some_and(|c| !c.is_ascii_digit())
                || b.first().is_some_and(|c| !c.is_ascii_digit())
            {
                let result = order(a.first().copied()).cmp(&order(b.first().copied()));
                if result != Ordering::Equal {
                    return result;
                }
                if a.first().is_some_and(|c| !c.is_ascii_digit()) {
                    a = &a[1..];
                }
                if b.first().is_some_and(|c| !c.is_ascii_digit()) {
                    b = &b[1..];
                }
            }
            let a_end = a.iter().position(|c| !c.is_ascii_digit()).unwrap_or(a.len());
            let b_end = b.iter().position(|c| !c.is_ascii_digit()).unwrap_or(b.len());
            let result = numeric(&a[..a_end], &b[..b_end]);
            if result != Ordering::Equal {
                return result;
            }
            a = &a[a_end..];
            b = &b[b_end..];
        }
        Ordering::Equal
    }

    let (a_epoch, a_upstream, a_revision) = parts(a);
    let (b_epoch, b_upstream, b_revision) = parts(b);
    numeric(a_epoch.as_bytes(), b_epoch.as_bytes())
        .then_with(|| component(a_upstream, b_upstream))
        .then_with(|| component(a_revision, b_revision))
}

#[cfg(test)]
mod tests {
    use super::{compare_versions, parse_candidate, valid_version};
    use std::cmp::Ordering;

    const REAL_POLICY: &str = "htop:
  Installed: 3.0.5-7build2
  Candidate: 3.0.5-7build2
  Version table:
 *** 3.0.5-7build2 500
        500 http://apt.pop-os.org/ubuntu jammy/main amd64 Packages
        100 /var/lib/dpkg/status";

    #[test]
    fn rejects_non_debian_probe_versions() {
        for version in ["1:2.0-1ubuntu2", "2.0~rc1-0", "1:2:3-4", "2.0"] {
            assert!(valid_version(version), "{version}");
        }
        for version in ["", "v2.0", "epoch:1.0", "1.0-", "1.0-2_1", " 1.0"] {
            assert!(!valid_version(version), "{version}");
        }
    }

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

    #[test]
    fn debian_versions_compare_epochs_and_revisions() {
        let cases = [
            ("1:1.0", "2.0", Ordering::Greater),
            ("0002:1", "2:1", Ordering::Equal),
            ("999999999999999999999999999999:1", "1000000000000000000000000000000:1", Ordering::Less),
            ("1.0", "1.0-0", Ordering::Equal),
            ("1.0-00", "1.0", Ordering::Equal),
            ("1.0-1", "1.0-2", Ordering::Less),
            ("1.0-1-2", "1.0-1-10", Ordering::Less),
            ("1.0-1-1", "1.0-2-0", Ordering::Less),
        ];
        for (a, b, expected) in cases {
            assert_eq!(compare_versions(a, b), expected, "{a} vs {b}");
            assert_eq!(compare_versions(b, a), expected.reverse(), "{b} vs {a}");
        }
    }

    #[test]
    fn debian_versions_compare_components() {
        let cases = [
            ("1.0~rc1", "1.0", Ordering::Less),
            ("1.0~~", "1.0~", Ordering::Less),
            ("1.0-1~bpo", "1.0-1", Ordering::Less),
            ("1.0a", "1.0+", Ordering::Less),
            ("1.0A", "1.0a", Ordering::Less),
            ("1.0002", "1.2", Ordering::Equal),
            ("1.9", "1.10", Ordering::Less),
            ("1.0000000000000000000000000010", "1.9", Ordering::Greater),
            ("1.0", "1.0a", Ordering::Less),
            ("1.0a1", "1.0a01", Ordering::Equal),
        ];
        for (a, b, expected) in cases {
            assert_eq!(compare_versions(a, b), expected, "{a} vs {b}");
            assert_eq!(compare_versions(b, a), expected.reverse(), "{b} vs {a}");
        }
    }

    #[test]
    fn debian_version_order_is_transitive() {
        let versions = ["1.0~~", "1.0~rc1", "1.0", "1.0-1", "1.0-2", "1.0a", "1:0"];
        for (i, a) in versions.iter().enumerate() {
            for b in &versions[i + 1..] {
                assert_eq!(compare_versions(a, b), Ordering::Less, "{a} vs {b}");
            }
        }
    }
}
