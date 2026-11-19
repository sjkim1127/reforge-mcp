use super::model::FileStamp;
use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde::{Serialize, de::DeserializeOwned};
use std::path::{Path, PathBuf};

pub struct Ledger {
    pub root: PathBuf,
}
impl Ledger {
    pub fn new(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root)?;
        let this = Self {
            root: root.canonicalize()?,
        };
        this.connection()?.execute_batch("PRAGMA journal_mode=WAL;
          CREATE TABLE IF NOT EXISTS migration_records (id TEXT PRIMARY KEY,campaign_id TEXT NOT NULL,parent_id TEXT NOT NULL,kind TEXT NOT NULL,record TEXT NOT NULL);
          CREATE INDEX IF NOT EXISTS migration_parent ON migration_records(parent_id,kind);
          CREATE INDEX IF NOT EXISTS migration_campaign ON migration_records(campaign_id,kind);
          CREATE TABLE IF NOT EXISTS migration_files (owner_id TEXT NOT NULL,path TEXT NOT NULL,record TEXT NOT NULL,PRIMARY KEY(owner_id,path));
          CREATE TABLE IF NOT EXISTS migration_events (sequence INTEGER PRIMARY KEY AUTOINCREMENT,campaign_id TEXT NOT NULL,object_id TEXT NOT NULL,kind TEXT NOT NULL,record TEXT NOT NULL,created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')));")?;
        Ok(this)
    }
    pub fn connection(&self) -> Result<Connection> {
        let conn = Connection::open(self.root.join("porting.db"))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(conn)
    }
    pub fn save<T: Serialize>(
        &self,
        id: &str,
        campaign: &str,
        parent: &str,
        kind: &str,
        value: &T,
        files: Option<&[FileStamp]>,
    ) -> Result<()> {
        let mut conn = self.connection()?;
        let tx = conn.transaction()?;
        let record = serde_json::to_string(value)?;
        tx.execute("INSERT INTO migration_records VALUES (?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET record=excluded.record",params![id,campaign,parent,kind,record])?;
        tx.execute(
            "INSERT INTO migration_events(campaign_id,object_id,kind,record) VALUES (?1,?2,?3,?4)",
            params![campaign, id, kind, record],
        )?;
        if let Some(files) = files {
            let mut statement = tx.prepare("INSERT INTO migration_files VALUES (?1,?2,?3)")?;
            for file in files {
                statement.execute(params![id, file.path, serde_json::to_string(file)?])?;
            }
        }
        tx.commit()?;
        Ok(())
    }
    pub fn load<T: DeserializeOwned>(&self, id: &str, kind: &str) -> Result<T> {
        let json: String = self
            .connection()?
            .query_row(
                "SELECT record FROM migration_records WHERE id=?1 AND kind=?2",
                params![id, kind],
                |r| r.get(0),
            )
            .context("migration object not found")?;
        Ok(serde_json::from_str(&json)?)
    }
    pub fn list<T: DeserializeOwned>(
        &self,
        parent: &str,
        kind: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<T>> {
        let conn = self.connection()?;
        let mut stmt=conn.prepare("SELECT record FROM migration_records WHERE parent_id=?1 AND kind=?2 ORDER BY rowid LIMIT ?3 OFFSET ?4")?;
        let rows = stmt.query_map(
            params![parent, kind, i64::try_from(limit)?, i64::try_from(offset)?],
            |r| r.get::<_, String>(0),
        )?;
        rows.map(|r| Ok(serde_json::from_str(&r?)?)).collect()
    }
    pub fn count(&self, parent: &str, kind: &str) -> Result<usize> {
        Ok(self.connection()?.query_row(
            "SELECT count(*) FROM migration_records WHERE parent_id=?1 AND kind=?2",
            params![parent, kind],
            |r| r.get::<_, i64>(0),
        )? as usize)
    }
    pub fn files(&self, owner: &str, offset: usize, limit: usize) -> Result<Vec<FileStamp>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "SELECT record FROM migration_files WHERE owner_id=?1 ORDER BY path LIMIT ?2 OFFSET ?3",
        )?;
        let rows = stmt.query_map(
            params![owner, i64::try_from(limit)?, i64::try_from(offset)?],
            |r| r.get::<_, String>(0),
        )?;
        rows.map(|r| Ok(serde_json::from_str(&r?)?)).collect()
    }
    pub fn file(&self, owner: &str, path: &str) -> Result<FileStamp> {
        let json: String = self
            .connection()?
            .query_row(
                "SELECT record FROM migration_files WHERE owner_id=?1 AND path=?2",
                params![owner, path],
                |r| r.get(0),
            )
            .context("file is not in the pinned inventory")?;
        Ok(serde_json::from_str(&json)?)
    }
    pub fn event_count(&self, campaign: &str) -> Result<usize> {
        Ok(self.connection()?.query_row(
            "SELECT count(*) FROM migration_events WHERE campaign_id=?1",
            [campaign],
            |r| r.get::<_, i64>(0),
        )? as usize)
    }
}
