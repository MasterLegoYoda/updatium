//! Explicit cleanup of recognizable per-app artifacts. Current installations,
//! configuration, history, external destinations, and application data are untouched.

use std::path::{Component, Path, PathBuf};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use crate::{config::HomePaths, store::Store};

#[derive(Debug, Clone, Copy, Default)]
pub struct CleanupOptions {
    pub downloads: bool,
    pub staging: bool,
    pub snapshots: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum CleanupCategory { Download, Staging, Snapshot }

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CleanupItem {
    pub category: CleanupCategory,
    pub path: PathBuf,
}

fn checked_dir(path: &Path) -> Result<bool> {
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => anyhow::bail!("unsafe directory {}", path.display()),
            Component::CurDir => continue,
            _ => current.push(component.as_os_str()),
        }
        match std::fs::symlink_metadata(&current) {
            Ok(meta) => ensure!(meta.file_type().is_dir(), "{} is not a real directory", current.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e).with_context(|| format!("inspecting {}", current.display())),
        }
    }
    Ok(true)
}

fn temporary_name(name: &str, prefix: &str) -> bool {
    name.strip_prefix(prefix).is_some_and(|suffix| suffix.len() == 6 && suffix.bytes().all(|b| b.is_ascii_alphanumeric()))
}


fn collect(home: &HomePaths, id: &str, options: &CleanupOptions, expected: Option<&[CleanupItem]>) -> Result<Vec<CleanupItem>> {
    let mut parts = Path::new(id).components();
    ensure!(matches!(parts.next(), Some(Component::Normal(_))) && parts.next().is_none(), "invalid application id {id:?}");
    let key = hex::encode(Sha256::digest(id.as_bytes()));
    let locks = home.state_dir.join("update-locks");
    std::fs::create_dir_all(&locks)?;
    ensure!(checked_dir(&locks)?, "missing lock directory");
    let lock_path = locks.join(&key);
    if let Ok(meta) = std::fs::symlink_metadata(&lock_path) {
        ensure!(meta.file_type().is_file(), "invalid lock file {}", lock_path.display());
    }
    let app_lock = std::fs::OpenOptions::new().create(true).write(true).open(&lock_path)?;
    app_lock.try_lock().with_context(|| format!("another operation for '{id}' is running"))?;
    let store = Store::open(&home.db_path())?;
    ensure!(store.pending_update(id)?.is_none(), "application '{id}' has a pending update requiring recovery");

    let downloads = home.downloads().join(&key);
    let download_lock = if options.downloads && checked_dir(&downloads)? {
        let file = std::fs::File::open(&downloads)?;
        file.try_lock().with_context(|| format!("download in {} is active", downloads.display()))?;
        Some(file)
    } else { None };
    let mut items = Vec::new();
    if download_lock.is_some() {
        // A recorded path alone is insufficient: require the latest recorded
        // digest to match the bytes currently at that path.
        let events = store.events(Some(id), u32::MAX)?;
        let mut recorded = std::collections::HashMap::new();
        for event in events.iter().filter(|event| event.kind == "download_done") {
            let Some(value) = event.detail.as_deref().and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok()) else { continue };
            if let Some(path) = value.get("path").and_then(|v| v.as_str()) {
                recorded.entry(PathBuf::from(path)).or_insert_with(|| value.get("sha256").and_then(|v| v.as_str()).map(str::to_owned));
            }
        }
        for entry in std::fs::read_dir(&downloads)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                if let Some(Some(digest)) = recorded.get(&entry.path()) {
                    if crate::engine::ownership::sha256_file(&entry.path())? == *digest {
                        items.push(CleanupItem { category: CleanupCategory::Download, path: entry.path() });
                    }
                }
            }
        }
    }
    let app = home.installs().join(id);
    let retained = app.join("retained");
    if options.staging {
        for parent in [&app, &retained] {
            if !checked_dir(parent)? { continue; }
            for entry in std::fs::read_dir(parent)? {
                let entry = entry?;
                if !entry.file_type()?.is_dir() || !temporary_name(&entry.file_name().to_string_lossy(), ".updatium-staging-") { continue; }
                let path = entry.path();
                let marker = path.with_extension("owner");
                if std::fs::symlink_metadata(&marker).is_ok_and(|m| m.file_type().is_file())
                    && std::fs::read(&marker).is_ok_and(|bytes| bytes == b"updatium-staging-v1\n") {
                    items.push(CleanupItem { category: CleanupCategory::Staging, path });
                }
            }
        }
    }
    if options.snapshots && checked_dir(&retained)? {
        for entry in std::fs::read_dir(&retained)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() || !temporary_name(&entry.file_name().to_string_lossy(), "snapshot-") { continue; }
            let path = entry.path();
            let manifest = path.join("manifest.json");
            if !std::fs::symlink_metadata(&manifest).is_ok_and(|m| m.file_type().is_file()) || !checked_dir(&path.join("tree"))? { continue; }
            let value: serde_json::Value = match std::fs::read(&manifest).ok().and_then(|s| serde_json::from_slice(&s).ok()) { Some(v) => v, None => continue };
            if value.get("format").and_then(|v| v.as_u64()) != Some(1) || value.get("version").and_then(|v| v.as_str()).is_none_or(str::is_empty) || value.get("created_nanos").and_then(|v| v.as_u64()).is_none() { continue; }
            let mut valid = true;
            for child in std::fs::read_dir(&path)? {
                let name = child?.file_name();
                if name != "tree" && name != "manifest.json" { valid = false; break; }
            }
            if valid { items.push(CleanupItem { category: CleanupCategory::Snapshot, path }); }
        }
    }
    items.sort_by(|a, b| a.path.cmp(&b.path));
    if let Some(expected) = expected {
        ensure!(items == expected, "cleanup scope changed since preview; inspect it again");
        for item in &items {
            match item.category {
                CleanupCategory::Download => std::fs::remove_file(&item.path),
                CleanupCategory::Staging => {
                    std::fs::remove_dir_all(&item.path)?;
                    std::fs::remove_file(item.path.with_extension("owner"))
                }
                CleanupCategory::Snapshot => std::fs::remove_dir_all(&item.path),
            }.with_context(|| format!("removing {}", item.path.display()))?;
        }
    }
    drop(download_lock);
    drop(app_lock);
    Ok(items)
}

/// Inspect cleanup candidates; no artifacts are removed.
pub fn preview(home: &HomePaths, id: &str, options: &CleanupOptions) -> Result<Vec<CleanupItem>> {
    collect(home, id, options, None)
}

/// Rescan and remove selected categories under the same locks used by updates.
/// A preview is advisory: candidates can change before this call.
pub fn apply(home: &HomePaths, id: &str, options: &CleanupOptions, expected: &[CleanupItem]) -> Result<Vec<CleanupItem>> {
    collect(home, id, options, Some(expected))
}
