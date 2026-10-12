use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use rusqlite::{Connection, params};
use std::path::{Path, PathBuf};

use crate::model::Job;

pub struct Store {
    root: PathBuf,
}
impl Store {
    pub fn new(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root)?;
        let this = Self {
            root: root.canonicalize()?,
        };
        this.connection()?.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS porting_jobs (id TEXT PRIMARY KEY, record TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS porting_events (
               sequence INTEGER PRIMARY KEY AUTOINCREMENT,
               job_id TEXT NOT NULL, state TEXT NOT NULL, record TEXT NOT NULL,
               created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             );",
        )?;
        Ok(this)
    }
    fn connection(&self) -> Result<Connection> {
        let conn = Connection::open(self.root.join("porting.db"))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(conn)
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn directory(&self, id: &str) -> Result<PathBuf> {
        ensure!(
            !id.is_empty()
                && id.len() < 128
                && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "invalid job_id"
        );
        Ok(self.root.join("jobs").join(id))
    }
    pub fn lock(&self, id: &str) -> Result<JobLock> {
        let dir = self.directory(id)?;
        ensure!(dir.is_dir(), "job not found");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join("job.lock"))?;
        file.try_lock_exclusive()
            .context("another operation is running for this job")?;
        Ok(JobLock(file))
    }
    pub fn save(&self, job: &Job) -> Result<()> {
        let mut conn = self.connection()?;
        let tx = conn.transaction()?;
        let record = serde_json::to_string(job)?;
        tx.execute("INSERT INTO porting_jobs VALUES (?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record",params![job.job_id,record])?;
        tx.execute(
            "INSERT INTO porting_events (job_id,state,record) VALUES (?1,?2,?3)",
            params![job.job_id, job.state, record],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn load(&self, id: &str) -> Result<Job> {
        self.directory(id)?;
        let record: String = self
            .connection()?
            .query_row("SELECT record FROM porting_jobs WHERE id=?1", [id], |row| {
                row.get(0)
            })
            .context("job not found")?;
        Ok(serde_json::from_str(&record)?)
    }
    pub fn list(&self) -> Result<Vec<Job>> {
        let conn = self.connection()?;
        let mut stmt =
            conn.prepare("SELECT record FROM porting_jobs ORDER BY rowid DESC LIMIT 100")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.map(|r| Ok(serde_json::from_str(&r?)?)).collect()
    }
}

/// Unlock explicitly: a concurrently spawned process may temporarily inherit an
/// open-file description between fork and exec even with close-on-exec enabled.
pub struct JobLock(std::fs::File);
impl Drop for JobLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}
