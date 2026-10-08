//! SQLite is the source of truth for local session links. All calls belong on
//! the sequenced side-effect path, never on trace ingestion.
use crate::config::Config;
use crate::error::GitAiError;
use crate::otel::{Source, db::Outbox};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct SessionIdentity {
    pub agent: String,
    pub id: String,
}

impl SessionIdentity {
    pub fn new(agent: &str, id: &str) -> Option<Self> {
        let agent = match agent {
            "claude" | "claude-code" => "claude-code",
            "cursor-background" => "cursor",
            "human" | "known_human" | "mock_known_human" | "mock_ai" | "ai_tab" | "" => {
                return None;
            }
            // agent-v1 integrations provide their native name in the hook.
            _ => agent,
        };
        if agent.trim().is_empty()
            || id.trim().is_empty()
            || id == "unknown"
            || id.starts_with("DEVIN_SESSION")
        {
            return None;
        }
        Some(Self {
            agent: agent.into(),
            id: id.into(),
        })
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingState {
    pub generation: i64,
    pub attached: bool,
}

/// Immutable operation evidence and repository metadata, prepared before the
/// transaction. No Git calls occur while a write transaction is held.
#[derive(Clone, Debug)]
pub struct OperationContext {
    pub id: String,
    pub family: String,
    pub worktree: String,
    pub git_dir: PathBuf,
    pub repo_url: Option<String>,
    pub time_ns: u128,
}

pub struct LinkStore {
    outbox: Outbox,
    delivery_endpoint: Option<String>,
}

pub struct LinkTransaction<'conn, 'context> {
    tx: Transaction<'conn>,
    context: &'context OperationContext,
    changed: BTreeMap<String, BTreeSet<SessionIdentity>>,
}

fn generation(conn: &Connection, family: &str, worktree: &str) -> Result<i64, GitAiError> {
    Ok(conn
        .query_row(
            "SELECT generation FROM link_worktrees WHERE family=?1 AND worktree=?2",
            params![family, worktree],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0))
}

fn ensure_worktree(conn: &Connection, family: &str, worktree: &str) -> Result<(), GitAiError> {
    conn.execute(
        "INSERT OR IGNORE INTO link_worktrees(family,worktree) VALUES (?1,?2)",
        params![family, worktree],
    )?;
    Ok(())
}

fn read_sessions(
    conn: &Connection,
    sql: &str,
    args: impl rusqlite::Params,
) -> Result<BTreeSet<SessionIdentity>, GitAiError> {
    Ok(conn
        .prepare_cached(sql)?
        .query_map(args, |r| {
            Ok(SessionIdentity {
                agent: r.get(0)?,
                id: r.get(1)?,
            })
        })?
        .collect::<Result<_, _>>()?)
}

fn read_operation_sources(
    conn: &Connection,
    family: &str,
    worktree: &str,
    kind: &str,
) -> Result<Vec<String>, GitAiError> {
    let value: Option<String> = conn.query_row("SELECT source_oids FROM operation_sources WHERE family=?1 AND worktree=?2 AND operation_kind=?3",params![family,worktree,kind],|row|row.get(0)).optional()?;
    value
        .map(|json| serde_json::from_str(&json).map_err(GitAiError::from))
        .unwrap_or_else(|| Ok(Vec::new()))
}

impl LinkStore {
    pub fn open() -> Result<Self, GitAiError> {
        Ok(Self {
            outbox: Outbox::open()?,
            delivery_endpoint: crate::otel::endpoint(&Config::fresh()),
        })
    }

    pub fn open_at(path: &Path) -> Result<Self, GitAiError> {
        Ok(Self {
            outbox: Outbox::open_at(path)?,
            delivery_endpoint: crate::otel::endpoint(&Config::fresh()),
        })
    }

    pub fn pending_state(
        &self,
        family: &str,
        worktree: &str,
        session: &SessionIdentity,
    ) -> Result<PendingState, GitAiError> {
        // One statement keeps the generation and attachment mutually consistent
        // even if another connection closes the window concurrently.
        let (generation, attached) = self.outbox.conn.query_row(
            "SELECT COALESCE((SELECT generation FROM link_worktrees WHERE family=?1 AND worktree=?2),0),
             EXISTS(SELECT 1 FROM pending_sessions WHERE family=?1 AND worktree=?2 AND agent=?3 AND session_id=?4
               AND generation=COALESCE((SELECT generation FROM link_worktrees WHERE family=?1 AND worktree=?2),0))",
            params![family,worktree,session.agent,session.id], |r| Ok((r.get(0)?,r.get(1)?)),
        )?;
        Ok(PendingState {
            generation,
            attached,
        })
    }

    /// False means the caller's generation is stale and it must recheck state.
    pub fn attach_session(
        &mut self,
        family: &str,
        worktree: &str,
        expected_generation: i64,
        session: &SessionIdentity,
    ) -> Result<bool, GitAiError> {
        let tx = self
            .outbox
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_worktree(&tx, family, worktree)?;
        if generation(&tx, family, worktree)? != expected_generation {
            return Ok(false);
        }
        tx.execute("INSERT OR IGNORE INTO pending_sessions(family,worktree,generation,agent,session_id) VALUES (?1,?2,?3,?4,?5)",params![family,worktree,expected_generation,session.agent,session.id])?;
        tx.commit()?;
        Ok(true)
    }

    pub fn sessions_for_commit(
        &self,
        family: &str,
        sha: &str,
    ) -> Result<BTreeSet<SessionIdentity>, GitAiError> {
        read_sessions(
            &self.outbox.conn,
            "SELECT agent,session_id FROM commit_sessions WHERE family=?1 AND commit_sha=?2",
            params![family, sha],
        )
    }

    pub fn operation_sources(
        &self,
        family: &str,
        worktree: &str,
        kind: &str,
    ) -> Result<Vec<String>, GitAiError> {
        read_operation_sources(&self.outbox.conn, family, worktree, kind)
    }

    /// The operation marker, window changes, local links and delivery sources
    /// commit together. A replay cannot consume sessions from a later window.
    pub fn apply_operation(
        &mut self,
        context: &OperationContext,
        apply: impl FnOnce(&mut LinkTransaction<'_, '_>) -> Result<(), GitAiError>,
    ) -> Result<bool, GitAiError> {
        let tx = self
            .outbox
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if tx.execute(
            "INSERT OR IGNORE INTO link_operations(family,operation_id,worktree) VALUES (?1,?2,?3)",
            params![context.family, context.id, context.worktree],
        )? == 0
        {
            return Ok(false);
        }
        ensure_worktree(&tx, &context.family, &context.worktree)?;
        let mut links = LinkTransaction {
            tx,
            context,
            changed: BTreeMap::new(),
        };
        apply(&mut links)?;
        let has_delivery = !links.changed.is_empty() && self.delivery_endpoint.is_some();
        if let Some(endpoint) = self
            .delivery_endpoint
            .as_ref()
            .filter(|_| !links.changed.is_empty())
        {
            let mut commit_times = BTreeMap::new();
            let mut stmt = links.tx.prepare_cached("SELECT finished_at_ns FROM created_commits WHERE family=?1 AND commit_sha=?2 LIMIT 1")?;
            for sha in links.changed.keys() {
                if let Some(time) = stmt
                    .query_row(params![context.family, sha], |row| row.get::<_, String>(0))
                    .optional()?
                {
                    commit_times.insert(sha.clone(), time);
                }
            }
            let source = Source {
                git_dir: context.git_dir.clone(),
                repo_url: context.repo_url.clone(),
                time_unix_nano: context.time_ns.to_string(),
                commit_times,
                commits: links.changed,
            };
            links.tx.execute(
                "INSERT INTO outbox(endpoint,kind,payload) VALUES (?1,'source',?2)",
                params![endpoint, serde_json::to_string(&source)?],
            )?;
        }
        links.tx.commit()?;
        if has_delivery {
            crate::otel::wake();
        }
        Ok(true)
    }

    pub fn begin_invocation(
        &mut self,
        family: &str,
        worktree: &str,
        invocation_id: &str,
        session: &SessionIdentity,
    ) -> Result<(), GitAiError> {
        let tx = self
            .outbox
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Some native hooks reuse IDs such as "bash". A new pre-hook is a new
        // invocation; a missing earlier post-hook cannot donate old commits.
        tx.execute(
            "DELETE FROM invocation_commits WHERE family=?1 AND worktree=?2 AND invocation_id=?3",
            params![family, worktree, invocation_id],
        )?;
        tx.execute("INSERT INTO link_invocations(family,worktree,invocation_id,agent,session_id) VALUES (?1,?2,?3,?4,?5) ON CONFLICT(family,worktree,invocation_id) DO UPDATE SET agent=excluded.agent,session_id=excluded.session_id",params![family,worktree,invocation_id,session.agent,session.id])?;
        tx.commit()?;
        Ok(())
    }

    pub fn abandon_invocation(
        &mut self,
        family: &str,
        worktree: &str,
        invocation_id: &str,
    ) -> Result<(), GitAiError> {
        let tx = self
            .outbox
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM invocation_commits WHERE family=?1 AND worktree=?2 AND invocation_id=?3",
            params![family, worktree, invocation_id],
        )?;
        tx.execute(
            "DELETE FROM link_invocations WHERE family=?1 AND worktree=?2 AND invocation_id=?3",
            params![family, worktree, invocation_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Called once at daemon startup. Metadata snapshots are intentionally
    /// ephemeral, so an interrupted call cannot claim future unrelated commits.
    pub fn abandon_invocations(&mut self) -> Result<(), GitAiError> {
        let tx = self
            .outbox
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM invocation_commits", [])?;
        tx.execute("DELETE FROM link_invocations", [])?;
        tx.commit()?;
        Ok(())
    }

    pub fn finish_invocation(
        &mut self,
        context: &OperationContext,
        invocation_id: &str,
        wrote: bool,
    ) -> Result<bool, GitAiError> {
        self.apply_operation(context, |tx| {
            let session: Option<SessionIdentity> = tx.tx.query_row(
                "SELECT agent,session_id FROM link_invocations WHERE family=?1 AND worktree=?2 AND invocation_id=?3",
                params![context.family,context.worktree,invocation_id], |r| Ok(SessionIdentity {agent:r.get(0)?,id:r.get(1)?}),
            ).optional()?;
            if wrote && let Some(session) = session {
                let commits = tx.tx.prepare("SELECT commit_sha FROM invocation_commits WHERE family=?1 AND worktree=?2 AND invocation_id=?3")?
                    .query_map(params![context.family,context.worktree,invocation_id], |r| r.get::<_,String>(0))?
                    .collect::<Result<Vec<_>,_>>()?;
                let sessions = BTreeSet::from([session]);
                if commits.is_empty() { tx.add_pending(&sessions)?; }
                else { for sha in commits { tx.link_commit(&sha,&sessions)?; } }
            }
            tx.tx.execute("DELETE FROM invocation_commits WHERE family=?1 AND worktree=?2 AND invocation_id=?3",params![context.family,context.worktree,invocation_id])?;
            tx.tx.execute("DELETE FROM link_invocations WHERE family=?1 AND worktree=?2 AND invocation_id=?3",params![context.family,context.worktree,invocation_id])?;
            Ok(())
        })
    }
}

impl LinkTransaction<'_, '_> {
    pub fn pending_sessions(&self) -> Result<BTreeSet<SessionIdentity>, GitAiError> {
        read_sessions(
            &self.tx,
            "SELECT agent,session_id FROM pending_sessions WHERE family=?1 AND worktree=?2
             UNION SELECT agent,session_id FROM deferred_sessions WHERE family=?1 AND worktree=?2",
            params![self.context.family, self.context.worktree],
        )
    }

    pub fn add_pending(&mut self, sessions: &BTreeSet<SessionIdentity>) -> Result<(), GitAiError> {
        let generation = generation(&self.tx, &self.context.family, &self.context.worktree)?;
        let mut stmt = self.tx.prepare_cached("INSERT OR IGNORE INTO pending_sessions(family,worktree,generation,agent,session_id) VALUES (?1,?2,?3,?4,?5)")?;
        for session in sessions {
            stmt.execute(params![
                self.context.family,
                self.context.worktree,
                generation,
                session.agent,
                session.id
            ])?;
        }
        Ok(())
    }

    pub fn clear_pending(&mut self) -> Result<(), GitAiError> {
        self.tx.execute(
            "DELETE FROM pending_sessions WHERE family=?1 AND worktree=?2",
            params![self.context.family, self.context.worktree],
        )?;
        self.tx.execute(
            "DELETE FROM deferred_sessions WHERE family=?1 AND worktree=?2",
            params![self.context.family, self.context.worktree],
        )?;
        self.tx.execute(
            "DELETE FROM operation_sources WHERE family=?1 AND worktree=?2",
            params![self.context.family, self.context.worktree],
        )?;
        self.tx.execute(
            "UPDATE link_worktrees SET generation=generation+1 WHERE family=?1 AND worktree=?2",
            params![self.context.family, self.context.worktree],
        )?;
        Ok(())
    }

    pub fn add_deferred(
        &mut self,
        kind: &str,
        sessions: &BTreeSet<SessionIdentity>,
    ) -> Result<(), GitAiError> {
        let mut stmt = self.tx.prepare_cached("INSERT OR IGNORE INTO deferred_sessions(family,worktree,operation_kind,agent,session_id) VALUES (?1,?2,?3,?4,?5)")?;
        for session in sessions {
            stmt.execute(params![
                self.context.family,
                self.context.worktree,
                kind,
                session.agent,
                session.id
            ])?;
        }
        Ok(())
    }

    pub fn clear_deferred(&mut self, kind: &str) -> Result<(), GitAiError> {
        self.tx.execute(
            "DELETE FROM deferred_sessions WHERE family=?1 AND worktree=?2 AND operation_kind=?3",
            params![self.context.family, self.context.worktree, kind],
        )?;
        self.clear_operation_sources(kind)
    }

    pub fn operation_sources(&self, kind: &str) -> Result<Vec<String>, GitAiError> {
        read_operation_sources(&self.tx, &self.context.family, &self.context.worktree, kind)
    }

    pub fn set_operation_sources(
        &mut self,
        kind: &str,
        sources: &[String],
    ) -> Result<(), GitAiError> {
        if sources.is_empty() {
            return self.clear_operation_sources(kind);
        }
        self.tx.execute("INSERT INTO operation_sources(family,worktree,operation_kind,source_oids) VALUES (?1,?2,?3,?4) ON CONFLICT(family,worktree,operation_kind) DO UPDATE SET source_oids=excluded.source_oids",params![self.context.family,self.context.worktree,kind,serde_json::to_string(sources)?])?;
        Ok(())
    }

    pub fn clear_operation_sources(&mut self, kind: &str) -> Result<(), GitAiError> {
        self.tx.execute(
            "DELETE FROM operation_sources WHERE family=?1 AND worktree=?2 AND operation_kind=?3",
            params![self.context.family, self.context.worktree, kind],
        )?;
        Ok(())
    }

    pub fn sessions_for_commits(
        &self,
        shas: &[String],
    ) -> Result<BTreeSet<SessionIdentity>, GitAiError> {
        let mut sessions = BTreeSet::new();
        for sha in shas {
            sessions.extend(read_sessions(
                &self.tx,
                "SELECT agent,session_id FROM commit_sessions WHERE family=?1 AND commit_sha=?2",
                params![self.context.family, sha],
            )?);
        }
        Ok(sessions)
    }

    /// Register creation even when its session set is empty: a shell post-hook
    /// may establish the write signal after the commit has been processed.
    pub fn record_created_commit(
        &mut self,
        sha: &str,
        started_at_ns: u128,
        finished_at_ns: u128,
    ) -> Result<(), GitAiError> {
        self.tx.execute("INSERT OR IGNORE INTO created_commits(family,worktree,commit_sha,started_at_ns,finished_at_ns) VALUES (?1,?2,?3,?4,?5)", params![self.context.family,self.context.worktree,sha,started_at_ns.to_string(),finished_at_ns.to_string()])?;
        self.tx.execute("INSERT OR IGNORE INTO invocation_commits(family,worktree,invocation_id,commit_sha) SELECT family,worktree,invocation_id,?3 FROM link_invocations WHERE family=?1 AND worktree=?2", params![self.context.family,self.context.worktree,sha])?;
        Ok(())
    }

    pub fn link_commit(
        &mut self,
        sha: &str,
        sessions: &BTreeSet<SessionIdentity>,
    ) -> Result<(), GitAiError> {
        // UNION (rather than UNION ALL) bounds cycles and converging rewrite
        // paths. This uses SQLite only, with no per-object Git work.
        let destinations = self.tx.prepare_cached(
            "WITH RECURSIVE destinations(sha) AS (SELECT ?2 UNION SELECT destination_sha FROM rewrite_links JOIN destinations ON source_sha=sha WHERE family=?1) SELECT sha FROM destinations"
        )?.query_map(params![self.context.family,sha],|r| r.get::<_,String>(0))?.collect::<Result<Vec<_>,_>>()?;
        let mut stmt = self.tx.prepare_cached("INSERT OR IGNORE INTO commit_sessions(family,commit_sha,agent,session_id) VALUES (?1,?2,?3,?4)")?;
        for destination in destinations {
            for session in sessions {
                if stmt.execute(params![
                    self.context.family,
                    destination,
                    session.agent,
                    session.id
                ])? != 0
                {
                    self.changed
                        .entry(destination.clone())
                        .or_default()
                        .insert(session.clone());
                }
            }
        }
        Ok(())
    }

    pub fn record_rewrite(&mut self, source: &str, destination: &str) -> Result<(), GitAiError> {
        self.tx.execute("INSERT OR IGNORE INTO rewrite_links(family,source_sha,destination_sha) VALUES (?1,?2,?3)",params![self.context.family,source,destination])?;
        let sessions = self.sessions_for_commits(&[source.into()])?;
        self.link_commit(destination, &sessions)
    }

    pub fn save_stash(&mut self, oid: &str, retain_pending: bool) -> Result<(), GitAiError> {
        let sessions = self.pending_sessions()?;
        {
            let mut stmt = self.tx.prepare_cached("INSERT OR IGNORE INTO stash_sessions(family,stash_oid,agent,session_id) VALUES (?1,?2,?3,?4)")?;
            for session in sessions {
                stmt.execute(params![self.context.family, oid, session.agent, session.id])?;
            }
        }
        if !retain_pending {
            self.clear_pending()?;
        }
        Ok(())
    }

    pub fn restore_stash(&mut self, oid: &str, remove: bool) -> Result<(), GitAiError> {
        let sessions = read_sessions(
            &self.tx,
            "SELECT agent,session_id FROM stash_sessions WHERE family=?1 AND stash_oid=?2",
            params![self.context.family, oid],
        )?;
        self.add_pending(&sessions)?;
        if remove {
            self.tx.execute(
                "DELETE FROM stash_sessions WHERE family=?1 AND stash_oid=?2",
                params![self.context.family, oid],
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(id: &str) -> OperationContext {
        OperationContext {
            id: id.into(),
            family: "family".into(),
            worktree: "worktree".into(),
            git_dir: PathBuf::from("git-dir"),
            repo_url: None,
            time_ns: 99,
        }
    }

    #[test]
    fn v2_store_delivery_is_atomic_and_uses_commit_time_for_late_links() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = LinkStore::open_at(&dir.path().join("store")).unwrap();
        store.delivery_endpoint = Some("endpoint".into());
        let session = SessionIdentity::new("codex", "session").unwrap();
        store
            .attach_session("family", "worktree", 0, &session)
            .unwrap();
        let result = store.apply_operation(&context("failed"), |tx| {
            tx.link_commit("sha", &tx.pending_sessions()?)?;
            tx.clear_pending()?;
            Err(GitAiError::Generic("rollback".into()))
        });
        assert!(result.is_err());
        let count: i64 = store
            .outbox
            .conn
            .query_row("SELECT COUNT(*) FROM outbox", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
        assert!(
            store
                .pending_state("family", "worktree", &session)
                .unwrap()
                .attached
        );
        store
            .apply_operation(&context("created"), |tx| {
                tx.record_created_commit("sha", 10, 20)
            })
            .unwrap();
        store
            .apply_operation(&context("late"), |tx| {
                tx.link_commit("sha", &tx.pending_sessions()?)?;
                tx.clear_pending()
            })
            .unwrap();
        let payload: String = store
            .outbox
            .conn
            .query_row("SELECT payload FROM outbox", [], |r| r.get(0))
            .unwrap();
        let source: Source = serde_json::from_str(&payload).unwrap();
        assert_eq!(source.commit_times["sha"], "20");
        assert_eq!(source.commits["sha"], BTreeSet::from([session]));
    }

    #[test]
    fn v2_store_enabling_delivery_does_not_backfill_existing_local_links() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = LinkStore::open_at(&dir.path().join("store")).unwrap();
        store.delivery_endpoint = None;
        let sessions = BTreeSet::from([SessionIdentity::new("codex", "session").unwrap()]);
        store
            .apply_operation(&context("offline"), |tx| tx.link_commit("old", &sessions))
            .unwrap();
        store.delivery_endpoint = Some("endpoint".into());
        store
            .apply_operation(&context("replay"), |tx| tx.link_commit("old", &sessions))
            .unwrap();
        store
            .apply_operation(&context("new"), |tx| tx.link_commit("new", &sessions))
            .unwrap();
        let payload: String = store
            .outbox
            .conn
            .query_row("SELECT payload FROM outbox", [], |r| r.get(0))
            .unwrap();
        let source: Source = serde_json::from_str(&payload).unwrap();
        assert_eq!(source.commits.keys().collect::<Vec<_>>(), ["new"]);
        assert_eq!(
            store.sessions_for_commit("family", "old").unwrap(),
            sessions
        );
    }
}
