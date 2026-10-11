use std::path::{Path, PathBuf};
use std::str::FromStr;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, ffi::sqlite3_auto_extension, params};
use sqlite_vec::sqlite3_vec_init;
use std::sync::Once;
use tracing::debug;

use crate::error::StorageError;
use crate::types::{CallEdge, RepoMeta, Symbol, SymbolKind, SymbolSource};

pub const CACHE_DIR_ENV: &str = "FOSSIL_CACHE_DIR";

/// Manages the SQLite index database globally across all repositories.
///
/// All repositories share the same `global.db` file.
pub struct GlobalStore {
    conn: Connection,
}

impl GlobalStore {
    /// Open (or create) the index database at `db_path`.
    pub fn open(db_path: &Path) -> Result<Self, StorageError> {
        if db_path != Path::new(":memory:")
            && let Some(parent) = db_path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }

        // Register the sqlite-vec extension globally for all connections (once).
        static INIT_VEC: Once = Once::new();
        INIT_VEC.call_once(|| unsafe {
            sqlite3_auto_extension(Some(std::mem::transmute::<
                *const (),
                unsafe extern "C" fn(
                    *mut rusqlite::ffi::sqlite3,
                    *mut *mut std::os::raw::c_char,
                    *const rusqlite::ffi::sqlite3_api_routines,
                ) -> i32,
            >(sqlite3_vec_init as *const ())));
        });

        let conn = Connection::open(db_path)?;
        // vec0 requires loading it if auto_extension doesn't immediately apply to the very first connection sometimes, but auto_extension applies to newly opened connections.

        let store = Self { conn };
        store.init_schema()?;
        Ok(store)
    }

    /// Initialise tables and indices if they don't already exist.
    fn init_schema(&self) -> Result<(), StorageError> {
        self.conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;

            CREATE TABLE IF NOT EXISTS repos (
                repo_id     TEXT PRIMARY KEY,
                url         TEXT NOT NULL,
                alias       TEXT,
                path        TEXT NOT NULL,
                indexed_at  TEXT,
                last_accessed_at TEXT,
                symbol_count INTEGER DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS symbols (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                repo_id         TEXT NOT NULL,
                name            TEXT NOT NULL,
                kind            TEXT NOT NULL,
                file_path       TEXT NOT NULL,
                line_start      INTEGER NOT NULL,
                line_end        INTEGER NOT NULL,
                signature       TEXT NOT NULL,
                language        TEXT NOT NULL,
                -- Dependency metadata (NULL for user code)
                source          TEXT NOT NULL DEFAULT 'user_code',
                package_name    TEXT,
                package_version TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_symbols_repo_id ON symbols(repo_id);
            CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(name);
            CREATE INDEX IF NOT EXISTS idx_symbols_language ON symbols(language);

            CREATE TABLE IF NOT EXISTS call_edges (
                id        INTEGER PRIMARY KEY AUTOINCREMENT,
                repo_id   TEXT NOT NULL,
                caller    TEXT NOT NULL,
                callee    TEXT NOT NULL,
                file_path TEXT NOT NULL,
                line      INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_call_edges_repo_id ON call_edges(repo_id);
            CREATE INDEX IF NOT EXISTS idx_call_edges_caller ON call_edges(caller);
            CREATE INDEX IF NOT EXISTS idx_call_edges_callee ON call_edges(callee);

            -- Tracks which external packages have already been indexed (for cache).
            -- UNIQUE on (name, version, language) prevents duplicate indexing.
            CREATE TABLE IF NOT EXISTS indexed_packages (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                name         TEXT NOT NULL,
                version      TEXT NOT NULL,
                language     TEXT NOT NULL,
                source_path  TEXT NOT NULL,
                symbol_count INTEGER NOT NULL DEFAULT 0,
                indexed_at   INTEGER NOT NULL,
                UNIQUE(name, version, language)
            );

            CREATE INDEX IF NOT EXISTS idx_indexed_packages_lang ON indexed_packages(language);

            -- Vector table for semantic search. We use 384 dimensions for BGE-small-en-v1.5
            CREATE VIRTUAL TABLE IF NOT EXISTS vec_symbols USING vec0(
                symbol_id INTEGER PRIMARY KEY,
                embedding float[384]
            );

            CREATE TABLE IF NOT EXISTS goals (
                id TEXT PRIMARY KEY, title TEXT NOT NULL, description TEXT,
                status TEXT NOT NULL DEFAULT 'active', created_at TEXT NOT NULL, updated_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS roadmap_items (
                id TEXT PRIMARY KEY, goal_id TEXT NOT NULL REFERENCES goals(id) ON DELETE CASCADE,
                title TEXT NOT NULL, description TEXT, status TEXT NOT NULL DEFAULT 'planned',
                position INTEGER NOT NULL, depends_on TEXT NOT NULL DEFAULT '[]',
                created_at TEXT NOT NULL, updated_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_roadmap_items_goal ON roadmap_items(goal_id, position);
            CREATE TABLE IF NOT EXISTS roadmap_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT, goal_id TEXT NOT NULL,
                item_id TEXT, event_type TEXT NOT NULL, from_status TEXT,
                to_status TEXT, created_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_roadmap_events_goal ON roadmap_events(goal_id, created_at);
            ",
        )?;
        // Dynamic migrations for existing DBs (ignore errors if column already exists)
        let _ = self
            .conn
            .execute("ALTER TABLE repos ADD COLUMN last_accessed_at TEXT", []);
        let _ = self.conn.execute(
            "ALTER TABLE symbols ADD COLUMN source TEXT NOT NULL DEFAULT 'user_code'",
            [],
        );
        let _ = self
            .conn
            .execute("ALTER TABLE symbols ADD COLUMN package_name TEXT", []);
        let _ = self
            .conn
            .execute("ALTER TABLE symbols ADD COLUMN package_version TEXT", []);

        // Create indices for new columns after they are guaranteed to exist
        self.conn.execute_batch(
            "
            CREATE INDEX IF NOT EXISTS idx_symbols_source ON symbols(source);
            CREATE INDEX IF NOT EXISTS idx_symbols_package ON symbols(package_name, package_version);
            ",
        )?;
        Ok(())
    }

    pub fn create_goal(&self, goal: &crate::types::Goal) -> Result<(), StorageError> {
        self.conn.execute(
            "INSERT INTO goals VALUES (?1,?2,?3,?4,?5,?6)",
            params![
                goal.id,
                goal.title,
                goal.description,
                goal.status,
                goal.created_at.to_rfc3339(),
                goal.updated_at.to_rfc3339()
            ],
        )?;
        self.record_roadmap_event(
            &goal.id,
            None,
            "goal_created",
            None,
            Some(&goal.status),
            goal.created_at,
        )?;
        Ok(())
    }
    fn record_roadmap_event(
        &self,
        goal_id: &str,
        item_id: Option<&str>,
        event_type: &str,
        from_status: Option<&str>,
        to_status: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<(), StorageError> {
        self.conn.execute("INSERT INTO roadmap_events (goal_id,item_id,event_type,from_status,to_status,created_at) VALUES (?1,?2,?3,?4,?5,?6)", params![goal_id,item_id,event_type,from_status,to_status,now.to_rfc3339()])?;
        Ok(())
    }
    pub fn roadmap_history(
        &self,
        goal_id: &str,
    ) -> Result<Vec<crate::types::RoadmapEvent>, StorageError> {
        let mut s=self.conn.prepare("SELECT id,goal_id,item_id,event_type,from_status,to_status,created_at FROM roadmap_events WHERE goal_id=?1 ORDER BY id DESC")?;
        let rows = s.query_map(params![goal_id], |r| {
            Ok(crate::types::RoadmapEvent {
                id: r.get(0)?,
                goal_id: r.get(1)?,
                item_id: r.get(2)?,
                event_type: r.get(3)?,
                from_status: r.get(4)?,
                to_status: r.get(5)?,
                created_at: DateTime::parse_from_rfc3339(&r.get::<_, String>(6)?)
                    .map_err(|_| rusqlite::Error::InvalidQuery)?
                    .with_timezone(&Utc),
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::from)
    }
    pub fn list_goals(&self) -> Result<Vec<crate::types::Goal>, StorageError> {
        let mut stmt = self.conn.prepare("SELECT id,title,description,status,created_at,updated_at FROM goals ORDER BY updated_at DESC")?;
        let rows = stmt.query_map([], |r| {
            Ok(crate::types::Goal {
                id: r.get(0)?,
                title: r.get(1)?,
                description: r.get(2)?,
                status: r.get(3)?,
                created_at: DateTime::parse_from_rfc3339(&r.get::<_, String>(4)?)
                    .map_err(|_| rusqlite::Error::InvalidQuery)?
                    .with_timezone(&Utc),
                updated_at: DateTime::parse_from_rfc3339(&r.get::<_, String>(5)?)
                    .map_err(|_| rusqlite::Error::InvalidQuery)?
                    .with_timezone(&Utc),
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::from)
    }
    pub fn update_goal(
        &self,
        id: &str,
        title: Option<&str>,
        description: Option<&str>,
        status: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<(), StorageError> {
        if status == Some("completed") {
            let incomplete: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM roadmap_items WHERE goal_id=?1 AND status!='done'",
                params![id],
                |r| r.get(0),
            )?;
            if incomplete > 0 {
                return Err(StorageError::InvalidRoadmap(format!(
                    "goal cannot be completed while {} milestone(s) remain unfinished",
                    incomplete
                )));
            }
        }
        let old_status: String = self
            .conn
            .query_row("SELECT status FROM goals WHERE id=?1", params![id], |r| {
                r.get(0)
            })
            .map_err(|e| {
                if matches!(e, rusqlite::Error::QueryReturnedNoRows) {
                    StorageError::InvalidRoadmap(format!("goal '{}' does not exist", id))
                } else {
                    StorageError::Sqlite(e)
                }
            })?;
        let changed = self.conn.execute("UPDATE goals SET title=COALESCE(?1,title), description=COALESCE(?2,description), status=COALESCE(?3,status), updated_at=?4 WHERE id=?5", params![title, description, status, now.to_rfc3339(), id])?;
        if changed == 0 {
            return Err(StorageError::InvalidRoadmap(format!(
                "goal '{}' does not exist",
                id
            )));
        }
        if let Some(new_status) = status
            && new_status != old_status
        {
            self.record_roadmap_event(
                id,
                None,
                "goal_status_changed",
                Some(&old_status),
                Some(new_status),
                now,
            )?;
        }
        Ok(())
    }
    pub fn add_roadmap_item(&self, item: &crate::types::RoadmapItem) -> Result<(), StorageError> {
        let goal_exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM goals WHERE id=?1)",
            params![item.goal_id],
            |r| r.get(0),
        )?;
        if !goal_exists {
            return Err(StorageError::InvalidRoadmap(format!(
                "goal '{}' does not exist",
                item.goal_id
            )));
        }
        if item.depends_on.iter().any(|id| id == &item.id) {
            return Err(StorageError::InvalidRoadmap(
                "an item cannot depend on itself".into(),
            ));
        }
        for dep in &item.depends_on {
            let valid: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM roadmap_items WHERE id=?1 AND goal_id=?2)",
                params![dep, item.goal_id],
                |r| r.get(0),
            )?;
            if !valid {
                return Err(StorageError::InvalidRoadmap(format!(
                    "dependency '{}' is missing or belongs to another goal",
                    dep
                )));
            }
        }
        let position = if item.position == 0 {
            self.conn.query_row(
                "SELECT COALESCE(MAX(position)+1, 0) FROM roadmap_items WHERE goal_id=?1",
                params![item.goal_id],
                |r| r.get::<_, i64>(0),
            )?
        } else {
            item.position
        };
        self.conn.execute(
            "INSERT INTO roadmap_items VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                item.id,
                item.goal_id,
                item.title,
                item.description,
                item.status,
                position,
                serde_json::to_string(&item.depends_on).unwrap(),
                item.created_at.to_rfc3339(),
                item.updated_at.to_rfc3339()
            ],
        )?;
        self.record_roadmap_event(
            &item.goal_id,
            Some(&item.id),
            "item_created",
            None,
            Some(&item.status),
            item.created_at,
        )?;
        self.conn.execute(
            "UPDATE goals SET updated_at=?1 WHERE id=?2",
            params![item.updated_at.to_rfc3339(), item.goal_id],
        )?;
        Ok(())
    }
    pub fn roadmap(
        &self,
        goal_id: &str,
    ) -> Result<(crate::types::Goal, Vec<crate::types::RoadmapItem>), StorageError> {
        let g = self
            .conn
            .query_row(
                "SELECT id,title,description,status,created_at,updated_at FROM goals WHERE id=?1",
                params![goal_id],
                |r| {
                    Ok(crate::types::Goal {
                        id: r.get(0)?,
                        title: r.get(1)?,
                        description: r.get(2)?,
                        status: r.get(3)?,
                        created_at: DateTime::parse_from_rfc3339(&r.get::<_, String>(4)?)
                            .unwrap()
                            .with_timezone(&Utc),
                        updated_at: DateTime::parse_from_rfc3339(&r.get::<_, String>(5)?)
                            .unwrap()
                            .with_timezone(&Utc),
                    })
                },
            )
            .map_err(StorageError::Sqlite)?;
        let mut s=self.conn.prepare("SELECT id,goal_id,title,description,status,position,depends_on,created_at,updated_at FROM roadmap_items WHERE goal_id=?1 ORDER BY position,id")?;
        let rows = s
            .query_map(params![goal_id], |r| {
                let deps: String = r.get(6)?;
                Ok(crate::types::RoadmapItem {
                    id: r.get(0)?,
                    goal_id: r.get(1)?,
                    title: r.get(2)?,
                    description: r.get(3)?,
                    status: r.get(4)?,
                    position: r.get(5)?,
                    depends_on: serde_json::from_str(&deps).unwrap_or_default(),
                    created_at: DateTime::parse_from_rfc3339(&r.get::<_, String>(7)?)
                        .unwrap()
                        .with_timezone(&Utc),
                    updated_at: DateTime::parse_from_rfc3339(&r.get::<_, String>(8)?)
                        .unwrap()
                        .with_timezone(&Utc),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok((g, rows))
    }
    pub fn update_roadmap_item(
        &self,
        id: &str,
        status: &str,
        now: DateTime<Utc>,
    ) -> Result<(), StorageError> {
        let (goal_id, deps, old_status): (String, String, String) = self
            .conn
            .query_row(
                "SELECT goal_id, depends_on, status FROM roadmap_items WHERE id=?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map_err(|e| {
                if matches!(e, rusqlite::Error::QueryReturnedNoRows) {
                    StorageError::InvalidRoadmap(format!("item '{}' does not exist", id))
                } else {
                    StorageError::Sqlite(e)
                }
            })?;
        if status == "in_progress" || status == "done" {
            let dependencies: Vec<String> = serde_json::from_str(&deps).unwrap_or_default();
            for dep in dependencies {
                let dep_status: String = self
                    .conn
                    .query_row(
                        "SELECT status FROM roadmap_items WHERE id=?1 AND goal_id=?2",
                        params![dep, goal_id],
                        |r| r.get(0),
                    )
                    .map_err(|_| {
                        StorageError::InvalidRoadmap(
                            "dependency is missing or belongs to another goal".to_string(),
                        )
                    })?;
                if dep_status != "done" {
                    return Err(StorageError::InvalidRoadmap(format!(
                        "dependency is not done (status: {})",
                        dep_status
                    )));
                }
            }
        }
        let changed = self.conn.execute(
            "UPDATE roadmap_items SET status=?1,updated_at=?2 WHERE id=?3",
            params![status, now.to_rfc3339(), id],
        )?;
        if changed == 0 {
            return Err(StorageError::InvalidRoadmap(format!(
                "item '{}' does not exist",
                id
            )));
        }
        self.record_roadmap_event(
            &goal_id,
            Some(id),
            "status_changed",
            Some(&old_status),
            Some(status),
            now,
        )?;
        self.conn.execute("UPDATE goals SET status=CASE WHEN (SELECT COUNT(*) FROM roadmap_items WHERE goal_id=?1)=0 THEN 'active' WHEN (SELECT COUNT(*) FROM roadmap_items WHERE goal_id=?1 AND status='done')=(SELECT COUNT(*) FROM roadmap_items WHERE goal_id=?1) THEN 'completed' ELSE 'active' END, updated_at=?2 WHERE id=?1", params![goal_id, now.to_rfc3339()])?;
        Ok(())
    }
    pub fn edit_roadmap_item(
        &self,
        id: &str,
        title: Option<&str>,
        description: Option<&str>,
        position: Option<i64>,
        now: DateTime<Utc>,
    ) -> Result<(), StorageError> {
        let goal_id: String = self
            .conn
            .query_row(
                "SELECT goal_id FROM roadmap_items WHERE id=?1",
                params![id],
                |r| r.get(0),
            )
            .map_err(|e| {
                if matches!(e, rusqlite::Error::QueryReturnedNoRows) {
                    StorageError::InvalidRoadmap(format!("item '{}' does not exist", id))
                } else {
                    StorageError::Sqlite(e)
                }
            })?;
        self.conn.execute("UPDATE roadmap_items SET title=COALESCE(?1,title), description=COALESCE(?2,description), position=COALESCE(?3,position), updated_at=?4 WHERE id=?5", params![title, description, position, now.to_rfc3339(), id])?;
        self.record_roadmap_event(&goal_id, Some(id), "item_edited", None, None, now)?;
        self.conn.execute(
            "UPDATE goals SET updated_at=?1 WHERE id=?2",
            params![now.to_rfc3339(), goal_id],
        )?;
        Ok(())
    }
    pub fn delete_roadmap_item(&self, id: &str, now: DateTime<Utc>) -> Result<(), StorageError> {
        let goal_id: String = self
            .conn
            .query_row(
                "SELECT goal_id FROM roadmap_items WHERE id=?1",
                params![id],
                |r| r.get(0),
            )
            .map_err(|e| {
                if matches!(e, rusqlite::Error::QueryReturnedNoRows) {
                    StorageError::InvalidRoadmap(format!("item '{}' does not exist", id))
                } else {
                    StorageError::Sqlite(e)
                }
            })?;
        let used: bool = self.conn.query_row("SELECT EXISTS(SELECT 1 FROM roadmap_items WHERE goal_id=?1 AND instr(depends_on, ?2) > 0)", params![goal_id, format!("\"{}\"", id)], |r| r.get(0))?;
        if used {
            return Err(StorageError::InvalidRoadmap(
                "cannot delete an item that another item depends on".into(),
            ));
        }
        self.conn
            .execute("DELETE FROM roadmap_items WHERE id=?1", params![id])?;
        self.record_roadmap_event(&goal_id, Some(id), "item_deleted", None, None, now)?;
        self.conn.execute(
            "UPDATE goals SET status='active',updated_at=?1 WHERE id=?2",
            params![now.to_rfc3339(), goal_id],
        )?;
        Ok(())
    }

    // ── Repo metadata ───────────────────────────────────────────────────────

    /// Upsert repository metadata.
    pub fn upsert_repo(&self, meta: &RepoMeta) -> Result<(), StorageError> {
        self.conn.execute(
            "INSERT INTO repos (repo_id, url, alias, path, indexed_at, last_accessed_at, symbol_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(repo_id) DO UPDATE SET
                url          = excluded.url,
                alias        = excluded.alias,
                path         = excluded.path,
                indexed_at   = excluded.indexed_at,
                last_accessed_at = excluded.last_accessed_at,
                symbol_count = excluded.symbol_count",
            params![
                meta.repo_id,
                meta.url,
                meta.alias,
                meta.path.to_string_lossy(),
                meta.indexed_at.map(|t| t.to_rfc3339()),
                meta.last_accessed_at.map(|t| t.to_rfc3339()),
                meta.symbol_count as i64,
            ],
        )?;
        Ok(())
    }

    /// Fetch a single repo by id.
    pub fn get_repo(&self, repo_id: &str) -> Result<RepoMeta, StorageError> {
        let meta = self
            .conn
            .query_row(
                "SELECT repo_id, url, alias, path, indexed_at, last_accessed_at, symbol_count FROM repos WHERE repo_id = ?1",
                params![repo_id],
                |row| {
                    let path: String = row.get(3)?;
                    let indexed_at_str: Option<String> = row.get(4)?;
                    let last_accessed_str: Option<String> = row.get(5)?;
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        PathBuf::from(path),
                        indexed_at_str,
                        last_accessed_str,
                        row.get::<_, i64>(6)?,
                    ))
                },
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    StorageError::RepoNotFound(repo_id.to_string())
                }
                other => StorageError::Sqlite(other),
            })?;

        let indexed_at = meta
            .4
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|dt| dt.with_timezone(&Utc));

        let last_accessed_at = meta
            .5
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|dt| dt.with_timezone(&Utc));

        Ok(RepoMeta {
            repo_id: meta.0,
            url: meta.1,
            alias: meta.2,
            path: meta.3,
            indexed_at,
            last_accessed_at,
            symbol_count: meta.6 as u64,
        })
    }

    /// Mark a repository as accessed for LRU tracking.
    pub fn mark_accessed(&self, repo_id: &str) -> Result<(), StorageError> {
        self.conn.execute(
            "UPDATE repos SET last_accessed_at = ?1 WHERE repo_id = ?2",
            params![chrono::Utc::now().to_rfc3339(), repo_id],
        )?;
        Ok(())
    }

    /// Delete all DB records for a given repository.
    pub fn evict_repo(&self, repo_id: &str) -> Result<(), StorageError> {
        let tx = self.conn.unchecked_transaction()?;
        {
            // First delete vector embeddings associated with symbols from this repo
            tx.execute(
                "DELETE FROM vec_symbols WHERE symbol_id IN (SELECT id FROM symbols WHERE repo_id = ?1)",
                params![repo_id],
            )?;

            // Delete call edges
            tx.execute(
                "DELETE FROM call_edges WHERE repo_id = ?1",
                params![repo_id],
            )?;

            // Delete symbols
            tx.execute("DELETE FROM symbols WHERE repo_id = ?1", params![repo_id])?;

            // Delete repo metadata
            tx.execute("DELETE FROM repos WHERE repo_id = ?1", params![repo_id])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Return all known repositories.
    pub fn list_repos(&self) -> Result<Vec<RepoMeta>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT repo_id, url, alias, path, indexed_at, last_accessed_at, symbol_count FROM repos",
        )?;
        let rows = stmt.query_map([], |row| {
            let path: String = row.get(3)?;
            let indexed_at_str: Option<String> = row.get(4)?;
            let last_accessed_str: Option<String> = row.get(5)?;
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                PathBuf::from(path),
                indexed_at_str,
                last_accessed_str,
                row.get::<_, i64>(6)?,
            ))
        })?;

        let mut result = Vec::new();
        for row in rows {
            let r = row?;
            let indexed_at =
                r.4.and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                    .map(|dt| dt.with_timezone(&Utc));
            let last_accessed_at =
                r.5.and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                    .map(|dt| dt.with_timezone(&Utc));
            result.push(RepoMeta {
                repo_id: r.0,
                url: r.1,
                alias: r.2,
                path: r.3,
                indexed_at,
                last_accessed_at,
                symbol_count: r.6 as u64,
            });
        }
        Ok(result)
    }

    // ── Symbols ─────────────────────────────────────────────────────────────

    /// Remove all symbols previously stored for a repository (used before re-indexing).
    pub fn clear_symbols(&self, repo_id: &str) -> Result<(), StorageError> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM vec_symbols WHERE symbol_id IN (SELECT id FROM symbols WHERE repo_id = ?1)", params![repo_id])?;
        tx.execute(
            "DELETE FROM call_edges WHERE repo_id = ?1",
            params![repo_id],
        )?;
        tx.execute("DELETE FROM symbols WHERE repo_id = ?1", params![repo_id])?;
        tx.commit()?;
        Ok(())
    }

    /// Remove symbols, call_edges, and vec_symbols for a specific file in a repository.
    pub fn remove_file_symbols(&self, repo_id: &str, file_path: &str) -> Result<u64, StorageError> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM vec_symbols WHERE symbol_id IN (SELECT id FROM symbols WHERE repo_id = ?1 AND file_path = ?2)",
            params![repo_id, file_path],
        )?;
        tx.execute(
            "DELETE FROM call_edges WHERE repo_id = ?1 AND file_path = ?2",
            params![repo_id, file_path],
        )?;
        let count = tx.execute(
            "DELETE FROM symbols WHERE repo_id = ?1 AND file_path = ?2",
            params![repo_id, file_path],
        )?;
        tx.commit()?;
        Ok(count as u64)
    }

    /// Atomically update symbols and call edges for a single file in a repository.
    /// Returns the inserted symbols (with their new database IDs) for vector embedding generation.
    pub fn update_file_symbols(
        &self,
        repo_id: &str,
        file_path: &str,
        symbols: &[Symbol],
        edges: &[CallEdge],
    ) -> Result<Vec<Symbol>, StorageError> {
        let tx = self.conn.unchecked_transaction()?;

        tx.execute(
            "DELETE FROM vec_symbols WHERE symbol_id IN (SELECT id FROM symbols WHERE repo_id = ?1 AND file_path = ?2)",
            params![repo_id, file_path],
        )?;
        tx.execute(
            "DELETE FROM call_edges WHERE repo_id = ?1 AND file_path = ?2",
            params![repo_id, file_path],
        )?;
        tx.execute(
            "DELETE FROM symbols WHERE repo_id = ?1 AND file_path = ?2",
            params![repo_id, file_path],
        )?;

        let mut inserted_symbols = Vec::with_capacity(symbols.len());
        {
            let mut stmt = tx.prepare(
                "INSERT INTO symbols (repo_id, name, kind, file_path, line_start, line_end, signature, language, source, package_name, package_version)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )?;
            for sym in symbols {
                stmt.execute(params![
                    sym.repo_id,
                    sym.name,
                    sym.kind.to_string(),
                    sym.file_path,
                    sym.line_start,
                    sym.line_end,
                    sym.signature,
                    sym.language,
                    sym.source.to_string(),
                    sym.package_name,
                    sym.package_version,
                ])?;
                let last_id = tx.last_insert_rowid();
                let mut sym_with_id = sym.clone();
                sym_with_id.id = Some(last_id);
                inserted_symbols.push(sym_with_id);
            }
        }

        {
            let mut stmt = tx.prepare(
                "INSERT INTO call_edges (repo_id, caller, callee, file_path, line)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for edge in edges {
                stmt.execute(params![
                    edge.repo_id,
                    edge.caller,
                    edge.callee,
                    edge.file_path,
                    edge.line,
                ])?;
            }
        }

        tx.commit()?;
        Ok(inserted_symbols)
    }

    /// Bulk-insert symbols using a transaction.
    pub fn insert_symbols(&self, symbols: &[Symbol]) -> Result<(), StorageError> {
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO symbols (repo_id, name, kind, file_path, line_start, line_end, signature, language, source, package_name, package_version)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )?;
            for sym in symbols {
                stmt.execute(params![
                    sym.repo_id,
                    sym.name,
                    sym.kind.to_string(),
                    sym.file_path,
                    sym.line_start,
                    sym.line_end,
                    sym.signature,
                    sym.language,
                    sym.source.to_string(),
                    sym.package_name,
                    sym.package_version,
                ])?;
            }
        }
        tx.commit()?;
        debug!("Inserted {} symbols", symbols.len());
        Ok(())
    }

    /// Load all symbols for a repository (or all repos if repo_id is None).
    pub fn load_symbols(&self, repo_id: Option<&str>) -> Result<Vec<Symbol>, StorageError> {
        let mut stmt;
        if let Some(r_id) = repo_id {
            stmt = self.conn.prepare(
                "SELECT id, repo_id, name, kind, file_path, line_start, line_end, signature, language, source, package_name, package_version FROM symbols WHERE repo_id = ?1",
            )?;
            let rows = stmt.query_map(params![r_id], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, u32>(5)?,
                    row.get::<_, u32>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, Option<String>>(10)?,
                    row.get::<_, Option<String>>(11)?,
                ))
            })?;

            let mut result = Vec::new();
            for row in rows {
                let r = row?;
                let kind = SymbolKind::from_str(&r.3).unwrap_or(SymbolKind::Function);
                let source = r.9.parse::<SymbolSource>().unwrap_or_default();
                result.push(Symbol {
                    id: Some(r.0),
                    repo_id: r.1,
                    name: r.2,
                    kind,
                    file_path: r.4,
                    line_start: r.5,
                    line_end: r.6,
                    signature: r.7,
                    language: r.8,
                    source,
                    package_name: r.10,
                    package_version: r.11,
                });
            }
            Ok(result)
        } else {
            stmt = self.conn.prepare(
                "SELECT id, repo_id, name, kind, file_path, line_start, line_end, signature, language, source, package_name, package_version FROM symbols",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, u32>(5)?,
                    row.get::<_, u32>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, Option<String>>(10)?,
                    row.get::<_, Option<String>>(11)?,
                ))
            })?;

            let mut result = Vec::new();
            for row in rows {
                let r = row?;
                let kind = SymbolKind::from_str(&r.3).unwrap_or(SymbolKind::Function);
                let source = r.9.parse::<SymbolSource>().unwrap_or_default();
                result.push(Symbol {
                    id: Some(r.0),
                    repo_id: r.1,
                    name: r.2,
                    kind,
                    file_path: r.4,
                    line_start: r.5,
                    line_end: r.6,
                    signature: r.7,
                    language: r.8,
                    source,
                    package_name: r.10,
                    package_version: r.11,
                });
            }
            Ok(result)
        }
    }

    // ── indexed_packages (dependency cache) ────────────────────────────────

    /// Check if a package is already indexed. Returns Some(symbol_count) if yes.
    pub fn is_package_indexed(
        &self,
        name: &str,
        version: &str,
        language: &str,
    ) -> Result<Option<i64>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT symbol_count FROM indexed_packages WHERE name = ?1 AND version = ?2 AND language = ?3",
        )?;
        let result = stmt
            .query_row(params![name, version, language], |row| row.get::<_, i64>(0))
            .optional();
        match result {
            Ok(opt) => Ok(opt),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Sqlite(e)),
        }
    }

    /// Mark a package as indexed (upsert).
    pub fn mark_package_indexed(
        &self,
        name: &str,
        version: &str,
        language: &str,
        source_path: &str,
        symbol_count: i64,
    ) -> Result<(), StorageError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        self.conn.execute(
            "INSERT INTO indexed_packages (name, version, language, source_path, symbol_count, indexed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(name, version, language) DO UPDATE SET
                source_path  = excluded.source_path,
                symbol_count = excluded.symbol_count,
                indexed_at   = excluded.indexed_at",
            params![name, version, language, source_path, symbol_count, now],
        )?;
        Ok(())
    }

    /// List all indexed packages.
    pub fn list_indexed_packages(
        &self,
        language: Option<&str>,
    ) -> Result<Vec<serde_json::Value>, StorageError> {
        let mut stmt;
        let rows: Vec<_>;
        if let Some(lang) = language {
            stmt = self.conn.prepare(
                "SELECT name, version, language, source_path, symbol_count, indexed_at FROM indexed_packages WHERE language = ?1 ORDER BY name",
            )?;
            rows = stmt
                .query_map(params![lang], |row| {
                    Ok(serde_json::json!({
                        "name":         row.get::<_, String>(0)?,
                        "version":      row.get::<_, String>(1)?,
                        "language":     row.get::<_, String>(2)?,
                        "source_path":  row.get::<_, String>(3)?,
                        "symbol_count": row.get::<_, i64>(4)?,
                        "indexed_at":   row.get::<_, i64>(5)?,
                    }))
                })?
                .filter_map(|r| r.ok())
                .collect();
        } else {
            stmt = self.conn.prepare(
                "SELECT name, version, language, source_path, symbol_count, indexed_at FROM indexed_packages ORDER BY language, name",
            )?;
            rows = stmt
                .query_map([], |row| {
                    Ok(serde_json::json!({
                        "name":         row.get::<_, String>(0)?,
                        "version":      row.get::<_, String>(1)?,
                        "language":     row.get::<_, String>(2)?,
                        "source_path":  row.get::<_, String>(3)?,
                        "symbol_count": row.get::<_, i64>(4)?,
                        "indexed_at":   row.get::<_, i64>(5)?,
                    }))
                })?
                .filter_map(|r| r.ok())
                .collect();
        }
        Ok(rows)
    }

    /// Bulk-insert call edges using a transaction.
    pub fn insert_call_edges(&self, edges: &[CallEdge]) -> Result<(), StorageError> {
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO call_edges (repo_id, caller, callee, file_path, line) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for edge in edges {
                stmt.execute(params![
                    edge.repo_id,
                    edge.caller,
                    edge.callee,
                    edge.file_path,
                    edge.line
                ])?;
            }
        }
        tx.commit()?;
        debug!("Inserted {} call edges", edges.len());
        Ok(())
    }

    /// Return all call edges where `caller` matches the given symbol name.
    pub fn calls_made_by(
        &self,
        caller: &str,
        repo_id: Option<&str>,
    ) -> Result<Vec<CallEdge>, StorageError> {
        let mut stmt;
        if let Some(r_id) = repo_id {
            stmt = self.conn.prepare("SELECT repo_id, caller, callee, file_path, line FROM call_edges WHERE caller = ?1 AND repo_id = ?2")?;
            let rows = stmt.query_map(params![caller, r_id], |row| {
                Ok(CallEdge {
                    repo_id: row.get(0)?,
                    caller: row.get(1)?,
                    callee: row.get(2)?,
                    file_path: row.get(3)?,
                    line: row.get(4)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(StorageError::Sqlite)
        } else {
            stmt = self.conn.prepare(
                "SELECT repo_id, caller, callee, file_path, line FROM call_edges WHERE caller = ?1",
            )?;
            let rows = stmt.query_map(params![caller], |row| {
                Ok(CallEdge {
                    repo_id: row.get(0)?,
                    caller: row.get(1)?,
                    callee: row.get(2)?,
                    file_path: row.get(3)?,
                    line: row.get(4)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(StorageError::Sqlite)
        }
    }

    /// Return all call edges where `callee` matches the given symbol name.
    pub fn callers_of(
        &self,
        callee: &str,
        repo_id: Option<&str>,
    ) -> Result<Vec<CallEdge>, StorageError> {
        let mut stmt;
        if let Some(r_id) = repo_id {
            stmt = self.conn.prepare("SELECT repo_id, caller, callee, file_path, line FROM call_edges WHERE callee = ?1 AND repo_id = ?2")?;
            let rows = stmt.query_map(params![callee, r_id], |row| {
                Ok(CallEdge {
                    repo_id: row.get(0)?,
                    caller: row.get(1)?,
                    callee: row.get(2)?,
                    file_path: row.get(3)?,
                    line: row.get(4)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(StorageError::Sqlite)
        } else {
            stmt = self.conn.prepare(
                "SELECT repo_id, caller, callee, file_path, line FROM call_edges WHERE callee = ?1",
            )?;
            let rows = stmt.query_map(params![callee], |row| {
                Ok(CallEdge {
                    repo_id: row.get(0)?,
                    caller: row.get(1)?,
                    callee: row.get(2)?,
                    file_path: row.get(3)?,
                    line: row.get(4)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(StorageError::Sqlite)
        }
    }

    /// Update symbol_count in the repos table.
    pub fn update_symbol_count(
        &self,
        repo_id: &str,
        count: u64,
        indexed_at: DateTime<Utc>,
    ) -> Result<(), StorageError> {
        self.conn.execute(
            "UPDATE repos SET symbol_count = ?1, indexed_at = ?2 WHERE repo_id = ?3",
            params![count as i64, indexed_at.to_rfc3339(), repo_id],
        )?;
        Ok(())
    }

    /// Insert vector embeddings for semantic search.
    /// Returns the number of inserted vectors.
    pub fn insert_embeddings(&self, embeddings: &[(i64, Vec<f32>)]) -> Result<usize, StorageError> {
        let tx = self.conn.unchecked_transaction()?;
        let mut count = 0;
        {
            let mut stmt =
                tx.prepare("INSERT INTO vec_symbols (symbol_id, embedding) VALUES (?1, ?2)")?;
            for (id, vec) in embeddings {
                let bytes: &[u8] = bytemuck::cast_slice(vec.as_slice());
                stmt.execute(params![id, bytes])?;
                count += 1;
            }
        }
        tx.commit()?;
        debug!("Inserted {} vector embeddings", count);
        Ok(count)
    }

    /// Perform a vector search using KNN (K-Nearest Neighbors).
    /// Returns a list of symbol_ids and their distance (smaller is closer).
    pub fn search_embeddings(
        &self,
        query_embedding: &[f32],
        limit: usize,
        repo_id: Option<&str>,
    ) -> Result<Vec<(i64, f64)>, StorageError> {
        let bytes: &[u8] = bytemuck::cast_slice(query_embedding);

        let mut stmt;
        if let Some(r_id) = repo_id {
            tracing::debug!("Searching embeddings with repo_id={}", r_id);
            stmt = self.conn.prepare(
                "
                SELECT symbol_id, distance
                FROM vec_symbols
                WHERE embedding MATCH ?1
                  AND k = ?3
                  AND symbol_id IN (SELECT id FROM symbols WHERE repo_id = ?2)
                ",
            )?;
            let rows = stmt.query_map(params![bytes, r_id, limit as i64], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?))
            })?;
            let mut results = Vec::new();
            for row in rows {
                results.push(row?);
            }
            tracing::debug!(
                "search_embeddings with repo_id found {} matches",
                results.len()
            );
            Ok(results)
        } else {
            tracing::debug!("Searching embeddings WITHOUT repo_id");
            stmt = self.conn.prepare(
                "
                SELECT symbol_id, distance
                FROM vec_symbols
                WHERE embedding MATCH ?1
                  AND k = ?2
                ",
            )?;
            let rows = stmt.query_map(params![bytes, limit as i64], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?))
            })?;
            let mut results = Vec::new();
            for row in rows {
                results.push(row?);
            }
            Ok(results)
        }
    }
}

/// Canonical path for the global fossil-mcp cache directory.
pub fn cache_root() -> PathBuf {
    if let Some(path) = std::env::var_os(CACHE_DIR_ENV)
        && !path.is_empty()
    {
        return PathBuf::from(path);
    }

    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".fossil-mcp")
        .join("cache")
}

/// Path to the global SQLite index file.
pub fn global_db_path() -> PathBuf {
    cache_root().join("global.db")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn get_test_store() -> GlobalStore {
        GlobalStore::open(std::path::Path::new(":memory:")).unwrap()
    }

    #[test]
    fn open_creates_parent_directory_for_file_db() {
        let unique = format!(
            "fossil-core-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let dir = std::env::temp_dir().join(unique);
        let db_path = dir.join("nested").join("global.db");

        assert!(!db_path.parent().unwrap().exists());
        let store = GlobalStore::open(&db_path).unwrap();
        drop(store);

        assert!(db_path.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn test_mark_accessed_and_evict() {
        let store = get_test_store();

        let meta1 = crate::types::RepoMeta {
            repo_id: "r1".to_string(),
            url: "http://url1".to_string(),
            alias: None,
            path: PathBuf::from("/p1"),
            indexed_at: None,
            last_accessed_at: None,
            symbol_count: 0,
        };
        store.upsert_repo(&meta1).unwrap();

        store.mark_accessed("r1").unwrap();
        let r1 = store.get_repo("r1").unwrap();
        assert!(r1.last_accessed_at.is_some());

        store.evict_repo("r1").unwrap();
        assert!(store.get_repo("r1").is_err());
    }

    #[test]
    fn roadmap_validates_dependencies_and_advances_position() {
        let store = get_test_store();
        let now = Utc::now();
        store
            .create_goal(&crate::types::Goal {
                id: "g".into(),
                title: "Goal".into(),
                description: None,
                status: "active".into(),
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        let first = crate::types::RoadmapItem {
            id: "a".into(),
            goal_id: "g".into(),
            title: "First".into(),
            description: None,
            status: "planned".into(),
            position: 0,
            depends_on: vec![],
            created_at: now,
            updated_at: now,
        };
        store.add_roadmap_item(&first).unwrap();
        let second = crate::types::RoadmapItem {
            id: "b".into(),
            goal_id: "g".into(),
            title: "Second".into(),
            description: None,
            status: "planned".into(),
            position: 0,
            depends_on: vec!["a".into()],
            created_at: now,
            updated_at: now,
        };
        store.add_roadmap_item(&second).unwrap();
        assert_eq!(store.roadmap("g").unwrap().1[1].position, 1);
        assert!(store.update_roadmap_item("b", "done", now).is_err());
        store.update_roadmap_item("a", "done", now).unwrap();
        store.update_roadmap_item("b", "done", now).unwrap();
        assert_eq!(store.roadmap("g").unwrap().0.status, "completed");
        let missing = crate::types::RoadmapItem {
            id: "c".into(),
            goal_id: "g".into(),
            title: "Bad".into(),
            description: None,
            status: "planned".into(),
            position: 0,
            depends_on: vec!["missing".into()],
            created_at: now,
            updated_at: now,
        };
        assert!(store.add_roadmap_item(&missing).is_err());
    }

    #[test]
    fn roadmap_rejects_manual_completion_with_open_items() {
        let store = get_test_store();
        let now = Utc::now();
        store
            .create_goal(&crate::types::Goal {
                id: "g2".into(),
                title: "Goal".into(),
                description: None,
                status: "active".into(),
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        store
            .add_roadmap_item(&crate::types::RoadmapItem {
                id: "open".into(),
                goal_id: "g2".into(),
                title: "Open".into(),
                description: None,
                status: "planned".into(),
                position: 0,
                depends_on: vec![],
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        assert!(
            store
                .update_goal("g2", None, None, Some("completed"), now)
                .is_err()
        );
    }

    #[test]
    fn test_update_and_remove_file_symbols() {
        let store = get_test_store();
        let sym1 = crate::types::Symbol {
            id: None,
            repo_id: "repo1".to_string(),
            name: "fn1".to_string(),
            kind: crate::types::SymbolKind::Function,
            file_path: "src/main.rs".to_string(),
            line_start: 1,
            line_end: 5,
            signature: "fn fn1()".to_string(),
            language: "rust".to_string(),
            source: crate::types::SymbolSource::UserCode,
            package_name: None,
            package_version: None,
        };
        let edge1 = crate::types::CallEdge {
            repo_id: "repo1".to_string(),
            caller: "fn1".to_string(),
            callee: "fn2".to_string(),
            file_path: "src/main.rs".to_string(),
            line: 3,
        };

        let inserted = store
            .update_file_symbols("repo1", "src/main.rs", &[sym1], &[edge1])
            .unwrap();
        assert_eq!(inserted.len(), 1);
        assert!(inserted[0].id.is_some());

        let loaded = store.load_symbols(Some("repo1")).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "fn1");

        let removed_count = store.remove_file_symbols("repo1", "src/main.rs").unwrap();
        assert_eq!(removed_count, 1);

        let remaining = store.load_symbols(Some("repo1")).unwrap();
        assert_eq!(remaining.len(), 0);
    }
}
