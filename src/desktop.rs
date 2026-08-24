//! freedesktop `.desktop` entry writer.
//!
//! Security: `exec` and `icon` are validated to contain neither `"` nor `\n`
//! before being written, preventing option-injection into generated launchers.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

fn assert_clean(field: &str, what: &str) -> Result<()> {
    if field.contains('"') || field.contains('\n') || field.contains('\\') {
        bail!("{what} contains forbidden characters (\n, \\, \"): {field:?}");
    }
    Ok(())
}

/// Escape a string per the Desktop Entry Specification (backslash escapes).
fn descape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            ' ' => out.push_str("\\s"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\\' => out.push_str("\\\\"), // unreachable post-validation, kept for safety
            other => out.push(other),
        }
    }
    out
}

/// Write `<dir>/updatium.<id>.desktop`. Returns the written path.
pub fn write_desktop(
    dir: &Path,
    id: &str,
    name: &str,
    exec_path: &Path,
    icon: Option<&Path>,
    categories: &[String],
    terminal: bool,
) -> Result<PathBuf> {
    let exec_str = exec_path.to_string_lossy();
    assert_clean(&exec_str, "exec path")?;
    let icon_str = icon.map(|p| p.to_string_lossy().into_owned());
    if let Some(i) = &icon_str {
        assert_clean(i, "icon path")?;
    }

    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(format!("updatium.{id}.desktop"));

    let mut cats = String::new();
    for c in categories {
        assert_clean(c, "category")?;
        cats.push_str(&descape(c));
        cats.push(';');
    }

    let content = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name={}\n\
         Exec={} %U\n\
         Terminal={}\n\
         Categories={}\n\
         Icon={}\n",
        descape(name),
        descape(&exec_str),
        terminal,
        cats,
        icon_str.as_deref().map(descape).unwrap_or_default(),
    );
    std::fs::write(&path, content).with_context(|| format!("writing {}", path.display()))?;

    // Best-effort cache refresh; absence/failure is fine.
    let _ = std::process::Command::new("update-desktop-database")
        .arg(dir)
        .status();

    Ok(path)
}

/// Delete `<dir>/updatium.<id>.desktop` if present (missing = ok).
pub fn remove_desktop(dir: &Path, id: &str) -> Result<()> {
    let path = dir.join(format!("updatium.{id}.desktop"));
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn golden_content_exact_match() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_desktop(
            dir.path(),
            "freecad",
            "FreeCAD",
            Path::new("/opt/apps/freecad/current/freecad.AppImage"),
            Some(Path::new("/opt/apps/freecad/current/icon.svg")),
            &["Graphics".to_string(), "Engineering".to_string()],
            false,
        )
        .unwrap();
        let expected = "[Desktop Entry]\n\
            Type=Application\n\
            Name=FreeCAD\n\
            Exec=/opt/apps/freecad/current/freecad.AppImage %U\n\
            Terminal=false\n\
            Categories=Graphics;Engineering;\n\
            Icon=/opt/apps/freecad/current/icon.svg\n";
        assert_eq!(std::fs::read_to_string(&p).unwrap(), expected);
        assert!(p.file_name().unwrap() == "updatium.freecad.desktop");
    }

    #[test]
    fn rejects_quotes_and_newlines() {
        let dir = tempfile::tempdir().unwrap();
        let evil_exec = Path::new("/bin/sh -c \"rm -rf ~\"");
        assert!(write_desktop(dir.path(), "x", "X", evil_exec, None, &[], false).is_err());
        let multiline = Path::new("/bad\nExec=evil");
        assert!(write_desktop(dir.path(), "x", "X", multiline, None, &[], false).is_err());
        // No file left behind on rejection.
        assert!(!dir.path().join("updatium.x.desktop").exists());
    }

    #[test]
    fn remove_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        write_desktop(
            dir.path(),
            "y",
            "Y",
            Path::new("/bin/true"),
            None,
            &[],
            true,
        )
        .unwrap();
        remove_desktop(dir.path(), "y").unwrap();
        remove_desktop(dir.path(), "y").unwrap(); // gone already: still ok
        assert!(!dir.path().join("updatium.y.desktop").exists());
    }
}
