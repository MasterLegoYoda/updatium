//! Conservative proof of individual files published by builtin installers.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Entry {
    pub path: PathBuf,
    pub fingerprint: String,
    pub installation: bool,
}

// Reject symlink ancestors: never follow a replacement directory into unrelated data.
fn safe_ancestors(path: &Path) -> Result<bool> {
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir | Component::CurDir)) { return Ok(false); }
    let Some(parent) = path.parent() else { return Ok(false) };
    let mut prefix = PathBuf::new();
    for component in parent.components() {
        prefix.push(component.as_os_str());
        match std::fs::symlink_metadata(&prefix) {
            Ok(meta) if meta.file_type().is_dir() => {},
            Ok(_) => return Ok(false),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(true)
}

pub(crate) fn fingerprint(path: &Path) -> Result<Option<String>> {
    if !safe_ancestors(path)? { return Ok(None); }
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if meta.file_type().is_symlink() {
        use std::os::unix::ffi::OsStrExt;
        return Ok(Some(format!("link:{}", hex::encode(std::fs::read_link(path)?.as_os_str().as_bytes()))));
    }
    if !meta.file_type().is_file() { return Ok(None); }
    use std::os::unix::fs::PermissionsExt;
    Ok(Some(format!("file:{:o}:{}", meta.permissions().mode() & 0o7777, sha256_file(path)?)))
}

pub(crate) fn sha256_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hash = Sha256::new();
    let mut buf = [0u8; 65536];
    loop { let n = file.read(&mut buf)?; if n == 0 { break; } hash.update(&buf[..n]); }
    Ok(hex::encode(hash.finalize()))
}

pub(crate) fn capture(entries: &mut Vec<Entry>, path: &Path, installation: bool) -> Result<()> {
    let path = if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir()?.join(path) };
    if let Some(fingerprint) = fingerprint(&path)? {
        let entry = Entry { path, fingerprint, installation };
        if !entries.contains(&entry) { entries.push(entry); }
    }
    Ok(())
}

pub(crate) fn matches(entry: &Entry) -> Result<bool> {
    Ok(fingerprint(&entry.path)?.as_deref() == Some(entry.fingerprint.as_str()))
}
