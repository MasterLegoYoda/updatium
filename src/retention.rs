//! Snapshots of known managed directory installations. Callers must hold the
//! application operation lock and establish ownership and supported install shape.
//! These snapshots do not cover external commands or application data.

use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::config::HomePaths;

const KEEP: usize = 2;
const PREFIX: &str = "snapshot-";

#[derive(Serialize, Deserialize)]
struct Manifest {
    format: u32,
    version: String,
    created_nanos: u128,
}

struct Snapshot {
    path: PathBuf,
    manifest: Manifest,
}

// Do not traverse symlinks in any existing component, including ancestors.
fn checked_directory(path: &Path) -> Result<()> {
    let mut prefix = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => bail!("unsafe directory path {}", path.display()),
            Component::CurDir => continue,
            _ => prefix.push(component.as_os_str()),
        }
        let metadata = std::fs::symlink_metadata(&prefix)
            .with_context(|| format!("inspecting {}", prefix.display()))?;
        ensure!(metadata.file_type().is_dir(), "{} is not a real directory", prefix.display());
    }
    Ok(())
}

fn paths(home: &HomePaths, id: &str) -> Result<(PathBuf, PathBuf)> {
    let mut components = Path::new(id).components();
    ensure!(matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none(), "invalid application id {id:?}");
    let app = home.installs().join(id);
    checked_directory(&app)?;
    Ok((app.join("current"), app.join("retained")))
}

fn snapshots(root: &Path) -> Result<Vec<Snapshot>> {
    match std::fs::symlink_metadata(root) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("inspecting {}", root.display())),
        Ok(_) => checked_directory(root)?,
    }
    let mut found = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with(PREFIX) {
            continue;
        }
        let path = entry.path();
        ensure!(entry.file_type()?.is_dir(), "invalid retained snapshot {}", path.display());
        let manifest_path = path.join("manifest.json");
        let meta = std::fs::symlink_metadata(&manifest_path)?;
        ensure!(meta.file_type().is_file(), "invalid snapshot manifest {}", manifest_path.display());
        let manifest: Manifest = serde_json::from_slice(&std::fs::read(&manifest_path)?)
            .with_context(|| format!("reading snapshot manifest {}", manifest_path.display()))?;
        ensure!(manifest.format == 1 && !manifest.version.is_empty(), "invalid snapshot manifest {}", manifest_path.display());
        checked_directory(&path.join("tree"))?;
        // Never remove a snapshot directory containing unexpected entries.
        for child in std::fs::read_dir(&path)? {
            let name = child?.file_name();
            ensure!(name == "tree" || name == "manifest.json", "unexpected entry in retained snapshot {}", path.display());
        }
        found.push(Snapshot { path, manifest });
    }
    found.sort_by(|a, b| b.manifest.created_nanos.cmp(&a.manifest.created_nanos).then_with(|| b.path.cmp(&a.path)));
    Ok(found)
}

// Reject symlinks and special files instead of following them outside the tree.
fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let source = entry.path();
        let dest = destination.join(entry.file_name());
        let metadata = std::fs::symlink_metadata(&source)?;
        if metadata.file_type().is_dir() {
            std::fs::create_dir(&dest)?;
            copy_tree(&source, &dest)?;
            std::fs::set_permissions(&dest, metadata.permissions())?;
        } else if metadata.file_type().is_file() {
            std::fs::copy(&source, &dest).with_context(|| format!("copying {}", source.display()))?;
            std::fs::set_permissions(&dest, metadata.permissions())?;
        } else {
            bail!("refusing to retain symlink or special file {}", source.display());
        }
    }
    Ok(())
}

/// Record a known managed current directory before replacement. Keeps the two
/// newest snapshots. The caller must not use this for external or adopted trees.
pub fn record(home: &HomePaths, id: &str, version: &str) -> Result<()> {
    ensure!(!version.is_empty(), "cannot retain an unknown version");
    let (current, root) = paths(home, id)?;
    checked_directory(&current)?;
    match std::fs::create_dir(&root) {
        Ok(()) => {},
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => checked_directory(&root)?,
        Err(e) => return Err(e).with_context(|| format!("creating {}", root.display())),
    }
    // Validate existing entries before modifying anything.
    let _ = snapshots(&root)?;
    let staged = tempfile::Builder::new().prefix(".staging-").tempdir_in(&root)?;
    let tree = staged.path().join("tree");
    std::fs::create_dir(&tree)?;
    copy_tree(&current, &tree)?;
    std::fs::set_permissions(&tree, std::fs::metadata(&current)?.permissions())?;
    let manifest = Manifest { format: 1, version: version.to_owned(), created_nanos: SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos() };
    std::fs::write(staged.path().join("manifest.json"), serde_json::to_vec(&manifest)?)?;
    let name = staged.path().file_name().expect("temporary directory has a name").to_string_lossy();
    let destination = root.join(format!("{PREFIX}{}", name.trim_start_matches(".staging-")));
    rustix::fs::renameat_with(rustix::fs::CWD, staged.path(), rustix::fs::CWD, &destination, rustix::fs::RenameFlags::NOREPLACE)
        .context("publishing retained snapshot")?;
    for snapshot in snapshots(&root)?.into_iter().skip(KEEP) {
        std::fs::remove_dir_all(&snapshot.path).with_context(|| format!("removing old snapshot {}", snapshot.path.display()))?;
    }
    Ok(())
}

/// List retained versions, newest first. Duplicate version strings are possible.
pub fn list(home: &HomePaths, id: &str) -> Result<Vec<String>> {
    let (_, root) = paths(home, id)?;
    Ok(snapshots(&root)?.into_iter().map(|s| s.manifest.version).collect())
}

/// Restore the newest retained snapshot with this exact version. The current
/// managed directory must exist. Snapshots remain available after restoration.
pub fn rollback(home: &HomePaths, id: &str, version: &str) -> Result<()> {
    let (current, root) = paths(home, id)?;
    checked_directory(&current)?;
    let snapshot = snapshots(&root)?.into_iter().find(|s| s.manifest.version == version)
        .with_context(|| format!("no retained snapshot for version {version:?}"))?;
    let staged = crate::installer::stage_directory(&current)?;
    copy_tree(&snapshot.path.join("tree"), staged.path())?;
    std::fs::set_permissions(staged.path(), std::fs::metadata(snapshot.path.join("tree"))?.permissions())?;
    // Recheck immediately before exchange; activate_directory also checks both.
    checked_directory(&current)?;
    crate::installer::activate_directory(staged.path(), &current)
        .context("restoring retained installation")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, HomePaths) {
        let tmp = tempfile::tempdir().unwrap();
        let home = HomePaths::resolve(Some(tmp.path())).unwrap();
        std::fs::create_dir_all(home.install_current("app")).unwrap();
        (tmp, home)
    }

    #[test]
    fn retains_two_and_restores_selected_version() {
        let (_tmp, home) = setup();
        let current = home.install_current("app");
        for version in ["one", "two", "three"] {
            std::fs::write(current.join("binary"), version).unwrap();
            record(&home, "app", version).unwrap();
        }
        assert_eq!(list(&home, "app").unwrap(), ["three", "two"]);
        std::fs::write(current.join("binary"), "new").unwrap();
        rollback(&home, "app", "two").unwrap();
        assert_eq!(std::fs::read_to_string(current.join("binary")).unwrap(), "two");
        assert!(rollback(&home, "app", "one").is_err());
        assert_eq!(list(&home, "app").unwrap(), ["three", "two"]);
    }

    #[test]
    fn rejects_unsafe_paths_and_symlinks() {
        let (_tmp, home) = setup();
        assert!(record(&home, "../other", "x").is_err());
        let current = home.install_current("app");
        std::os::unix::fs::symlink("/etc/passwd", current.join("link")).unwrap();
        assert!(record(&home, "app", "x").is_err());
        std::fs::remove_file(current.join("link")).unwrap();
        record(&home, "app", "x/../not-a-path").unwrap();
        std::fs::remove_dir_all(&current).unwrap();
        std::os::unix::fs::symlink("/tmp", &current).unwrap();
        assert!(rollback(&home, "app", "x/../not-a-path").is_err());
        assert_eq!(list(&home, "app").unwrap(), ["x/../not-a-path"]);
    }
}
