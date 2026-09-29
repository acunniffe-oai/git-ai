//! Durable source batches and immutable OTLP records. Never share delivery state
//! with the notes or metrics queues.
use crate::error::GitAiError;
use rusqlite::{Connection, params};
use std::path::Path;

pub(super) struct Outbox {
    conn: Connection,
}

pub(super) struct Row {
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
        if version > 1 {
            return Err(GitAiError::Generic(format!(
                "Unsupported OTLP outbox schema version {version}"
            )));
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
             PRAGMA user_version=1;",
        )?;
        Ok(Self { conn })
    }

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
