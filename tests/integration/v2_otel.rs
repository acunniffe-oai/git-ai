use crate::repos::test_repo::TestRepo;
use crate::test_utils::codex_checkpoint;
use git_ai::commit_links::store::LinkStore;
use serde_json::Value;
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

fn attribute<'a>(record: &'a Value, key: &str) -> &'a str {
    record["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["key"] == key)
        .unwrap_or_else(|| panic!("missing {key}: {record}"))["value"]["stringValue"]
        .as_str()
        .unwrap()
}

struct Fixture {
    repo: TestRepo,
    db: std::path::PathBuf,
    records: Arc<Mutex<Vec<Value>>>,
    _mock: mockito::Mock,
    _logs: mockito::Mock,
    _server: mockito::ServerGuard,
    _dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self::with_status(200)
    }

    fn with_status(status: usize) -> Self {
        let mut server = mockito::Server::new();
        let records = Arc::new(Mutex::new(Vec::new()));
        let captured = records.clone();
        let mock = server
            .mock("POST", "/api/otel/git-ai/v1/logs")
            .match_header("authorization", "Bearer test-otel-key")
            .match_header("content-type", "application/json")
            .with_status(status)
            .with_body_from_request(move |request| {
                let body: Value = serde_json::from_slice(request.body().unwrap()).unwrap();
                for resource in body["resourceLogs"].as_array().unwrap() {
                    for scope in resource["scopeLogs"].as_array().unwrap() {
                        captured
                            .lock()
                            .unwrap()
                            .extend(scope["logRecords"].as_array().unwrap().clone());
                    }
                }
                b"{}".to_vec()
            })
            .create();
        let logs = server
            .mock("POST", "/api/gitai/worker/logs/upload")
            .with_status(200)
            .with_body(r#"{"accepted":100,"dropped":0,"enqueued":true,"errors":[]}"#)
            .create();
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("otel-db");
        let base = format!("{}/api/gitai", server.url());
        let repo = TestRepo::new_with_daemon_env(&[
            ("GIT_AI_API_KEY", "test-otel-key"),
            ("GIT_AI_API_BASE_URL", &base),
            ("GIT_AI_TEST_OTEL_DB_PATH", db.to_str().unwrap()),
        ]);
        Self {
            repo,
            db,
            records,
            _mock: mock,
            _logs: logs,
            _server: server,
            _dir: dir,
        }
    }

    fn edit(&self, content: &str, session: &str) {
        let path = self.repo.path().join("generated.txt");
        codex_checkpoint(&self.repo, &path, session, "PreToolUse", session);
        std::fs::write(&path, content).unwrap();
        codex_checkpoint(&self.repo, &path, session, "PostToolUse", session);
    }

    fn assert_links(&self, sha: &str, expected: &[&str]) {
        self.repo.sync_daemon();
        let family = self
            .repo
            .git(&["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .unwrap();
        let sessions: BTreeSet<_> = LinkStore::open_at(&self.db)
            .unwrap()
            .sessions_for_commit(family.trim(), sha)
            .unwrap()
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(sessions, expected.iter().map(|s| s.to_string()).collect());
    }

    fn flush(&self) {
        if let Err(error) = self.repo.git_ai(&["await", "--timeout", "20"]) {
            panic!(
                "{error}\nDaemon status: {}\nDaemon stderr: {}",
                self.repo.daemon_status(),
                self.repo.daemon_stderr_contents()
            );
        }
    }
}

#[test]
fn v2_otel_commit_payload_and_amend_keep_original_ids_without_notes() {
    let f = Fixture::new();
    f.repo
        .git(&[
            "remote",
            "add",
            "origin",
            "git@github.com:example/otel-test.git",
        ])
        .unwrap();
    f.edit("first\n", "raw-session-id");
    let first = f
        .repo
        .stage_all_and_commit("Subject\n\nCommit body")
        .unwrap()
        .commit_sha;
    f.assert_links(&first, &["raw-session-id"]);
    f.flush();
    let records = f.records.lock().unwrap().clone();
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(
        record["body"]["stringValue"],
        "git_ai.session.linked_commit"
    );
    assert_eq!(
        attribute(record, "event.name"),
        "git_ai.session.linked_commit"
    );
    assert_eq!(attribute(record, "git_ai.agent"), "codex");
    assert_eq!(attribute(record, "session.id"), "raw-session-id");
    assert_eq!(attribute(record, "git_ai.commit.sha"), first);
    assert_eq!(attribute(record, "git_ai.commit.subject"), "Subject");
    assert_eq!(attribute(record, "git_ai.commit.body"), "Commit body\n");
    assert_eq!(
        attribute(record, "vcs.repository.url.full"),
        "git@github.com:example/otel-test.git"
    );
    assert!(!attribute(record, "git_ai.commit.author_email").is_empty());
    assert_eq!(attribute(record, "git_ai.commit.patch_id").len(), 40);

    f.repo
        .git(&["commit", "--amend", "-m", "Amended subject"])
        .unwrap();
    let amended = f
        .repo
        .git(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_owned();
    f.assert_links(&amended, &["raw-session-id"]);
    f.flush();
    let records = f.records.lock().unwrap().clone();
    assert_eq!(records.len(), 2);
    let new = records
        .iter()
        .find(|r| attribute(r, "git_ai.commit.sha") == amended)
        .unwrap();
    assert_eq!(
        attribute(new, "git_ai.commit.patch_id"),
        attribute(record, "git_ai.commit.patch_id")
    );
    assert_eq!(attribute(new, "git_ai.commit.subject"), "Amended subject");
    assert!(
        f.repo
            .git(&["show-ref", "--verify", "refs/notes/ai"])
            .is_err()
    );
    f.flush();
    assert_eq!(f.records.lock().unwrap().len(), 2);
}

#[test]
fn v2_otel_empty_commit_still_keeps_its_local_link() {
    let f = Fixture::new();
    std::fs::write(f.repo.path().join("generated.txt"), "base\n").unwrap();
    let base = f.repo.stage_all_and_commit("Base").unwrap().commit_sha;
    f.assert_links(&base, &[]);
    f.edit("agent write\n", "reverted-session");
    std::fs::write(f.repo.path().join("generated.txt"), "base\n").unwrap();
    f.repo
        .git(&["commit", "--allow-empty", "-m", "Empty"])
        .unwrap();
    let empty = f
        .repo
        .git(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_owned();
    f.assert_links(&empty, &["reverted-session"]);
    f.flush();
    assert!(f.records.lock().unwrap().is_empty());
}

#[test]
fn v2_otel_restart_with_a_new_key_recovers_unauthorized_delivery_without_changing_payload() {
    let mut f = Fixture::with_status(401);
    f.edit("agent\n", "original-id");
    let sha = f
        .repo
        .stage_all_and_commit("Persist across restart")
        .unwrap()
        .commit_sha;
    f.assert_links(&sha, &["original-id"]);
    for _ in 0..2 {
        let response = git_ai::daemon::send_control_request(
            &f.repo.daemon_control_socket_path(),
            &git_ai::daemon::ControlRequest::Await { timeout_secs: 1 },
        )
        .unwrap();
        let data = response.data.unwrap();
        assert_eq!(data["done"], false);
        assert_eq!(data["otel_remaining"], 1);
        assert_eq!(data["otel_failed"], 1);
    }
    let original = f.records.lock().unwrap().clone();
    assert_eq!(
        original.len(),
        1,
        "an unchanged rejected key must not be retried"
    );
    f._mock.remove();
    let captured = f.records.clone();
    f._mock = f
        ._server
        .mock("POST", "/api/otel/git-ai/v1/logs")
        .match_header("authorization", "Bearer changed-key")
        .with_status(200)
        .with_body_from_request(move |request| {
            let body: Value = serde_json::from_slice(request.body().unwrap()).unwrap();
            captured.lock().unwrap().extend(
                body["resourceLogs"][0]["scopeLogs"][0]["logRecords"]
                    .as_array()
                    .unwrap()
                    .clone(),
            );
            b"{}".to_vec()
        })
        .create();
    let base = format!("{}/api/gitai", f._server.url());
    f.repo.restart_dedicated_daemon_with_env_for_test(&[
        ("GIT_AI_API_KEY", "changed-key"),
        ("GIT_AI_API_BASE_URL", &base),
        ("GIT_AI_TEST_OTEL_DB_PATH", f.db.to_str().unwrap()),
    ]);
    f.assert_links(&sha, &["original-id"]);
    f.flush();
    let delivered = f.records.lock().unwrap().clone();
    assert_eq!(delivered.len(), 2);
    assert_eq!(
        delivered[1], original[0],
        "recovery must preserve the original immutable event"
    );
}

#[test]
fn v2_otel_queued_sources_survive_later_head_and_removed_worktree() {
    use std::sync::{Condvar, mpsc};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    let mut f = Fixture::new();
    f._mock.remove();
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    struct Release(Arc<(Mutex<bool>, Condvar)>);
    impl Drop for Release {
        fn drop(&mut self) {
            *self.0.0.lock().unwrap() = true;
            self.0.1.notify_all();
        }
    }
    let release = Release(gate.clone());
    let captured = f.records.clone();
    let (started_tx, started_rx) = mpsc::channel();
    f._mock = f
        ._server
        .mock("POST", "/api/otel/git-ai/v1/logs")
        .with_status(200)
        .with_body_from_request(move |request| {
            let _ = started_tx.send(());
            let (released, _) = gate
                .1
                .wait_timeout_while(
                    gate.0.lock().unwrap(),
                    Duration::from_secs(30),
                    |released| !*released,
                )
                .unwrap();
            drop(released);
            let body: Value = serde_json::from_slice(request.body().unwrap()).unwrap();
            captured.lock().unwrap().extend(
                body["resourceLogs"][0]["scopeLogs"][0]["logRecords"]
                    .as_array()
                    .unwrap()
                    .clone(),
            );
            b"{}".to_vec()
        })
        .create();
    f.edit("first\n", "first-session");
    let first = f.repo.stage_all_and_commit("First").unwrap().commit_sha;
    f.assert_links(&first, &["first-session"]);
    started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let worktree = f._dir.path().join("temporary-worktree");
    f.repo
        .git(&[
            "worktree",
            "add",
            "--detach",
            worktree.to_str().unwrap(),
            &first,
        ])
        .unwrap();
    let path = worktree.join("generated.txt");
    codex_checkpoint(
        &f.repo,
        &path,
        "queued-session",
        "PreToolUse",
        "queued-edit",
    );
    std::fs::write(&path, "first\nsecond\n").unwrap();
    codex_checkpoint(
        &f.repo,
        &path,
        "queued-session",
        "PostToolUse",
        "queued-edit",
    );
    f.repo
        .git(&["-C", worktree.to_str().unwrap(), "add", "generated.txt"])
        .unwrap();
    f.repo
        .git(&[
            "-C",
            worktree.to_str().unwrap(),
            "commit",
            "-m",
            "Queued commit",
        ])
        .unwrap();
    let second = f
        .repo
        .git(&["-C", worktree.to_str().unwrap(), "rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_owned();
    f.assert_links(&second, &["queued-session"]);
    f.repo.git(&["checkout", "--detach", &second]).unwrap();
    f.repo.git(&["checkout", "--detach", &first]).unwrap();
    f.repo
        .git(&["worktree", "remove", "--force", worktree.to_str().unwrap()])
        .unwrap();
    assert!(!worktree.exists());
    assert!(
        f.records.lock().unwrap().is_empty(),
        "commits must finish while delivery is stalled"
    );
    let released_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    drop(release);
    f.flush();
    let records = f.records.lock().unwrap();
    assert_eq!(records.len(), 2);
    let queued = records
        .iter()
        .find(|r| attribute(r, "git_ai.commit.sha") == second)
        .unwrap();
    assert_eq!(attribute(queued, "session.id"), "queued-session");
    assert_eq!(attribute(queued, "git_ai.commit.subject"), "Queued commit");
    assert!(
        queued["timeUnixNano"]
            .as_str()
            .unwrap()
            .parse::<u128>()
            .unwrap()
            < released_at
    );
}
