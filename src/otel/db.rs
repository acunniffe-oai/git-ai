//! Durable local session links, source batches, and immutable OTLP records.
use crate::error::GitAiError;
use rusqlite::{Connection, params};
use std::path::Path;

pub(crate) struct Outbox {
    pub(crate) conn: Connection,
}

pub(crate) struct Row {
    pub id: i64,
    pub payload: String,
}

#[derive(Default, Clone, Copy, Debug)]
pub(crate) struct Status {
    pub remaining: usize,
    pub failed: usize,
}

impl Outbox {
    pub fn open() -> Result<Self, GitAiError> {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(path) = std::env::var_os("GIT_AI_TEST_OTEL_DB_PATH") {
            return Self::open_at(Path::new(&path));
        }
        #[cfg(any(test, feature = "test-support"))]
        if let Some(path) = std::env::var_os("GIT_AI_TEST_DB_PATH") {
            return Self::open_at(&Path::new(&path).with_extension("otel-db"));
        }
        let home = dirs::home_dir()
            .ok_or_else(|| GitAiError::Generic("Could not determine home directory".into()))?;
        Self::open_at(&home.join(".git-ai/internal/otel-db"))
    }

    pub fn open_at(path: &Path) -> Result<Self, GitAiError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = crate::sqlite::open_with_memory_limits(path)?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > 3 {
            return Err(GitAiError::Generic(format!(
                "Unsupported OTLP outbox schema version {version}"
            )));
        }
        if version == 3 {
            conn.pragma_update(None, "synchronous", "NORMAL")?;
            return Ok(Self { conn });
        }
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS outbox (
                id INTEGER PRIMARY KEY,
                endpoint TEXT NOT NULL,
                kind TEXT NOT NULL,
                identity TEXT UNIQUE,
                payload TEXT NOT NULL,
                state TEXT NOT NULL DEFAULT 'pending',
                attempts INTEGER NOT NULL DEFAULT 0,
                next_retry_at INTEGER NOT NULL DEFAULT 0,
                lease_at INTEGER,
                credential TEXT,
                last_error TEXT
             );
             CREATE INDEX IF NOT EXISTS outbox_due ON outbox(endpoint, kind, next_retry_at, id)
                WHERE state = 'pending';
             CREATE INDEX IF NOT EXISTS outbox_state ON outbox(endpoint, state);
             CREATE INDEX IF NOT EXISTS outbox_remaining ON outbox(endpoint, state)
                WHERE state != 'delivered';
             CREATE TABLE IF NOT EXISTS link_worktrees (
                family TEXT NOT NULL, worktree TEXT NOT NULL, generation INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY(family, worktree)
             );
             CREATE TABLE IF NOT EXISTS pending_sessions (
                family TEXT NOT NULL, worktree TEXT NOT NULL, generation INTEGER NOT NULL,
                agent TEXT NOT NULL, session_id TEXT NOT NULL,
                PRIMARY KEY(family, worktree, generation, agent, session_id)
             );
             CREATE TABLE IF NOT EXISTS deferred_sessions (
                family TEXT NOT NULL, worktree TEXT NOT NULL, operation_kind TEXT NOT NULL,
                agent TEXT NOT NULL, session_id TEXT NOT NULL,
                PRIMARY KEY(family, worktree, operation_kind, agent, session_id)
             );
             CREATE TABLE IF NOT EXISTS operation_sources (
                family TEXT NOT NULL, worktree TEXT NOT NULL, operation_kind TEXT NOT NULL,
                source_oids TEXT NOT NULL,
                PRIMARY KEY(family, worktree, operation_kind)
             );
             CREATE TABLE IF NOT EXISTS commit_sessions (
                family TEXT NOT NULL, commit_sha TEXT NOT NULL, agent TEXT NOT NULL, session_id TEXT NOT NULL,
                PRIMARY KEY(family, commit_sha, agent, session_id)
             );
             CREATE TABLE IF NOT EXISTS link_operations (
                family TEXT NOT NULL, operation_id TEXT NOT NULL, worktree TEXT NOT NULL,
                PRIMARY KEY(family, operation_id)
             );
             CREATE TABLE IF NOT EXISTS rewrite_links (
                family TEXT NOT NULL, source_sha TEXT NOT NULL, destination_sha TEXT NOT NULL,
                PRIMARY KEY(family, source_sha, destination_sha)
             );
             CREATE TABLE IF NOT EXISTS stash_sessions (
                family TEXT NOT NULL, stash_oid TEXT NOT NULL, agent TEXT NOT NULL, session_id TEXT NOT NULL,
                PRIMARY KEY(family, stash_oid, agent, session_id)
             );
             CREATE TABLE IF NOT EXISTS link_invocations (
                family TEXT NOT NULL, worktree TEXT NOT NULL, invocation_id TEXT NOT NULL,
                agent TEXT NOT NULL, session_id TEXT NOT NULL,
                PRIMARY KEY(family, worktree, invocation_id)
             );
             CREATE TABLE IF NOT EXISTS invocation_commits (
                family TEXT NOT NULL, worktree TEXT NOT NULL, invocation_id TEXT NOT NULL, commit_sha TEXT NOT NULL,
                PRIMARY KEY(family, worktree, invocation_id, commit_sha)
             );
             CREATE TABLE IF NOT EXISTS created_commits (
                family TEXT NOT NULL, worktree TEXT NOT NULL, commit_sha TEXT NOT NULL,
                started_at_ns TEXT NOT NULL, finished_at_ns TEXT NOT NULL,
                PRIMARY KEY(family, worktree, commit_sha)
             );
             PRAGMA user_version=3;",
        )?;
        Ok(Self { conn })
    }

    #[cfg(test)]
    pub fn enqueue_sources(
        &mut self,
        endpoint: &str,
        sources: &[String],
    ) -> Result<(), GitAiError> {
        let tx = self.conn.transaction()?;
        for payload in sources {
            tx.execute(
                "INSERT INTO outbox(endpoint, kind, payload) VALUES (?1, 'source', ?2)",
                params![endpoint, payload],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn recover(&self, endpoint: &str, credential: &str, now: i64) -> Result<(), GitAiError> {
        self.conn.execute(
            "UPDATE outbox SET state='pending', lease_at=NULL WHERE endpoint=?1 AND state='processing' AND lease_at < ?2",
            params![endpoint, now - 300],
        )?;
        self.conn.execute(
            "UPDATE outbox SET state='pending', next_retry_at=0 WHERE endpoint=?1 AND state='unauthorized' AND credential != ?2",
            params![endpoint, credential],
        )?;
        Ok(())
    }

    pub fn claim(
        &mut self,
        endpoint: &str,
        kind: &str,
        limit: usize,
        now: i64,
    ) -> Result<Vec<Row>, GitAiError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let rows = {
            let mut stmt = tx.prepare(
                "SELECT id, payload FROM outbox WHERE endpoint=?1 AND kind=?2 AND state='pending' AND next_retry_at <= ?3 ORDER BY next_retry_at, id LIMIT ?4",
            )?;
            stmt.query_map(params![endpoint, kind, now, limit as i64], |row| {
                Ok(Row {
                    id: row.get(0)?,
                    payload: row.get(1)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        for row in &rows {
            tx.execute(
                "UPDATE outbox SET state='processing', lease_at=?2 WHERE id=?1",
                params![row.id, now],
            )?;
        }
        tx.commit()?;
        Ok(rows)
    }

    /// Preparing records and retiring their source is one transaction. A crash
    /// cannot leave a source completed without its records, or erase a first payload.
    pub fn prepared(
        &mut self,
        source: i64,
        endpoint: &str,
        records: &[(String, String)],
    ) -> Result<(), GitAiError> {
        let tx = self.conn.transaction()?;
        for (identity, payload) in records {
            tx.execute("INSERT OR IGNORE INTO outbox(endpoint, kind, identity, payload) VALUES (?1, 'record', ?2, ?3)", params![endpoint, identity, payload])?;
        }
        tx.execute("DELETE FROM outbox WHERE id=?1 AND kind='source'", [source])?;
        tx.commit()?;
        Ok(())
    }

    pub fn finish(
        &mut self,
        ids: &[i64],
        state: &str,
        error: &str,
        credential: &str,
        now: i64,
    ) -> Result<(), GitAiError> {
        let tx = self.conn.transaction()?;
        for id in ids {
            tx.execute(
                "UPDATE outbox SET state=?2, last_error=?3, credential=?4, lease_at=NULL,
                 next_retry_at=CASE WHEN ?2='pending' THEN ?5 + (1 << MIN(attempts, 8)) * 5 ELSE 0 END,
                 attempts=attempts+1 WHERE id=?1",
                params![id, state, error, credential, now],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn status(&self, endpoint: &str) -> Result<Status, GitAiError> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(state IN ('failed','unauthorized')),0) FROM outbox WHERE endpoint=?1 AND state != 'delivered'",
            [endpoint], |row| Ok(Status { remaining: row.get(0)?, failed: row.get(1)? }),
        )?)
    }

    pub fn has_due(&self, endpoint: &str, now: i64) -> Result<bool, GitAiError> {
        Ok(self.conn.query_row("SELECT EXISTS(SELECT 1 FROM outbox WHERE endpoint=?1 AND state='pending' AND next_retry_at <= ?2)", params![endpoint, now], |row| row.get(0))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v2_schema2_migration_preserves_links_and_adds_ordered_operation_sources() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbox");
        let db = Outbox::open_at(&path).unwrap();
        db.conn.execute("INSERT INTO commit_sessions(family,commit_sha,agent,session_id) VALUES ('family','sha','codex','raw-id')",[]).unwrap();
        db.conn
            .execute_batch("DROP TABLE operation_sources; PRAGMA user_version=2;")
            .unwrap();
        drop(db);
        let db = Outbox::open_at(&path).unwrap();
        let session: String = db
            .conn
            .query_row("SELECT session_id FROM commit_sessions", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(session, "raw-id");
        let source_count: i64 = db
            .conn
            .query_row("SELECT count(*) FROM operation_sources", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(source_count, 0);
        let version: i64 = db
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 3);
    }

    #[test]
    fn v2_schema_migration_preserves_delivered_outbox_and_starts_without_links() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbox");
        let mut db = Outbox::open_at(&path).unwrap();
        db.prepared(-1, "endpoint", &[("old".into(), "old payload".into())])
            .unwrap();
        let row = db.claim("endpoint", "record", 1, 0).unwrap().remove(0);
        db.finish(&[row.id], "delivered", "", "key", 0).unwrap();
        db.conn.pragma_update(None, "user_version", 1).unwrap();
        drop(db);

        let db = Outbox::open_at(&path).unwrap();
        let version: i64 = db
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, 3);
        let row: (String, String) = db
            .conn
            .query_row(
                "SELECT payload, state FROM outbox WHERE identity='old'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(row, ("old payload".into(), "delivered".into()));
        let count: i64 = db
            .conn
            .query_row("SELECT count(*) FROM commit_sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn otel_outbox_survives_restart_and_keeps_first_delivered_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbox");
        let mut db = Outbox::open_at(&path).unwrap();
        db.enqueue_sources("endpoint", &["source".into()]).unwrap();
        let source = db.claim("endpoint", "source", 1, 10).unwrap();
        // Simulate a crash after claiming, then reclaim only after lease expiry.
        drop(db);
        let mut db = Outbox::open_at(&path).unwrap();
        db.recover("endpoint", "key", 100).unwrap();
        assert!(db.claim("endpoint", "source", 1, 100).unwrap().is_empty());
        db.recover("endpoint", "key", 311).unwrap();
        assert_eq!(
            db.claim("endpoint", "source", 1, 311).unwrap()[0].id,
            source[0].id
        );
        db.prepared(
            source[0].id,
            "endpoint",
            &[("same-session-repo-sha".into(), "first payload".into())],
        )
        .unwrap();
        let rows = db.claim("endpoint", "record", 100, 312).unwrap();
        db.finish(&[rows[0].id], "delivered", "", "key", 312)
            .unwrap();
        db.prepared(
            -1,
            "endpoint",
            &[("same-session-repo-sha".into(), "changed payload".into())],
        )
        .unwrap();
        assert!(db.claim("endpoint", "record", 100, 313).unwrap().is_empty());
        let payload: String = db
            .conn
            .query_row("SELECT payload FROM outbox", [], |r| r.get(0))
            .unwrap();
        assert_eq!(payload, "first payload");
        assert_eq!(db.status("endpoint").unwrap().remaining, 0);
    }

    #[test]
    fn otel_outbox_backoff_credentials_and_destinations_are_independent() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Outbox::open_at(&dir.path().join("outbox")).unwrap();
        db.prepared(-1, "first", &[("first".into(), "{}".into())])
            .unwrap();
        assert!(db.claim("second", "record", 1, 10).unwrap().is_empty());
        let id = db.claim("first", "record", 1, 10).unwrap()[0].id;
        db.finish(&[id], "pending", "network", "old", 10).unwrap();
        assert!(!db.has_due("first", 14).unwrap());
        assert_eq!(db.claim("first", "record", 1, 15).unwrap()[0].id, id);
        db.finish(&[id], "unauthorized", "401", "old", 15).unwrap();
        db.recover("first", "old", 500).unwrap();
        assert!(!db.has_due("first", 500).unwrap());
        db.recover("first", "new", 500).unwrap();
        assert_eq!(db.claim("first", "record", 1, 500).unwrap()[0].id, id);
        db.finish(&[id], "failed", "400", "new", 500).unwrap();
        db.recover("first", "third", 900).unwrap();
        assert!(!db.has_due("first", 900).unwrap());
        assert_eq!(db.status("first").unwrap().failed, 1);
        assert_eq!(db.status("first").unwrap().remaining, 1);
    }
}
