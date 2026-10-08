//! Commit/session links for the configured backend's OTLP collector.
//! Transactional local links enqueue operation batches. Git enrichment and HTTP delivery
//! run on a bounded OTLP runtime, independently of the regular telemetry flusher.
pub(crate) mod db;
mod worker;

use crate::commit_links::store::SessionIdentity;
use crate::config::Config;
use crate::error::GitAiError;
use crate::version::GIT_AI_VERSION;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

pub(crate) use worker::{flush_and_wait, spawn_worker};

const EVENT: &str = "git_ai.session.linked_commit";

type Session = SessionIdentity;

#[derive(Serialize, Deserialize)]
pub(crate) struct Source {
    pub(crate) git_dir: PathBuf,
    pub(crate) repo_url: Option<String>,
    pub(crate) time_unix_nano: String,
    #[serde(default)]
    pub(crate) commit_times: BTreeMap<String, String>,
    pub(crate) commits: BTreeMap<String, BTreeSet<Session>>,
}

pub(crate) fn endpoint(config: &Config) -> Option<String> {
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

pub(crate) fn wake() {
    worker::wake();
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
    let patch_ids = crate::commit_links::git::stable_patch_ids_for_commits(&repo, &shas)?;
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
            let time = source
                .commit_times
                .get(sha)
                .unwrap_or(&source.time_unix_nano);
            let record = json!({"timeUnixNano": time, "severityNumber": 9, "severityText": "INFO",
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
    fn v2_existing_outbox_source_payload_remains_readable() {
        let source: Source = serde_json::from_value(json!({
            "git_dir":"/repo/.git", "repo_url":null, "time_unix_nano":"123",
            "commits":{"abc":[{"agent":"codex","id":"original-id"}]}
        }))
        .unwrap();
        assert!(source.commit_times.is_empty());
        assert_eq!(source.time_unix_nano, "123");
        assert_eq!(
            source.commits["abc"].iter().next().unwrap().id,
            "original-id"
        );
    }

    #[test]
    fn otel_agent_mapping_preserves_external_ids_and_rejects_placeholders() {
        for (tool, expected) in [
            ("claude", Some("claude-code")),
            ("claude-code", Some("claude-code")),
            ("github-copilot", Some("github-copilot")),
            ("github-copilot-cli", Some("github-copilot-cli")),
            ("codex", Some("codex")),
            ("devin", Some("devin")),
            ("mock_ai", None),
            ("cursor", Some("cursor")),
            ("windsurf", Some("windsurf")),
            ("cline", Some("cline")),
            ("gemini", Some("gemini")),
            ("continue-cli", Some("continue-cli")),
            ("amp", Some("amp")),
            ("firebender", Some("firebender")),
            ("agent-v1", Some("agent-v1")),
            ("droid", Some("droid")),
            ("opencode", Some("opencode")),
            ("pi", Some("pi")),
        ] {
            let mapped = SessionIdentity::new(tool, "exact_external_id");
            assert_eq!(mapped.as_ref().map(|s| s.agent.as_str()), expected);
            if let Some(mapped) = mapped {
                assert_eq!(mapped.id, "exact_external_id");
            }
        }
        for id in ["", "unknown", "DEVIN_SESSION123"] {
            assert!(SessionIdentity::new("devin", id).is_none());
        }
    }
}
