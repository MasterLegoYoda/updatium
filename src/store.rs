//! SQLite-backed state: known versions, install state, integrity hashes, event log.

use anyhow::{Context, Result, bail};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension};
use std::path::Path;

pub const EVENT_KINDS: &[&str] = &[
    "check_ok",
    "check_error",
    "update_available",
    "download_done",
    "verify_failed",
    "script_changed",
    "approved",
    "install_started",
    "install_ok",
    "install_failed",
    "applied",
    "notified",
];

const MIGRATION_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS apps(
  id TEXT PRIMARY KEY, provider TEXT NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 1, created_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS known_versions(
  app_id TEXT NOT NULL REFERENCES apps(id), version TEXT NOT NULL,
  first_seen TEXT NOT NULL, PRIMARY KEY(app_id, version));
CREATE TABLE IF NOT EXISTS app_state(
  app_id TEXT PRIMARY KEY REFERENCES apps(id),
  installed_version TEXT, installed_at TEXT,
  script_hash TEXT, last_check TEXT, last_error TEXT);
CREATE TABLE IF NOT EXISTS events(
  seq INTEGER PRIMARY KEY AUTOINCREMENT, ts TEXT NOT NULL,
  app_id TEXT, kind TEXT NOT NULL, detail TEXT);
"#;

pub struct Store {
    conn: Connection,
}

#[derive(Debug, Clone)]
pub struct AppRow {
    pub id: String,
    pub provider: String,
    pub enabled: bool,
}

#[derive(Debug, Clone)]
pub struct AppState {
    pub installed_version: Option<String>,
    pub installed_at: Option<String>,
    pub script_hash: Option<String>,
    pub last_check: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EventRow {
    pub seq: i64,
    pub ts: String,
    pub app_id: Option<String>,
    pub kind: String,
    pub detail: Option<String>,
}

fn now() -> String {
    Utc::now().to_rfc3339()
}

impl Store {
    /// Open (creating dirs/schema as needed). WAL + busy timeout per contract.
    pub fn open(path: &Path) -> Result<Store> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        let s = Store { conn };
        s.apply_pragmas()?;
        Ok(s)
    }

    /// In-memory store for tests and short-lived reads.
    pub fn open_memory() -> Result<Store> {
        let s = Store {
            conn: Connection::open_in_memory()?,
        };
        s.apply_pragmas()?;
        Ok(s)
    }

    fn apply_pragmas(&self) -> Result<()> {
        // WAL is a no-op on :memory: databases; harmless either way.
        self.conn
            .pragma_update(None, "journal_mode", "WAL")
            .context("setting journal_mode=WAL")?;
        self.conn
            .busy_timeout(std::time::Duration::from_millis(5000))?;
        self.conn.execute_batch(MIGRATION_SQL)?;
        Ok(())
    }

    /// Insert apps that don't exist yet; refresh provider/enabled for existing
    /// ones. NEVER deletes apps absent from the input.
    pub fn sync_apps(&self, apps: &[(&str, &str, bool)]) -> Result<()> {
        let mut stmt = self.conn.prepare_cached(
            "INSERT INTO apps(id, provider, enabled, created_at) VALUES(?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET provider = excluded.provider,
                                          enabled = excluded.enabled",
        )?;
        for (id, provider, enabled) in apps {
            stmt.execute((id, provider, *enabled as i64, now()))?;
        }
        Ok(())
    }

    pub fn list_apps(&self) -> Result<Vec<AppRow>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, provider, enabled FROM apps ORDER BY id")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(AppRow {
                    id: r.get(0)?,
                    provider: r.get(1)?,
                    enabled: r.get::<_, i64>(2)? != 0,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Synchronize an app's known versions with the current provider result:
    /// insert unseen versions (first_seen preserved), then drop versions the
    /// provider no longer advertises (e.g. filtered out by a tightened regex).
    /// Returns the number of newly inserted versions.
    pub fn record_versions(&self, app: &str, versions: &[String]) -> Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        let mut added = 0;
        let ts = now();
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR IGNORE INTO known_versions(app_id, version, first_seen)
                 VALUES(?1, ?2, ?3)",
            )?;
            for v in versions {
                added += stmt.execute((app, v, &ts))?;
            }
        }

        if versions.is_empty() {
            tx.execute("DELETE FROM known_versions WHERE app_id = ?1", [app])?;
        } else {
            let placeholders = vec!["?"; versions.len()].join(",");
            let sql = format!(
                "DELETE FROM known_versions WHERE app_id = ?1 AND version NOT IN ({placeholders})"
            );
            let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(versions.len() + 1);
            params.push(&app);
            for v in versions {
                params.push(v);
            }
            tx.execute(&sql, params.as_slice())?;
        }
        tx.commit()?;
        Ok(added)
    }

    /// Highest known version for `app` under `cmp_version` ordering.
    pub fn latest_known(&self, app: &str) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT version FROM known_versions WHERE app_id = ?1")?;
        let mut best: Option<String> = None;
        let mut rows = stmt.query([app])?;
        while let Some(r) = rows.next()? {
            let v: String = r.get(0)?;
            match &best {
                None => best = Some(v),
                Some(b) => {
                    if crate::version::cmp_version(&v, b) == std::cmp::Ordering::Greater {
                        best = Some(v);
                    }
                }
            }
        }
        Ok(best)
    }

    pub fn known_versions(&self, app: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT version FROM known_versions WHERE app_id = ?1")?;
        let rows = stmt
            .query_map([app], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn set_installed(&self, app: &str, version: &str) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO app_state(app_id, installed_version, installed_at)
             VALUES(?1, ?2, ?3)
             ON CONFLICT(app_id) DO UPDATE SET installed_version = excluded.installed_version,
                                               installed_at = excluded.installed_at",
            )?
            .execute((app, version, now()))?;
        Ok(())
    }

    pub fn script_hash(&self, app: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT script_hash FROM app_state WHERE app_id = ?1",
                [app],
                |r| r.get(0),
            )
            .optional()
            .map(|o| o.flatten())
            .map_err(Into::into)
    }

    pub fn set_script_hash(&self, app: &str, hash: Option<&str>) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO app_state(app_id, script_hash) VALUES(?1, ?2)
             ON CONFLICT(app_id) DO UPDATE SET script_hash = excluded.script_hash",
            )?
            .execute((app, hash))?;
        Ok(())
    }

    /// Update last_check (and clear/set last_error).
    pub fn touch_check(&self, app: &str, err: Option<&str>) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO app_state(app_id, last_check, last_error) VALUES(?1, ?2, ?3)
             ON CONFLICT(app_id) DO UPDATE SET last_check = excluded.last_check,
                                               last_error = excluded.last_error",
            )?
            .execute((app, now(), err))?;
        Ok(())
    }

    pub fn state(&self, app: &str) -> Result<Option<AppState>> {
        self.conn
            .query_row(
                "SELECT installed_version, installed_at, script_hash, last_check, last_error
                 FROM app_state WHERE app_id = ?1",
                [app],
                |r| {
                    Ok(AppState {
                        installed_version: r.get(0)?,
                        installed_at: r.get(1)?,
                        script_hash: r.get(2)?,
                        last_check: r.get(3)?,
                        last_error: r.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// Append one event. `detail` is pre-serialized JSON (or free text).
    pub fn event(&self, kind: &str, app: Option<&str>, detail: Option<&str>) -> Result<i64> {
        if !EVENT_KINDS.contains(&kind) {
            bail!(
                "unknown event kind '{kind}' (valid: {})",
                EVENT_KINDS.join(", ")
            );
        }
        self.conn.execute(
            "INSERT INTO events(ts, app_id, kind, detail) VALUES(?1, ?2, ?3, ?4)",
            (now(), app, kind, detail),
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Newest-first events, optionally filtered by app.
    pub fn events(&self, app: Option<&str>, limit: u32) -> Result<Vec<EventRow>> {
        let sql = match app {
            Some(_) => {
                "SELECT seq, ts, app_id, kind, detail FROM events WHERE app_id = ?1
                 ORDER BY seq DESC LIMIT ?2"
            }
            None => "SELECT seq, ts, app_id, kind, detail FROM events ORDER BY seq DESC LIMIT ?1",
        };
        let mut stmt = self.conn.prepare(sql)?;
        let map = |r: &rusqlite::Row| {
            Ok(EventRow {
                seq: r.get(0)?,
                ts: r.get(1)?,
                app_id: r.get(2)?,
                kind: r.get(3)?,
                detail: r.get(4)?,
            })
        };
        let rows = match app {
            Some(a) => stmt.query_map(rusqlite::params![a, limit], map)?,
            None => stmt.query_map(rusqlite::params![limit], map)?,
        };
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_and_sync_roundtrip() {
        let s = Store::open_memory().unwrap();
        assert!(s.list_apps().unwrap().is_empty());
        s.sync_apps(&[("a", "github-releases", true), ("b", "command", false)])
            .unwrap();
        // Re-sync updates fields, never duplicates, never deletes.
        s.sync_apps(&[("a", "html-links", false)]).unwrap();
        let apps = s.list_apps().unwrap();
        assert_eq!(apps.len(), 2);
        let a = apps.iter().find(|x| x.id == "a").unwrap();
        assert_eq!(a.provider, "html-links");
        assert!(!a.enabled);
        assert!(apps.iter().any(|x| x.id == "b"));
    }

    #[test]
    fn versions_unique_and_latest() {
        let s = Store::open_memory().unwrap();
        s.sync_apps(&[("a", "github-releases", true)]).unwrap();
        let n = s
            .record_versions("a", &["1.9".into(), "1.10".into()])
            .unwrap();
        assert_eq!(n, 2);
        // Duplicate inserts ignored; versions no longer advertised are pruned.
        let n = s
            .record_versions("a", &["1.10".into(), "2.0".into()])
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(s.known_versions("a").unwrap().len(), 2);
        assert!(!s.known_versions("a").unwrap().contains(&"1.9".to_string()));
        assert_eq!(s.latest_known("a").unwrap(), Some("2.0".to_string()));
        assert_eq!(s.latest_known("ghost").unwrap(), None);

        s.record_versions("a", &[]).unwrap();
        assert!(s.known_versions("a").unwrap().is_empty());
    }

    #[test]
    fn state_roundtrip() {
        let s = Store::open_memory().unwrap();
        s.sync_apps(&[("a", "command", true)]).unwrap();
        assert!(s.state("a").unwrap().is_none());
        s.touch_check("a", None).unwrap();
        let st = s.state("a").unwrap().unwrap();
        assert!(st.installed_version.is_none());
        assert!(st.last_check.is_some());
        assert!(st.last_error.is_none());

        s.set_installed("a", "1.0").unwrap();
        s.set_script_hash("a", Some("abc")).unwrap();
        assert_eq!(s.script_hash("a").unwrap(), Some("abc".into()));
        s.set_script_hash("a", None).unwrap();
        assert_eq!(s.script_hash("a").unwrap(), None);

        s.touch_check("a", Some("boom")).unwrap();
        let st = s.state("a").unwrap().unwrap();
        assert_eq!(st.installed_version.as_deref(), Some("1.0"));
        assert_eq!(st.last_error.as_deref(), Some("boom"));
    }

    #[test]
    fn events_closed_set_and_filter() {
        let s = Store::open_memory().unwrap();
        s.event("check_ok", Some("a"), Some(r#"{"v":"1.0"}"#))
            .unwrap();
        s.event("update_available", Some("b"), None).unwrap();
        assert!(s.event("bogus_kind", None, None).is_err());

        let all = s.events(None, 10).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].kind, "update_available"); // newest first

        let only_a = s.events(Some("a"), 10).unwrap();
        assert_eq!(only_a.len(), 1);
        assert_eq!(only_a[0].app_id.as_deref(), Some("a"));
        assert_eq!(only_a[0].detail.as_deref(), Some(r#"{"v":"1.0"}"#));

        let one = s.events(None, 1).unwrap();
        assert_eq!(one.len(), 1);
    }

    #[test]
    fn file_backed_store_wal() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state.db");
        let s = Store::open(&p).unwrap();
        s.sync_apps(&[("x", "command", true)]).unwrap();
        drop(s);
        let s2 = Store::open(&p).unwrap();
        assert_eq!(s2.list_apps().unwrap()[0].id, "x");
    }
}
