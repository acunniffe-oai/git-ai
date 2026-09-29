//! Commit/session links for the configured backend's OTLP collector.
//! Note writers only collect successful writes. Git enrichment and HTTP delivery
//! run on a bounded OTLP runtime, independently of the regular telemetry flusher.
mod db;
mod worker;

use crate::authorship::authorship_log_serialization::{AuthorshipLog, GIT_AI_VERSION};
use crate::authorship::working_log::AgentId;
use crate::config::Config;
use crate::error::GitAiError;
use crate::git::repository::Repository;
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) use worker::{flush_and_wait, spawn_worker};

const EVENT: &str = "git_ai.session.linked_commit";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
struct Session {
    agent: String,
    id: String,
}

#[derive(Serialize, Deserialize)]
struct Source {
    git_dir: PathBuf,
    repo_url: Option<String>,
    time_unix_nano: String,
    commits: BTreeMap<String, BTreeSet<Session>>,
}

struct Capture {
    endpoint: String,
    time_unix_nano: String,
    repos: BTreeMap<PathBuf, (Repository, BTreeMap<String, BTreeSet<Session>>)>,
}

tokio::task_local! {
    static CAPTURE: RefCell<Capture>;
}

fn endpoint(config: &Config) -> Option<String> {
    // Test builds must explicitly opt into an isolated outbox; never send links
    // from unrelated fixtures using credentials inherited from the developer.
    #[cfg(any(test, feature = "test-support"))]
    std::env::var_os("GIT_AI_TEST_OTEL_DB_PATH")?;
    config.api_key().filter(|key| !key.is_empty())?;
    let mut url = url::Url::parse(config.api_base_url()).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return None;
    }
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_path("/api/otel/git-ai/v1/logs");
    url.set_query(None);
    url.set_fragment(None);
    Some(url.into())
}

pub(crate) fn enabled() -> bool {
    endpoint(&Config::fresh()).is_some()
}

fn session(agent: &AgentId) -> Option<Session> {
    let tool = match agent.tool.as_str() {
        "claude" | "claude-code" => "claude-code",
        "codex" => "codex",
        "devin" => "devin",
        _ => return None,
    };
    if agent.id.is_empty() || agent.id == "unknown" || agent.id.starts_with("DEVIN_SESSION") {
        return None;
    }
    Some(Session {
        agent: tool.into(),
        id: agent.id.clone(),
    })
}

impl Capture {
    fn new(endpoint: String, time_ns: u128) -> Self {
        Self {
            endpoint,
            time_unix_nano: time_ns.to_string(),
            repos: BTreeMap::new(),
        }
    }

    fn note(&mut self, repo: &Repository, sha: &str, content: &str) {
        if !valid_hex(sha, &[40, 64]) {
            return;
        }
        let Ok(note) = AuthorshipLog::deserialize_from_string(content) else {
            return;
        };
        let sessions = note
            .metadata
            .prompts
            .values()
            .map(|r| &r.agent_id)
            .chain(note.metadata.sessions.values().map(|r| &r.agent_id))
            .filter_map(session)
            .collect();
        self.repos
            .entry(repo.path().to_path_buf())
            .or_insert_with(|| (repo.clone(), BTreeMap::new()))
            .1
            .insert(sha.into(), sessions);
    }

    fn persist(self) -> Result<(), GitAiError> {
        let mut sources = Vec::new();
        for (_, (repo, mut commits)) in self.repos {
            commits.retain(|_, sessions| !sessions.is_empty());
            if commits.is_empty() {
                continue;
            }
            let repo_url = repo.get_default_remote().ok().flatten().and_then(|name| {
                repo.remotes_with_urls()
                    .ok()?
                    .into_iter()
                    .find(|(n, _)| n == &name)
                    .map(|(_, url)| url)
            });
            sources.push(serde_json::to_string(&Source {
                git_dir: repo.common_dir().to_path_buf(),
                repo_url,
                time_unix_nano: self.time_unix_nano.clone(),
                commits,
            })?);
        }
        if !sources.is_empty() {
            db::Outbox::open()?.enqueue_sources(&self.endpoint, &sources)?;
            worker::wake();
        }
        Ok(())
    }
}

/// Scoped to the *side-effect* future, never the trace listener/normalizer.
/// Task-local state survives awaits without leaking between repository actors.
pub(crate) async fn capture_operation<T>(time_ns: u128, future: impl Future<Output = T>) -> T {
    let Some(endpoint) = endpoint(&Config::fresh()) else {
        return future.await;
    };
    CAPTURE
        .scope(RefCell::new(Capture::new(endpoint, time_ns)), async move {
            let result = std::panic::AssertUnwindSafe(future).catch_unwind().await;
            let capture = CAPTURE.with(|c| {
                let mut capture = c.borrow_mut();
                let replacement = Capture::new(String::new(), 0);
                std::mem::replace(&mut *capture, replacement)
            });
            // Durable acceptance is part of asynchronous side-effect completion.
            // Disk work uses the OTLP pool, not an ingestion runtime worker.
            if !capture.repos.is_empty() {
                let persisted = crate::tokio_runtime::otel_runtime()
                    .spawn_blocking(move || capture.persist())
                    .await;
                if !matches!(persisted, Ok(Ok(()))) {
                    tracing::warn!(?persisted, "otel: could not persist note updates");
                }
            }
            match result {
                Ok(value) => value,
                Err(panic) => std::panic::resume_unwind(panic),
            }
        })
        .await
}

/// Called only after an authorship note has been successfully written.
pub(crate) fn notes_updated<'a>(
    repo: &Repository,
    notes: impl IntoIterator<Item = (&'a str, &'a str)>,
) {
    let mut notes = notes.into_iter();
    if CAPTURE
        .try_with(|capture| {
            for (sha, content) in notes.by_ref() {
                capture.borrow_mut().note(repo, sha, content);
            }
        })
        .is_ok()
    {
        return;
    }
    // Library/direct note writers have no trace command context. Their link is
    // made now; persist it for the daemon's independent periodic worker.
    let Some(endpoint) = endpoint(&Config::fresh()) else {
        return;
    };
    let time_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut capture = Capture::new(endpoint, time_ns);
    for (sha, content) in notes {
        capture.note(repo, sha, content);
    }
    if let Err(error) = capture.persist() {
        tracing::warn!(%error, "otel: could not persist note updates");
    }
}

fn valid_hex(value: &str, lengths: &[usize]) -> bool {
    lengths.contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn string_attribute(key: &str, value: &str) -> Value {
    json!({"key": key, "value": {"stringValue": value}})
}

fn envelope(records: &[Value]) -> Value {
    json!({"resourceLogs": [{
        "resource": {"attributes": [string_attribute("service.name", "git-ai"), string_attribute("service.version", GIT_AI_VERSION)]},
        "scopeLogs": [{"scope": {"name": "git-ai", "version": GIT_AI_VERSION}, "logRecords": records}]
    }]})
}

/// Three Git processes for the entire operation, irrespective of commit count.
fn prepare(source: Source, endpoint: &str) -> Result<Vec<(String, String)>, GitAiError> {
    use crate::git::repository::{exec_git_stdin, from_bare_repository};
    if !source.git_dir.is_dir() {
        return Err(GitAiError::Generic(
            "OTLP source Git directory no longer exists".into(),
        ));
    }
    // Read immutable objects from the common Git directory. A linked worktree
    // can disappear while delivery is queued; its objects still live here.
    let repo = from_bare_repository(&source.git_dir)?;
    let shas: Vec<_> = source.commits.keys().cloned().collect();
    let patch_ids =
        crate::authorship::rewrite_cherry_pick::stable_patch_ids_for_commits(&repo, &shas)?;
    let mut args = repo.global_args_for_exec();
    args.extend(
        [
            "log",
            "--stdin",
            "--no-walk",
            "--no-show-signature",
            "--no-notes",
            "--format=%H%x00%ae%x00%s%x00%b%x00",
        ]
        .map(str::to_string),
    );
    let output = exec_git_stdin(&args, (shas.join("\n") + "\n").as_bytes())?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut records = Vec::new();
    let fields: Vec<_> = text.split('\0').collect();
    for fields in fields.as_chunks::<4>().0 {
        let sha = fields[0].trim();
        let email = fields[1];
        let Some(patch_id) = patch_ids.get(sha).filter(|id| valid_hex(id, &[40])) else {
            continue;
        };
        let Some(sessions) = source.commits.get(sha) else {
            continue;
        };
        if email.is_empty() {
            continue;
        }
        for session in sessions {
            let mut attributes = vec![
                string_attribute("event.name", EVENT),
                string_attribute("git_ai.agent", &session.agent),
                string_attribute("session.id", &session.id),
                string_attribute("git_ai.commit.sha", sha),
                string_attribute("git_ai.commit.patch_id", patch_id),
                string_attribute("git_ai.commit.author_email", email),
                string_attribute("git_ai.commit.subject", fields[2]),
                string_attribute("git_ai.commit.body", fields[3]),
            ];
            if let Some(url) = &source.repo_url {
                attributes.push(string_attribute("vcs.repository.url.full", url));
            }
            let record = json!({"timeUnixNano": source.time_unix_nano, "severityNumber": 9, "severityText": "INFO",
                "body": {"stringValue": EVENT}, "attributes": attributes});
            let identity = serde_json::to_string(&(
                endpoint,
                &session.agent,
                &session.id,
                &source.repo_url,
                sha,
            ))?;
            records.push((identity, serde_json::to_string(&record)?));
        }
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn otel_agent_mapping_preserves_external_ids_and_rejects_placeholders() {
        for (tool, expected) in [
            ("claude", Some("claude-code")),
            ("claude-code", Some("claude-code")),
            ("codex", Some("codex")),
            ("devin", Some("devin")),
            ("mock_ai", None),
            ("cursor", None),
        ] {
            let agent = AgentId {
                tool: tool.into(),
                id: "exact_external_id".into(),
                model: "unused".into(),
            };
            let mapped = session(&agent);
            assert_eq!(mapped.as_ref().map(|s| s.agent.as_str()), expected);
            if let Some(mapped) = mapped {
                assert_eq!(mapped.id, agent.id);
            }
        }
        for id in ["", "unknown", "DEVIN_SESSION123"] {
            assert!(
                session(&AgentId {
                    tool: "devin".into(),
                    id: id.into(),
                    model: String::new()
                })
                .is_none()
            );
        }
    }
}
