//! Version ordering used everywhere a "latest" must be picked.
//!
//! Rule: if both sides parse as strict semver (`semver::Version`), compare with
//! semver precedence (handles prereleases like `1.0.0-rc.1 < 1.0.0`). Otherwise
//! fall back to a natural (version-aware) byte order: the strings are split into
//! digit-runs and non-digit-runs; digit runs compare numerically, non-digit runs
//! byte-wise; a proper prefix sorts lower. Total order, pure function.

use std::cmp::Ordering;

/// Compare two version strings.
pub fn cmp_version(a: &str, b: &str) -> Ordering {
    match (
        semver::Version::parse(a.trim()),
        semver::Version::parse(b.trim()),
    ) {
        // Build metadata must not create phantom updates: compare core + prerelease.
        (Ok(mut va), Ok(mut vb)) => {
            va.build = semver::BuildMetadata::EMPTY;
            vb.build = semver::BuildMetadata::EMPTY;
            va.cmp(&vb)
        }
        _ => natural_cmp(a.trim().as_bytes(), b.trim().as_bytes()),
    }
}

enum Chunk<'a> {
    Num(&'a [u8]),
    Str(&'a [u8]),
}

/// Split into alternating digit / non-digit runs.
fn chunks(mut s: &[u8]) -> Vec<Chunk<'_>> {
    let mut out = Vec::new();
    while !s.is_empty() {
        let digit = s[0].is_ascii_digit();
        let end = s
            .iter()
            .position(|c| c.is_ascii_digit() != digit)
            .unwrap_or(s.len());
        let (chunk, rest) = s.split_at(end);
        out.push(if digit {
            Chunk::Num(chunk)
        } else {
            Chunk::Str(chunk)
        });
        s = rest;
    }
    out
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
    let ca = chunks(a);
    let cb = chunks(b);
    for i in 0..ca.len().max(cb.len()) {
        match (ca.get(i), cb.get(i)) {
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
    Ordering::Equal
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
    fn mixed_semver_and_natural() {
        // "v" prefix blocks semver parsing -> natural order, still correct here.
        assert_eq!(cmp_version("v1.10", "v1.9"), Greater);
        assert_eq!(cmp_version("1.2.3", "1.2.3"), Equal);
    }
}
