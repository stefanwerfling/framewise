//! SQLite persistence: scan cache, profiles, jobs and history.
//!
//! Rows store their payload as JSON next to a few indexed columns. This keeps
//! the schema small and robust against model extensions (serde defaults).

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension, params};

use crate::core::error::{Error, Result};
use crate::encoding::profile::EncodingProfile;
use crate::media::model::MediaFile;
use crate::queue::job::{HistoryEntry, Job, JobId};
use crate::scanner::ScanCache;

const SCHEMA_VERSION: i32 = 1;

#[derive(Clone)]
pub struct Database {
    conn: Arc<Mutex<Connection>>,
}

impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Database")
    }
}

impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let version: i32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version < 1 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS media (
                    path TEXT PRIMARY KEY,
                    root TEXT NOT NULL,
                    size INTEGER NOT NULL,
                    mtime INTEGER NOT NULL,
                    data TEXT NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS media_root ON media(root);
                 CREATE TABLE IF NOT EXISTS profiles (
                    id TEXT PRIMARY KEY,
                    data TEXT NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS jobs (
                    id INTEGER PRIMARY KEY,
                    state TEXT NOT NULL,
                    data TEXT NOT NULL,
                    updated INTEGER NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS history (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    job_id INTEGER NOT NULL,
                    finished INTEGER NOT NULL,
                    data TEXT NOT NULL
                 );",
            )?;
        }
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        // A poisoned lock only means another thread panicked mid-operation;
        // SQLite itself stays consistent, so continue with the connection.
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ---- scan cache

    pub fn load_media_cache(&self, root: &Path) -> Result<ScanCache> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT data FROM media WHERE root = ?1")?;
        let rows = stmt.query_map(params![root.to_string_lossy()], |r| r.get::<_, String>(0))?;
        let mut cache = ScanCache::new();
        for row in rows {
            // Entries from older/incompatible versions are skipped (re-probed).
            if let Ok(m) = serde_json::from_str::<MediaFile>(&row?) {
                cache.insert(m.absolute_path.clone(), m);
            }
        }
        Ok(cache)
    }

    /// Replace the stored scan of `root` with `files`.
    pub fn replace_media(&self, root: &Path, files: &[MediaFile]) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let root_s = root.to_string_lossy();
        tx.execute("DELETE FROM media WHERE root = ?1", params![root_s])?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO media (path, root, size, mtime, data) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for f in files {
                stmt.execute(params![
                    f.absolute_path.to_string_lossy(),
                    root_s,
                    f.size_bytes as i64,
                    f.modified_unix,
                    serde_json::to_string(f)?
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    // ---- profiles

    /// Built-in profiles merged with stored ones (stored versions win, so
    /// edited built-ins persist).
    pub fn load_profiles(&self) -> Result<Vec<EncodingProfile>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT data FROM profiles ORDER BY id")?;
        let stored: Vec<EncodingProfile> = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .filter_map(|s| serde_json::from_str(&s).ok())
            .collect();
        let mut profiles = EncodingProfile::builtins();
        for p in stored {
            match profiles.iter_mut().find(|b| b.id == p.id) {
                Some(slot) => *slot = p,
                None => profiles.push(p),
            }
        }
        Ok(profiles)
    }

    pub fn save_profile(&self, profile: &EncodingProfile) -> Result<()> {
        self.lock().execute(
            "INSERT OR REPLACE INTO profiles (id, data) VALUES (?1, ?2)",
            params![profile.id, serde_json::to_string(profile)?],
        )?;
        Ok(())
    }

    pub fn delete_profile(&self, id: &str) -> Result<()> {
        self.lock()
            .execute("DELETE FROM profiles WHERE id = ?1", params![id])?;
        Ok(())
    }

    // ---- jobs

    pub fn next_job_id(&self) -> Result<JobId> {
        let conn = self.lock();
        let max_job: Option<i64> = conn.query_row("SELECT MAX(id) FROM jobs", [], |r| r.get(0))?;
        let max_hist: Option<i64> = conn
            .query_row("SELECT MAX(job_id) FROM history", [], |r| r.get(0))
            .optional()?
            .flatten();
        Ok(max_job.unwrap_or(0).max(max_hist.unwrap_or(0)) + 1)
    }

    pub fn save_job(&self, job: &Job) -> Result<()> {
        self.lock().execute(
            "INSERT OR REPLACE INTO jobs (id, state, data, updated) VALUES (?1, ?2, ?3, ?4)",
            params![
                job.id,
                job.state.as_db(),
                serde_json::to_string(job)?,
                crate::queue::job::now_unix()
            ],
        )?;
        Ok(())
    }

    pub fn load_jobs(&self) -> Result<Vec<Job>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT data FROM jobs ORDER BY id")?;
        let jobs = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .filter_map(|s| match serde_json::from_str::<Job>(&s) {
                Ok(j) => Some(j),
                Err(e) => {
                    tracing::warn!("skipping unreadable job row: {e}");
                    None
                }
            })
            .collect();
        Ok(jobs)
    }

    pub fn delete_job(&self, id: JobId) -> Result<()> {
        self.lock()
            .execute("DELETE FROM jobs WHERE id = ?1", params![id])?;
        Ok(())
    }

    // ---- history

    pub fn add_history(&self, entry: &HistoryEntry) -> Result<i64> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO history (job_id, finished, data) VALUES (?1, ?2, ?3)",
            params![
                entry.job_id,
                entry.finished_unix,
                serde_json::to_string(entry)?
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn load_history(&self, limit: usize) -> Result<Vec<HistoryEntry>> {
        let conn = self.lock();
        let mut stmt =
            conn.prepare("SELECT id, data FROM history ORDER BY finished DESC, id DESC LIMIT ?1")?;
        let entries = stmt
            .query_map(params![limit as i64], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })?
            .filter_map(|r| r.ok())
            .filter_map(|(id, s)| {
                serde_json::from_str::<HistoryEntry>(&s).ok().map(|mut e| {
                    e.id = id;
                    e
                })
            })
            .collect();
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::plan::{PlanCapabilities, build_plan};
    use crate::media::fixtures::*;
    use crate::queue::job::JobState;

    fn sample() -> MediaFile {
        media(
            "mkv",
            123,
            60.0,
            vec![video("h264", 1280, 720)],
            vec![audio("ac3", None, 6, "ger")],
            vec![subtitle("subrip", "eng")],
        )
    }

    #[test]
    fn media_cache_roundtrip() {
        let db = Database::open_in_memory().unwrap();
        let root = Path::new("/src");
        db.replace_media(root, &[sample()]).unwrap();
        let cache = db.load_media_cache(root).unwrap();
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.values().next().unwrap(), &sample());
        assert!(db.load_media_cache(Path::new("/other")).unwrap().is_empty());
        db.replace_media(root, &[]).unwrap();
        assert!(db.load_media_cache(root).unwrap().is_empty());
    }

    #[test]
    fn profiles_merge_with_builtins() {
        let db = Database::open_in_memory().unwrap();
        assert_eq!(db.load_profiles().unwrap().len(), 4);
        let mut u = EncodingProfile::universal();
        u.video.crf = 22;
        db.save_profile(&u).unwrap();
        db.save_profile(&EncodingProfile::custom("mine", "Mine"))
            .unwrap();
        let p = db.load_profiles().unwrap();
        assert_eq!(p.len(), 5);
        assert_eq!(
            p.iter().find(|p| p.id == "universal").unwrap().video.crf,
            22
        );
        db.delete_profile("mine").unwrap();
        assert_eq!(db.load_profiles().unwrap().len(), 4);
    }

    #[test]
    fn jobs_and_history_persist_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.sqlite");
        {
            let db = Database::open(&path).unwrap();
            let m = sample();
            let plan = build_plan(
                &m,
                &EncodingProfile::universal(),
                &PlanCapabilities::default(),
            );
            let id = db.next_job_id().unwrap();
            let mut job = Job::new(id, m, "/t/Movie.mp4".into(), "universal".into(), plan);
            job.state = JobState::Encoding;
            db.save_job(&job).unwrap();
            job.state = JobState::Done;
            job.result_size = Some(50);
            db.add_history(&HistoryEntry::from_job(&job)).unwrap();
        }
        let db = Database::open(&path).unwrap();
        let jobs = db.load_jobs().unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].state, JobState::Encoding);
        let h = db.load_history(10).unwrap();
        assert_eq!(h.len(), 1);
        assert!((h[0].savings_percent().unwrap() - 59.35).abs() < 0.1);
        assert_eq!(db.next_job_id().unwrap(), 2);
        db.delete_job(1).unwrap();
        assert!(db.load_jobs().unwrap().is_empty());
    }
}
