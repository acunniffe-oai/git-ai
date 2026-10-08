//! Sequenced, content-free checkpoint effects. Never called by trace ingestion.
use crate::commit_links::checkpoint::{CheckpointSignal, LinkCheckpoint, ShellInvocationSnapshot};
use crate::commit_links::store::{LinkStore, OperationContext, PendingState, SessionIdentity};
use crate::error::GitAiError;
use crate::git::repository::discover_repository_in_path_no_git_exec;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

type InvocationKey = (String, String, String, String);
const SHELL_SNAPSHOT_TTL: Duration = Duration::from_secs(300);
const MAX_SHELL_SNAPSHOTS: usize = 64;
const MAX_SHELL_SNAPSHOT_ENTRIES: usize = 100_000;

struct RetainedSnapshot {
    family: String,
    value: ShellInvocationSnapshot,
    started_at: Instant,
}

impl RetainedSnapshot {
    fn entries(&self) -> usize {
        self.value
            .snapshot
            .as_ref()
            .map_or(0, |snapshot| snapshot.entries.len())
    }
}

fn snapshot_limit() -> usize {
    #[cfg(feature = "test-support")]
    if let Ok(limit) = std::env::var("GIT_AI_TEST_SHELL_SNAPSHOT_LIMIT")
        && let Ok(limit) = limit.parse::<usize>()
    {
        return limit.clamp(1, MAX_SHELL_SNAPSHOTS);
    }
    MAX_SHELL_SNAPSHOTS
}

#[derive(Default)]
pub(super) struct CheckpointRuntime {
    snapshots: Mutex<BTreeMap<InvocationKey, RetainedSnapshot>>,
}

fn scope(worktree: &Path) -> Result<(String, String), GitAiError> {
    let repo = discover_repository_in_path_no_git_exec(worktree)?;
    Ok((
        repo.common_dir()
            .canonicalize()
            .unwrap_or_else(|_| repo.common_dir().to_path_buf())
            .to_string_lossy()
            .into_owned(),
        super::ActorDaemonCoordinator::worktree_state_key(worktree),
    ))
}

fn invocation_key(worktree: &str, session: &SessionIdentity, id: &str) -> InvocationKey {
    (
        worktree.into(),
        session.agent.clone(),
        session.id.clone(),
        id.into(),
    )
}

fn persisted_invocation_id(session: &SessionIdentity, id: &str) -> Result<String, GitAiError> {
    Ok(serde_json::to_string(&(&session.agent, &session.id, id))?)
}

impl CheckpointRuntime {
    pub(super) fn attachment(
        &self,
        worktree: &Path,
        session: &SessionIdentity,
    ) -> Result<PendingState, GitAiError> {
        let (family, worktree) = scope(worktree)?;
        LinkStore::open()?.pending_state(&family, &worktree, session)
    }

    pub(super) fn invocation(
        &self,
        worktree: &Path,
        session: &SessionIdentity,
        id: &str,
    ) -> Result<Option<ShellInvocationSnapshot>, GitAiError> {
        let (_, worktree) = scope(worktree)?;
        let mut snapshots = self
            .snapshots
            .lock()
            .map_err(|_| GitAiError::Generic("checkpoint snapshots lock poisoned".into()))?;
        let key = invocation_key(&worktree, session, id);
        if snapshots
            .get(&key)
            .is_some_and(|snapshot| snapshot.started_at.elapsed() >= SHELL_SNAPSHOT_TTL)
        {
            let expired = snapshots.get(&key).unwrap();
            LinkStore::open()?.abandon_invocation(
                &expired.family,
                &worktree,
                &persisted_invocation_id(session, id)?,
            )?;
            snapshots.remove(&key);
        }
        Ok(snapshots.get(&key).map(|snapshot| snapshot.value.clone()))
    }

    pub(super) fn apply(&self, request: LinkCheckpoint) -> Result<(), GitAiError> {
        let repo = discover_repository_in_path_no_git_exec(&request.repo_working_dir)?;
        if !crate::config::Config::get().is_allowed_repository(&Some(repo.clone())) {
            return Ok(());
        }
        let worktree = super::ActorDaemonCoordinator::worktree_state_key(&request.repo_working_dir);
        let remotes = repo.remotes_with_urls()?;
        let repo_url = remotes
            .iter()
            .find(|(name, _)| name == "origin")
            .or(remotes.first())
            .map(|(_, url)| url.clone());
        let context = OperationContext {
            id: format!("checkpoint:{}", request.request_id),
            family: repo
                .common_dir()
                .canonicalize()
                .unwrap_or_else(|_| repo.common_dir().to_path_buf())
                .to_string_lossy()
                .into_owned(),
            worktree: worktree.clone(),
            git_dir: repo.common_dir().to_path_buf(),
            repo_url,
            time_ns: super::now_unix_nanos(),
        };
        let mut store = LinkStore::open()?;
        match request.signal {
            CheckpointSignal::PostWrite => {
                store.apply_operation(&context, |tx| {
                    tx.add_pending(&BTreeSet::from([request.session]))
                })?;
            }
            CheckpointSignal::ShellStart {
                invocation_id,
                snapshot,
            } => {
                let state = store.pending_state(&context.family, &worktree, &request.session)?;
                let mut snapshots = self.snapshots.lock().map_err(|_| {
                    GitAiError::Generic("checkpoint snapshots lock poisoned".into())
                })?;
                let incoming_entries = snapshot
                    .as_ref()
                    .map_or(0, |snapshot| snapshot.entries.len());
                let incoming_key = invocation_key(&worktree, &request.session, &invocation_id);
                loop {
                    let oldest = snapshots
                        .iter()
                        .filter(|(key, _)| *key != &incoming_key)
                        .min_by_key(|(_, snapshot)| snapshot.started_at);
                    let Some((key, value)) = oldest else {
                        break;
                    };
                    let replaced_entries = snapshots
                        .get(&incoming_key)
                        .map_or(0, RetainedSnapshot::entries);
                    let retained_count =
                        snapshots.len() - usize::from(snapshots.contains_key(&incoming_key));
                    if retained_count < snapshot_limit()
                        && snapshots
                            .values()
                            .map(RetainedSnapshot::entries)
                            .sum::<usize>()
                            - replaced_entries
                            + incoming_entries
                            <= MAX_SHELL_SNAPSHOT_ENTRIES
                        && value.started_at.elapsed() < SHELL_SNAPSHOT_TTL
                    {
                        break;
                    }
                    let key = key.clone();
                    store.abandon_invocation(
                        &value.family,
                        &key.0,
                        &serde_json::to_string(&(&key.1, &key.2, &key.3))?,
                    )?;
                    snapshots.remove(&key);
                    tracing::warn!("abandoning expired or over-budget shell write evidence");
                }
                store.begin_invocation(
                    &context.family,
                    &worktree,
                    &persisted_invocation_id(&request.session, &invocation_id)?,
                    &request.session,
                )?;
                snapshots.insert(
                    incoming_key,
                    RetainedSnapshot {
                        family: context.family.clone(),
                        value: ShellInvocationSnapshot {
                            generation: state.generation,
                            attached: state.attached,
                            snapshot,
                        },
                        started_at: Instant::now(),
                    },
                );
            }
            CheckpointSignal::ShellEnd {
                invocation_id,
                wrote,
            } => {
                let key = invocation_key(&worktree, &request.session, &invocation_id);
                let attached = self
                    .snapshots
                    .lock()
                    .map_err(|_| GitAiError::Generic("checkpoint snapshots lock poisoned".into()))?
                    .get(&key)
                    .is_some_and(|snapshot| snapshot.value.attached);
                store.finish_invocation(
                    &context,
                    &persisted_invocation_id(&request.session, &invocation_id)?,
                    wrote || attached,
                )?;
                self.snapshots
                    .lock()
                    .map_err(|_| GitAiError::Generic("checkpoint snapshots lock poisoned".into()))?
                    .remove(&key);
            }
        }
        Ok(())
    }
}
