//! Evaluation of per-application update restrictions.

use anyhow::{Context, Result, bail};
use crate::config::{AppFile, SourceKind};

/// Explain why a candidate is blocked, or return `None` when it is allowed.
/// Range policies require SemVer candidates (optionally prefixed with `v` or `V`) and supported providers.
pub fn reason(app: &AppFile, version: &str, bulk: bool) -> Result<Option<String>> {
    let policy = &app.policy;
    let in_range = if let Some(range) = &policy.range {
        if !matches!(&app.source_kind, SourceKind::GithubReleases(_) | SourceKind::HtmlLinks(_)) {
            bail!("policy.range is unsupported for provider '{}'", app.provider);
        }
        let requirement = semver::VersionReq::parse(range)
            .with_context(|| format!("invalid policy.range '{range}'"))?;
        let normalized = version.strip_prefix('v').or_else(|| version.strip_prefix('V')).unwrap_or(version);
        let candidate = semver::Version::parse(normalized)
            .with_context(|| format!("version '{version}' is not a valid SemVer version required by policy.range"))?;
        Some(requirement.matches(&candidate))
    } else {
        None
    };

    if policy.hold {
        return Ok(Some("application is held by policy".into()));
    }
    if let Some(pin) = &policy.pin {
        if version != pin {
            return Ok(Some(format!("version is pinned to {pin}")));
        }
    }
    if policy.skip.iter().any(|skip| skip == version) {
        return Ok(Some(format!("version {version} is skipped by policy")));
    }
    if in_range == Some(false) {
        return Ok(Some(format!("version {version} is outside policy range {}", policy.range.as_deref().unwrap())));
    }
    if bulk && policy.manual {
        return Ok(Some("manual updates only by policy".into()));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(policy: &str) -> AppFile {
        AppFile::parse(&format!("id = 'test'\nprovider = 'github-releases'\nrepo = 'owner/repo'\nasset_regex = '.*'\n[policy]\n{policy}\n"), "test.toml").unwrap()
    }

    #[test]
    fn exact_pin_skip_hold_and_manual() {
        let pinned = app("pin = 'v1.0.0'\nskip = ['v2.0.0']");
        assert!(reason(&pinned, "v1.0.0", false).unwrap().is_none());
        assert!(reason(&pinned, "1.0.0", false).unwrap().is_some());
        let skipped = app("skip = ['v2.0.0']");
        assert!(reason(&skipped, "v2.0.0", false).unwrap().is_some());
        assert!(reason(&skipped, "v2.0.1", false).unwrap().is_none());
        assert!(reason(&app("hold = true"), "1.0.0", false).unwrap().is_some());
        let manual = app("manual = true");
        assert!(reason(&manual, "1.0.0", false).unwrap().is_none());
        assert!(reason(&manual, "1.0.0", true).unwrap().is_some());
    }

    #[test]
    fn ranges_require_strict_semver() {
        let app = app("range = '^1.2'");
        assert!(reason(&app, "1.3.0", false).unwrap().is_none());
        assert!(reason(&app, "v1.3.0", false).unwrap().is_none());
        assert!(reason(&app, "V1.3.0", false).unwrap().is_none());
        assert!(reason(&app, "v2.0.0", false).unwrap().is_some());
        assert!(reason(&app, "2.0.0", false).unwrap().is_some());
        for invalid in ["v1.3", "Vnightly", "1.3", "nightly"] {
            assert!(reason(&app, invalid, false).is_err());
        }
    }

    #[test]
    fn rejects_invalid_configuration() {
        let base = "id = 'test'\nprovider = 'github-releases'\nrepo = 'owner/repo'\nasset_regex = '.*'\n[policy]\n";
        for policy in ["pin = ''", "range = ''", "range = 'not a range'", "skip = [' ']", "pin = '1'\nrange = '^1'", "pin = '1'\nskip = ['1']", "holde = true"] {
            assert!(AppFile::parse(&format!("{base}{policy}"), "test.toml").is_err(), "{policy}");
        }
        let unsupported = "id = 'test'\nprovider = 'external:example'\n[policy]\nrange = '^1'";
        assert!(AppFile::parse(unsupported, "test.toml").is_err());
    }
}
