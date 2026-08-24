//! Content hashing and build-script snapshots for the source-build gate.

use anyhow::{Context, Result};
use std::path::Path;

/// Hash a set of files as `sha256(concat(sorted(rel \0 len \0 bytes)))`, hex.
/// Order of the input is irrelevant; any byte difference changes the hash.
pub fn bundle_hash(files: &[(String, Vec<u8>)]) -> String {
    use sha2::{Digest, Sha256};
    let mut sorted: Vec<&(String, Vec<u8>)> = files.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut h = Sha256::new();
    for (rel, bytes) in sorted {
        h.update(rel.as_bytes());
        h.update([0u8]);
        h.update(bytes.len().to_string().as_bytes());
        h.update([0u8]);
        h.update(bytes);
    }
    hex::encode(h.finalize())
}

/// Collect files under `root` matching any pattern (`*`, `?`, `/**/` supported),
/// returning `(relative path, contents)` sorted by path. `.git` is skipped.
pub fn collect_files(root: &Path, patterns: &[String]) -> Result<Vec<(String, Vec<u8>)>> {
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| e.file_name() != ".git")
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(root)
            .expect("walkdir yields paths under root")
            .to_string_lossy()
            .into_owned();
        if patterns.iter().any(|p| glob_match(p, &rel)) {
            let bytes = std::fs::read(entry.path())
                .with_context(|| format!("reading {}", entry.path().display()))?;
            out.push((rel, bytes));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// Save matched files as `<snap_dir>/<hash[..12]>.tar`, then prune older
/// snapshots keeping the newest 3. Returns the snapshot path.
pub fn save_snapshot(
    snap_dir: &Path,
    hash: &str,
    files: &[(String, Vec<u8>)],
) -> Result<std::path::PathBuf> {
    std::fs::create_dir_all(snap_dir)
        .with_context(|| format!("creating {}", snap_dir.display()))?;
    let short = &hash[..hash.len().min(12)];
    let dest = snap_dir.join(format!("{short}.tar"));
    let f = std::fs::File::create(&dest).with_context(|| format!("creating {}", dest.display()))?;
    let mut builder = tar::Builder::new(f);
    for (rel, bytes) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, rel, bytes.as_slice())
            .with_context(|| format!("archiving {rel}"))?;
    }
    builder.into_inner()?;
    prune_snapshots(snap_dir, 3)?;
    Ok(dest)
}

/// Keep only the `keep` newest `*.tar` files (by modification time).
pub fn prune_snapshots(snap_dir: &Path, keep: usize) -> Result<()> {
    let mut snaps: Vec<(std::time::SystemTime, std::path::PathBuf)> = std::fs::read_dir(snap_dir)?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "tar") {
                let mtime = e.metadata().ok()?.modified().ok()?;
                Some((mtime, p))
            } else {
                None
            }
        })
        .collect();
    snaps.sort_by(|a, b| b.0.cmp(&a.0)); // newest first
    for (_, stale) in snaps.into_iter().skip(keep) {
        std::fs::remove_file(stale).ok();
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Glob matching: `*` within one segment, `**` across segments, `?` single char.
// ---------------------------------------------------------------------------

/// Match a path (slash-separated) against a glob pattern.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let p: Vec<&str> = pattern.split('/').collect();
    let s: Vec<&str> = path.split('/').collect();
    match_segs(&p, &s)
}

fn match_segs(p: &[&str], s: &[&str]) -> bool {
    match p.first() {
        None => s.is_empty(),
        Some(&"**") => {
            if p.len() == 1 {
                return true;
            }
            (0..=s.len()).any(|i| match_segs(&p[1..], &s[i..]))
        }
        Some(seg) => !s.is_empty() && match_segment(seg, s[0]) && match_segs(&p[1..], &s[1..]),
    }
}

/// Single-segment wildcard match (`*` and `?`; no braces/brackets).
pub fn match_segment(pat: &str, text: &str) -> bool {
    let (pc, tc): (Vec<char>, Vec<char>) = (pat.chars().collect(), text.chars().collect());
    fn go(p: &[char], t: &[char]) -> bool {
        match (p.first(), t.first()) {
            (None, None) => true,
            (Some('*'), _) => (0..=t.len()).any(|i| go(&p[1..], &t[i..])),
            (Some('?'), Some(_)) => go(&p[1..], &t[1..]),
            (Some(a), Some(b)) => a == b && go(&p[1..], &t[1..]),
            _ => false,
        }
    }
    go(&pc, &tc)
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: (&str, &[u8]) = ("a.txt", b"hello");
    const B: (&str, &[u8]) = ("scripts/build.sh", b"#!/bin/sh\n");

    #[test]
    fn hash_order_independent_and_change_sensitive() {
        let h1 = bundle_hash(&[
            (A.0.to_string(), A.1.to_vec()),
            (B.0.to_string(), B.1.to_vec()),
        ]);
        let h2 = bundle_hash(&[
            (B.0.to_string(), B.1.to_vec()),
            (A.0.to_string(), A.1.to_vec()),
        ]);
        assert_eq!(h1, h2);

        let changed = bundle_hash(&[
            (A.0.to_string(), b"hellO".to_vec()),
            (B.0.to_string(), B.1.to_vec()),
        ]);
        assert_ne!(h1, changed);
    }

    #[test]
    fn glob_patterns() {
        assert!(glob_match("CMakeLists.txt", "CMakeLists.txt"));
        assert!(glob_match("cmake/**", "cmake/modules/x.cmake"));
        assert!(glob_match("scripts/*.sh", "scripts/build.sh"));
        assert!(!glob_match("scripts/*.sh", "scripts/sub/build.sh"));
        assert!(glob_match("scripts/**/*.sh", "scripts/sub/build.sh"));
        assert!(glob_match("**/*.sh", "deep/a/b/c.sh"));
        assert!(glob_match("b?t", "bat"));
        assert!(!glob_match("*.txt", "dir/a.txt"), "* must not cross '/'");
    }

    #[test]
    fn collect_matches_and_skips_git() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::create_dir_all(dir.path().join("cmake/mods")).unwrap();
        std::fs::create_dir_all(dir.path().join("scripts")).unwrap();
        std::fs::write(dir.path().join(".git/config"), b"secret").unwrap();
        std::fs::write(dir.path().join("CMakeLists.txt"), b"top").unwrap();
        std::fs::write(dir.path().join("cmake/mods/util.cmake"), b"x").unwrap();
        std::fs::write(dir.path().join("scripts/run.sh"), b"#sh").unwrap();

        let files = collect_files(
            dir.path(),
            &[
                "CMakeLists.txt".into(),
                "cmake/**".into(),
                "scripts/*.sh".into(),
            ],
        )
        .unwrap();
        let names: Vec<&str> = files.iter().map(|(r, _)| r.as_str()).collect();
        assert_eq!(
            names,
            vec!["CMakeLists.txt", "cmake/mods/util.cmake", "scripts/run.sh"]
        );
    }

    #[test]
    fn snapshot_prune_keeps_three() {
        let dir = tempfile::tempdir().unwrap();
        let snap = dir.path().join("snap");
        for i in 0..5 {
            let files = vec![("f.txt".to_string(), vec![b'a' + i as u8])];
            let h = bundle_hash(&files);
            save_snapshot(&snap, &h, &files).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        let mut left: Vec<_> = std::fs::read_dir(&snap)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left.len(), 3, "{left:?}");
    }

    #[test]
    fn snapshot_tar_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let snap = dir.path().join("snap");
        let files = vec![("sub/dir/f.txt".to_string(), b"data".to_vec())];
        let h = bundle_hash(&files);
        let p = save_snapshot(&snap, &h, &files).unwrap();
        assert!(p.exists());

        let f = std::fs::File::open(&p).unwrap();
        let mut ar = tar::Archive::new(f);
        let entries: Vec<String> = ar
            .entries()
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["sub/dir/f.txt"]);
    }
}
