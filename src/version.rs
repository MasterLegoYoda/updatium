//! Version ordering used everywhere a "latest" must be picked.
//!
//! SemVer tags (optionally prefixed with `v` or `V`), two-component
//! versions, and zero-padded numeric cores share SemVer precedence.
//! Other strings form a separate, lower-sorting class ordered naturally. A
//! fixed class per string keeps comparisons transitive across mixed formats.

use std::cmp::Ordering;

/// Compare versions without changing the original release tag.
pub fn cmp_version(a: &str, b: &str) -> Ordering {
    let (a, b) = (a.trim(), b.trim());
    match (semantic_version(a), semantic_version(b)) {
        (Some(a), Some(b)) => a.cmp_precedence(&b),
        (Some(_), None) => Ordering::Greater,
        (None, Some(_)) => Ordering::Less,
        (None, None) => natural_cmp(a.as_bytes(), b.as_bytes()),
    }
}

fn semantic_version(tag: &str) -> Option<semver::Version> {
    let tag = tag.strip_prefix('v').or_else(|| tag.strip_prefix('V')).unwrap_or(tag);
    if let Ok(version) = semver::Version::parse(tag) {
        return Some(version);
    }

    // Common partial and zero-padded versions are not strict SemVer. Normalize
    // the numeric core; validate the suffix with the SemVer parser.
    let end = tag.find(['-', '+']).unwrap_or(tag.len());
    let core = &tag[..end];
    let mut parts = core.split('.');
    let (Some(major), Some(minor), patch, None) =
        (parts.next(), parts.next(), parts.next(), parts.next()) else {
            return None;
        };
    let parts = [major, minor, patch.unwrap_or("0")];
    if !parts.iter().all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    let normalized = parts.map(|part| {
        let stripped = part.trim_start_matches('0');
        if stripped.is_empty() { "0" } else { stripped }
    });
    semver::Version::parse(&format!("{}.{}.{}{}", normalized[0], normalized[1], normalized[2], &tag[end..])).ok()
}

enum Chunk<'a> {
    Num(&'a [u8]),
    Str(&'a [u8]),
}

/// Split into alternating digit / non-digit runs.
fn chunks(mut s: &[u8]) -> impl Iterator<Item = Chunk<'_>> {
    std::iter::from_fn(move || {
        if s.is_empty() {
            return None;
        }
        let digit = s[0].is_ascii_digit();
        let end = s.iter().position(|c| c.is_ascii_digit() != digit).unwrap_or(s.len());
        let (chunk, rest) = s.split_at(end);
        s = rest;
        Some(if digit { Chunk::Num(chunk) } else { Chunk::Str(chunk) })
    })
}

fn cmp_num(a: &[u8], b: &[u8]) -> Ordering {
    let a = strip_zeros(a);
    let b = strip_zeros(b);
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

fn strip_zeros(mut s: &[u8]) -> &[u8] {
    while s.len() > 1 && s[0] == b'0' {
        s = &s[1..];
    }
    s
}

fn natural_cmp(a: &[u8], b: &[u8]) -> Ordering {
    let (mut ca, mut cb) = (chunks(a), chunks(b));
    loop {
        match (ca.next(), cb.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                let ord = match (x, y) {
                    (Chunk::Num(p), Chunk::Num(q)) => cmp_num(p, q),
                    (Chunk::Str(p), Chunk::Str(q)) => p.cmp(q),
                    // Digits sort before other characters when classes differ.
                    (Chunk::Num(_), Chunk::Str(_)) => Ordering::Less,
                    (Chunk::Str(_), Chunk::Num(_)) => Ordering::Greater,
                };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering::*;

    #[test]
    fn semver_numeric_segments() {
        assert_eq!(cmp_version("1.10", "1.9"), Greater);
        assert_eq!(cmp_version("1.9", "1.10"), Less);
        assert_eq!(cmp_version("1.0", "1.0"), Equal);
    }

    #[test]
    fn natural_tag_numbers() {
        assert_eq!(cmp_version("b1234", "b999"), Greater);
        assert_eq!(cmp_version("b999", "b1234"), Less);
        assert_eq!(cmp_version("release-88", "release-9"), Greater);
    }

    #[test]
    fn semver_prerelease_ordering() {
        assert_eq!(cmp_version("1.0.0-alpha", "1.0.0"), Less);
        assert_eq!(cmp_version("1.0.0-alpha.1", "1.0.0-alpha.2"), Less);
        assert_eq!(cmp_version("1.0.0-rc.1", "1.0.0-beta.11"), Greater);
        assert_eq!(cmp_version("1.0.0+build5", "1.0.0"), Equal);
    }

    #[test]
    fn date_versions_with_leading_zeros() {
        assert_eq!(cmp_version("2024.06.12", "2024.06.2"), Greater);
        assert_eq!(cmp_version("2024.06.2", "2024.06.12"), Less);
    }

    #[test]
    fn mixed_formats_have_stable_class_order() {
        assert_eq!(cmp_version("v1.10", "v1.9"), Greater);
        assert_eq!(cmp_version("v1.10", "1.9.0"), Greater);
        assert_eq!(cmp_version("1.2", "v1.2.0+build"), Equal);
        assert_eq!(cmp_version("1.2.3", "v1.2.3+build.7"), Equal);
        assert_eq!(cmp_version("2024.06.2", "2024.6.12"), Less);
        assert_eq!(cmp_version("1.2.3", "b999"), Greater);
    }

    #[test]
    fn prefixed_semver_uses_semver_precedence() {
        assert_eq!(cmp_version("v1.0.0-rc.1", "v1.0.0"), Less);
        assert_eq!(cmp_version("V1.0.0", "1.0.0+build.2"), Equal);
        assert_eq!(cmp_version("v1.0.0-alpha.10", "1.0.0-alpha.2"), Greater);
        assert_eq!(cmp_version("1.0.0-alpha.1", "1.0.0-alpha.beta"), Less);
        assert_eq!(cmp_version("v1.0.0-alpha-beta", "1.0.0-alpha.1"), Greater);
    }

    #[test]
    fn comparison_is_transitive_and_equality_is_consistent() {
        let versions = [
            "1.0.0", "v1.0.0", "V1.0.0+build.1", "1.0.0-rc.1",
            "1.0.0-alpha.2", "1.0.0-alpha.10", "1.0.0-alpha.beta",
            "1.0.0-alpha-beta", "1.0.0-rc.01", "1.0", "v1.0",
            "b999", "b1234", "2024.06.2", "2024.6.12", "2024-06-02",
            "2024-06-12", "1.02.003", "1.2.3", "1.2.3+other", "",
        ];
        for a in versions {
            for b in versions {
                let ab = cmp_version(a, b);
                assert_eq!(ab, cmp_version(b, a).reverse(), "{a:?}, {b:?}");
                for c in versions {
                    let bc = cmp_version(b, c);
                    let ac = cmp_version(a, c);
                    if ab != Greater && bc != Greater {
                        assert_ne!(ac, Greater, "{a:?} <= {b:?} <= {c:?}");
                    }
                    if ab == Equal {
                        assert_eq!(ac, bc, "{a:?} == {b:?}, versus {c:?}");
                    }
                }
            }
        }
    }
}
