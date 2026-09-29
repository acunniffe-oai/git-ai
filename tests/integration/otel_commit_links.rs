use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use crate::test_utils::codex_checkpoint;
use git_ai::config::{ConfigPatch, NotesBackendConfig, NotesBackendKind};
use serde_json::Value;
use std::fs;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const EVENT: &str = "git_ai.session.linked_commit";

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
    records: Arc<Mutex<Vec<Value>>>,
    _mock: mockito::Mock,
    _backend_mock: mockito::Mock,
    _server: mockito::ServerGuard,
    _dir: tempfile::TempDir,
}

impl Fixture {
    fn new(kind: NotesBackendKind) -> Self {
        Self::with_options(kind, 200, &[])
    }

    fn with_options(kind: NotesBackendKind, status: usize, extra_env: &[(&str, &str)]) -> Self {
        let mut server = mockito::Server::new();
        let backend_mock = server.mock("POST", mockito::Matcher::Regex("^/(api/gitai/)?worker/".into()))
            .with_status(200).with_body(r#"{"errors":[],"success_count":100,"failure_count":0,"enqueued":true,"accepted":0,"dropped":0}"#).create();
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
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("otel-db");
        let backend = NotesBackendConfig {
            kind,
            backend_url: Some(server.url()),
        };
        let patch = ConfigPatch {
            notes_backend: Some(backend.clone()),
            prompt_storage: Some("notes".into()),
            exclude_prompts_in_repositories: Some(vec![]),
            ..Default::default()
        };
        let patch_json = serde_json::to_string(&patch).unwrap();
        let base_url = format!("{}/api/gitai", server.url());
        let mut env = vec![
            ("GIT_AI_API_KEY", "test-otel-key"),
            ("GIT_AI_API_BASE_URL", &base_url),
            ("GIT_AI_TEST_CONFIG_PATCH", &patch_json),
            ("GIT_AI_TEST_OTEL_DB_PATH", db.to_str().unwrap()),
        ];
        env.extend_from_slice(extra_env);
        let mut repo = TestRepo::new_with_daemon_env(&env);
        repo.patch_git_ai_config(|p| p.notes_backend = Some(backend));
        Self {
            repo,
            records,
            _mock: mock,
            _backend_mock: backend_mock,
            _server: server,
            _dir: dir,
        }
    }

    fn wait_for(&self, sha: &str, count: usize) -> Vec<Value> {
        self.repo.sync_daemon();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let matching: Vec<_> = self
                .records
                .lock()
                .unwrap()
                .iter()
                .filter(|r| attribute(r, "git_ai.commit.sha") == sha)
                .cloned()
                .collect();
            if matching.len() >= count {
                return matching;
            }
            assert!(Instant::now() < deadline, "no OTLP links for {sha}");
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn commit(&self) -> String {
        let path = self.repo.path().join("generated.txt");
        codex_checkpoint(&self.repo, &path, "raw-session-id", "PreToolUse", "edit-1");
        fs::write(&path, "AI line\n").unwrap();
        codex_checkpoint(&self.repo, &path, "raw-session-id", "PostToolUse", "edit-1");
        self.repo.git(&["add", "generated.txt"]).unwrap();
        self.repo
            .git(&["commit", "-m", "Subject\n\nCommit body"])
            .unwrap();
        self.repo.sync_daemon();
        self.repo
            .filename("generated.txt")
            .assert_committed_lines(lines!["AI line".ai()]);
        self.repo
            .git(&["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string()
    }
}

fn check_commit_link(kind: NotesBackendKind) {
    let fixture = Fixture::new(kind);
    fixture
        .repo
        .git(&[
            "remote",
            "add",
            "origin",
            "git@github.com:example/otel-test.git",
        ])
        .unwrap();
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let sha = fixture.commit();
    let records = fixture.wait_for(&sha, 1);
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(attribute(record, "event.name"), EVENT);
    assert_eq!(record["body"]["stringValue"], EVENT);
    assert_eq!(attribute(record, "git_ai.agent"), "codex");
    assert_eq!(attribute(record, "session.id"), "raw-session-id");
    assert_eq!(
        attribute(record, "vcs.repository.url.full"),
        "git@github.com:example/otel-test.git"
    );
    assert_eq!(attribute(record, "git_ai.commit.subject"), "Subject");
    assert_eq!(attribute(record, "git_ai.commit.body"), "Commit body\n");
    assert_eq!(attribute(record, "git_ai.commit.patch_id").len(), 40);
    assert!(!attribute(record, "git_ai.commit.author_email").is_empty());
    let timestamp = record["timeUnixNano"]
        .as_str()
        .unwrap()
        .parse::<u128>()
        .unwrap();
    assert!(timestamp >= before);
}

#[test]
fn otel_links_use_logs_while_legacy_metrics_keep_their_upload_route() {
    let metrics_dir = tempfile::tempdir().unwrap();
    let metrics_path = metrics_dir.path().join("metrics.db");
    let mut fixture = Fixture::with_options(
        NotesBackendKind::GitNotes,
        200,
        &[(
            "GIT_AI_TEST_METRICS_DB_PATH",
            metrics_path.to_str().unwrap(),
        )],
    );
    fixture._backend_mock.remove();
    let batches = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = batches.clone();
    let legacy = fixture
        ._server
        .mock("POST", "/api/gitai/worker/metrics/upload")
        .match_header("x-api-key", "test-otel-key")
        .expect_at_least(1)
        .with_status(404)
        .with_body_from_request(move |request| {
            captured
                .lock()
                .unwrap()
                .push(serde_json::from_slice(request.body().unwrap()).unwrap());
            br#"{"error":"not found"}"#.to_vec()
        })
        .create();

    let sha = fixture.commit();
    let response = git_ai::daemon::send_control_request(
        &fixture.repo.daemon_control_socket_path(),
        &git_ai::daemon::ControlRequest::Await { timeout_secs: 15 },
    )
    .unwrap();
    assert_eq!(response.data.unwrap()["otel_remaining"], 0);
    let records = fixture.wait_for(&sha, 1);
    assert_eq!(records.len(), 1);
    assert_eq!(attribute(&records[0], "event.name"), EVENT);

    assert!(legacy.matched(), "legacy metrics must still be uploaded");
    for batch in batches.lock().unwrap().iter() {
        assert_eq!(batch["v"], 1);
        assert!(batch.get("resourceLogs").is_none());
        let events = batch["events"].as_array().unwrap();
        assert!(!events.is_empty());
        for event in events {
            assert!(event["e"].is_u64());
            assert!(event["v"].is_object());
            assert!(event["a"].is_object());
        }
    }
}

#[test]
fn otel_await_reports_permanent_failure_without_retrying_it() {
    let fixture = Fixture::with_options(NotesBackendKind::GitNotes, 403, &[]);
    let sha = fixture.commit();
    fixture.wait_for(&sha, 1);
    for _ in 0..2 {
        let response = git_ai::daemon::send_control_request(
            &fixture.repo.daemon_control_socket_path(),
            &git_ai::daemon::ControlRequest::Await { timeout_secs: 15 },
        )
        .unwrap();
        let data = response.data.unwrap();
        assert_eq!(data["done"], false);
        assert_eq!(data["otel_remaining"], 1);
        assert_eq!(data["otel_failed"], 1);
    }
    assert_eq!(fixture.records.lock().unwrap().len(), 1);
}

#[test]
fn otel_await_flushes_and_successful_links_are_not_resent() {
    let fixture = Fixture::new(NotesBackendKind::GitNotes);
    fixture.commit();
    for _ in 0..2 {
        let response = git_ai::daemon::send_control_request(
            &fixture.repo.daemon_control_socket_path(),
            &git_ai::daemon::ControlRequest::Await { timeout_secs: 15 },
        )
        .unwrap();
        let data = response.data.unwrap();
        assert_eq!(data["otel_remaining"], 0);
        assert_eq!(data["done"], true, "{data}");
    }
    assert_eq!(fixture.records.lock().unwrap().len(), 1);
}

#[test]
fn otel_rebase_git_spawn_count_is_constant() {
    fn count(n: usize) -> usize {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("spawns");
        let fixture = Fixture::with_options(
            NotesBackendKind::GitNotes,
            200,
            &[("GIT_AI_SPAWN_LOG", log.to_str().unwrap())],
        );
        base(&fixture);
        let main = fixture.repo.current_branch();
        fixture.repo.git(&["checkout", "-b", "feature"]).unwrap();
        for index in 0..n {
            let name = format!("file-{index}.txt");
            let path = fixture.repo.path().join(&name);
            codex_checkpoint(&fixture.repo, &path, "scaling-session", "PreToolUse", &name);
            fs::write(&path, "AI line\n").unwrap();
            codex_checkpoint(
                &fixture.repo,
                &path,
                "scaling-session",
                "PostToolUse",
                &name,
            );
            let commit = fixture.repo.stage_all_and_commit(&name).unwrap();
            fixture
                .repo
                .filename(&name)
                .assert_committed_lines(lines!["AI line".ai()]);
            fixture.wait_for(&commit.commit_sha, 1);
        }
        fixture.repo.git(&["checkout", &main]).unwrap();
        fs::write(fixture.repo.path().join("base.txt"), "advance\n").unwrap();
        fixture.repo.stage_all_and_commit("advance").unwrap();
        fixture
            .repo
            .filename("base.txt")
            .assert_committed_lines(lines!["advance".unattributed_human()]);
        fixture.repo.git(&["checkout", "feature"]).unwrap();
        fixture.repo.sync_daemon();
        let before = fs::read_to_string(&log)
            .unwrap()
            .lines()
            .filter(|s| *s == "patch-id")
            .count();
        fixture.repo.git(&["rebase", &main]).unwrap();
        let shas = fixture
            .repo
            .git(&["rev-list", &format!("{main}..HEAD")])
            .unwrap();
        for (index, sha) in shas.lines().enumerate() {
            fixture.wait_for(sha, 1);
            fixture
                .repo
                .filename(&format!("file-{index}.txt"))
                .assert_committed_lines(lines!["AI line".ai()]);
        }
        fs::read_to_string(&log)
            .unwrap()
            .lines()
            .filter(|s| *s == "patch-id")
            .count()
            - before
    }
    let small = count(2);
    let large = count(8);
    assert!(small > 0, "must actually exercise patch-id generation");
    assert_eq!(
        large, small,
        "patch-id processes must not scale with rewritten commits"
    );
}

#[test]
fn otel_claude_and_codex_share_a_commit_with_distinct_raw_sessions() {
    let fixture = Fixture::new(NotesBackendKind::GitNotes);
    let codex_path = fixture.repo.path().join("codex.txt");
    codex_checkpoint(
        &fixture.repo,
        &codex_path,
        "codex-raw-id",
        "PreToolUse",
        "codex-edit",
    );
    fs::write(&codex_path, "Codex line\n").unwrap();
    codex_checkpoint(
        &fixture.repo,
        &codex_path,
        "codex-raw-id",
        "PostToolUse",
        "codex-edit",
    );
    let claude_path = fixture.repo.path().join("claude.txt");
    let transcript = fixture._dir.path().join("claude-session.jsonl");
    fs::write(&transcript, "{}\n").unwrap();
    let hook = |event: &str| {
        serde_json::json!({
        "cwd": fixture.repo.path(), "transcript_path": transcript,
        "session_id": "claude-raw-id", "hook_event_name": event,
        "tool_name": "Write", "tool_use_id": "claude-edit", "tool_input": {"file_path": claude_path},
    }).to_string()
    };
    fixture
        .repo
        .git_ai(&["checkpoint", "claude", "--hook-input", &hook("PreToolUse")])
        .unwrap();
    fs::write(&claude_path, "Claude line\n").unwrap();
    fixture
        .repo
        .git_ai(&["checkpoint", "claude", "--hook-input", &hook("PostToolUse")])
        .unwrap();
    let commit = fixture.repo.stage_all_and_commit("Both agents").unwrap();
    fixture
        .repo
        .filename("codex.txt")
        .assert_committed_lines(lines!["Codex line".ai()]);
    fixture
        .repo
        .filename("claude.txt")
        .assert_committed_lines(lines!["Claude line".ai()]);
    let mut pairs: Vec<_> = fixture
        .wait_for(&commit.commit_sha, 2)
        .into_iter()
        .map(|r| {
            (
                attribute(&r, "git_ai.agent").to_string(),
                attribute(&r, "session.id").to_string(),
            )
        })
        .collect();
    pairs.sort();
    assert_eq!(
        pairs,
        [
            ("claude-code".into(), "claude-raw-id".into()),
            ("codex".into(), "codex-raw-id".into())
        ]
    );
}

#[test]
fn otel_ignores_human_unsupported_and_empty_commits() {
    let fixture = Fixture::new(NotesBackendKind::GitNotes);
    base(&fixture);
    fs::write(fixture.repo.path().join("mock.txt"), "Mock line\n").unwrap();
    fixture
        .repo
        .git_ai(&["checkpoint", "mock_ai", "mock.txt"])
        .unwrap();
    fixture
        .repo
        .stage_all_and_commit("unsupported agent")
        .unwrap();
    fixture
        .repo
        .filename("mock.txt")
        .assert_committed_lines(lines!["Mock line".ai()]);
    let sha = fixture.commit();
    fixture.wait_for(&sha, 1);
    fixture
        .repo
        .git(&["commit", "--allow-empty", "-m", "empty"])
        .unwrap();
    fixture
        .repo
        .filename("generated.txt")
        .assert_committed_lines(lines!["AI line".ai()]);
    let response = git_ai::daemon::send_control_request(
        &fixture.repo.daemon_control_socket_path(),
        &git_ai::daemon::ControlRequest::Await { timeout_secs: 15 },
    )
    .unwrap();
    assert_eq!(response.data.unwrap()["otel_remaining"], 0);
    assert_eq!(fixture.records.lock().unwrap().len(), 1);
}

#[test]
fn otel_is_disabled_without_the_backend_api_key() {
    let fixture = Fixture::with_options(NotesBackendKind::GitNotes, 200, &[("GIT_AI_API_KEY", "")]);
    fixture.commit();
    let response = git_ai::daemon::send_control_request(
        &fixture.repo.daemon_control_socket_path(),
        &git_ai::daemon::ControlRequest::Await { timeout_secs: 15 },
    )
    .unwrap();
    assert_eq!(response.data.unwrap()["otel_remaining"], 0);
    assert!(fixture.records.lock().unwrap().is_empty());
    assert!(!fixture._dir.path().join("otel-db").exists());
}

#[test]
fn otel_retries_while_idle_with_identical_payload() {
    let mut fixture = Fixture::with_options(NotesBackendKind::GitNotes, 503, &[]);
    let sha = fixture.commit();
    let first = fixture.wait_for(&sha, 1);
    fixture._mock.remove();
    let captured = fixture.records.clone();
    fixture._mock = fixture
        ._server
        .mock("POST", "/api/otel/git-ai/v1/logs")
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
    let retried = fixture.wait_for(&sha, 2);
    assert_eq!(first[0], retried[1]);
}

#[test]
fn otel_stalled_uploaders_do_not_block_commits_and_queued_links_ignore_later_head() {
    assert_queued_link_survives(QueuedRepositoryChange::MoveHead);
}

#[test]
fn otel_queued_link_survives_removed_worktree() {
    assert_queued_link_survives(QueuedRepositoryChange::RemoveWorktree);
}

enum QueuedRepositoryChange {
    MoveHead,
    RemoveWorktree,
}

fn assert_queued_link_survives(change: QueuedRepositoryChange) {
    use std::sync::{Condvar, mpsc};
    let mut fixture = Fixture::with_options(
        NotesBackendKind::GitNotes,
        200,
        &[("GIT_AI_TEST_TELEMETRY_UPLOAD_STALL_MS", "60000")],
    );
    fixture._mock.remove();
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    struct Release(Arc<(Mutex<bool>, Condvar)>);
    impl Drop for Release {
        fn drop(&mut self) {
            *self.0.0.lock().unwrap() = true;
            self.0.1.notify_all();
        }
    }
    let release = Release(gate.clone());
    let captured = fixture.records.clone();
    let (started_tx, started_rx) = mpsc::channel();
    fixture._mock = fixture
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
    let first = fixture.commit();
    started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let stall_deadline = Instant::now() + Duration::from_secs(10);
    while !fixture
        .repo
        .daemon_stderr_contents()
        .contains("test telemetry upload stall engaged")
    {
        assert!(
            Instant::now() < stall_deadline,
            "regular upload never stalled"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    let workdir = if matches!(change, QueuedRepositoryChange::RemoveWorktree) {
        let worktree = fixture._dir.path().join("temporary-worktree");
        fixture
            .repo
            .git(&[
                "worktree",
                "add",
                "--detach",
                worktree.to_str().unwrap(),
                &first,
            ])
            .unwrap();
        worktree
    } else {
        fixture.repo.path().to_path_buf()
    };
    let path = workdir.join("generated.txt");
    codex_checkpoint(
        &fixture.repo,
        &path,
        "later-session",
        "PreToolUse",
        "later-edit",
    );
    fs::write(&path, "AI line\nLater AI line\n").unwrap();
    codex_checkpoint(
        &fixture.repo,
        &path,
        "later-session",
        "PostToolUse",
        "later-edit",
    );
    fixture
        .repo
        .git(&["-C", workdir.to_str().unwrap(), "add", "generated.txt"])
        .unwrap();
    fixture
        .repo
        .git(&[
            "-C",
            workdir.to_str().unwrap(),
            "commit",
            "-m",
            "Later commit",
        ])
        .unwrap();
    let second = fixture
        .repo
        .git(&["-C", workdir.to_str().unwrap(), "rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    // Use the standard line-level assertion in the fixture's primary worktree.
    if matches!(change, QueuedRepositoryChange::RemoveWorktree) {
        fixture
            .repo
            .git(&["checkout", "--detach", &second])
            .unwrap();
    }
    fixture
        .repo
        .filename("generated.txt")
        .assert_committed_lines(lines!["AI line".ai(), "Later AI line".ai()]);
    fixture.repo.git(&["checkout", "--detach", &first]).unwrap();
    if matches!(change, QueuedRepositoryChange::RemoveWorktree) {
        fs::remove_dir_all(&workdir).unwrap();
    }
    fixture
        .repo
        .filename("generated.txt")
        .assert_committed_lines(lines!["AI line".ai()]);
    fixture.repo.sync_daemon();
    assert!(
        fixture.records.lock().unwrap().is_empty(),
        "both commits completed while HTTP was blocked"
    );
    let released_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    drop(release);
    let records = fixture.wait_for(&second, 1);
    assert_eq!(
        attribute(&records[0], "git_ai.commit.subject"),
        "Later commit"
    );
    assert!(
        records[0]["timeUnixNano"]
            .as_str()
            .unwrap()
            .parse::<u128>()
            .unwrap()
            < released_at
    );
}

#[test]
fn otel_commit_link_git_notes() {
    check_commit_link(NotesBackendKind::GitNotes);
}

#[test]
fn otel_commit_link_http_notes() {
    check_commit_link(NotesBackendKind::Http);
}

#[test]
fn otel_amend_preserves_patch_id_but_emits_new_sha() {
    let fixture = Fixture::new(NotesBackendKind::GitNotes);
    let original = fixture.commit();
    let first = fixture.wait_for(&original, 1);
    fixture
        .repo
        .git(&["commit", "--amend", "-m", "New subject"])
        .unwrap();
    fixture
        .repo
        .filename("generated.txt")
        .assert_committed_lines(lines!["AI line".ai()]);
    let amended = fixture
        .repo
        .git(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    assert_ne!(original, amended);
    let second = fixture.wait_for(&amended, 1);
    assert_eq!(
        attribute(&first[0], "git_ai.commit.patch_id"),
        attribute(&second[0], "git_ai.commit.patch_id")
    );
    assert_eq!(
        attribute(&second[0], "git_ai.commit.subject"),
        "New subject"
    );
    assert!(
        second[0]["timeUnixNano"]
            .as_str()
            .unwrap()
            .parse::<u128>()
            .unwrap()
            > first[0]["timeUnixNano"]
                .as_str()
                .unwrap()
                .parse::<u128>()
                .unwrap()
    );
}

fn base(fixture: &Fixture) {
    fs::write(fixture.repo.path().join("base.txt"), "base\n").unwrap();
    fixture.repo.stage_all_and_commit("base").unwrap();
    fixture
        .repo
        .filename("base.txt")
        .assert_committed_lines(lines!["base".unattributed_human()]);
}

#[test]
fn otel_rebase_and_cherry_pick_link_each_rewritten_sha() {
    let fixture = Fixture::new(NotesBackendKind::GitNotes);
    base(&fixture);
    let main = fixture.repo.current_branch();
    fixture.repo.git(&["branch", "pick-target"]).unwrap();
    fixture.repo.git(&["checkout", "-b", "feature"]).unwrap();
    let original = fixture.commit();
    let first = fixture.wait_for(&original, 1);
    fixture.repo.git(&["checkout", &main]).unwrap();
    fs::write(fixture.repo.path().join("base.txt"), "base\nadvance\n").unwrap();
    fixture.repo.stage_all_and_commit("advance main").unwrap();
    fixture
        .repo
        .filename("base.txt")
        .assert_committed_lines(lines![
            "base".unattributed_human(),
            "advance".unattributed_human()
        ]);
    fixture.repo.git(&["checkout", "feature"]).unwrap();
    fixture.repo.git(&["rebase", &main]).unwrap();
    fixture
        .repo
        .filename("generated.txt")
        .assert_committed_lines(lines!["AI line".ai()]);
    let rebased = fixture
        .repo
        .git(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    let second = fixture.wait_for(&rebased, 1);
    assert_ne!(original, rebased);
    assert_eq!(
        attribute(&first[0], "git_ai.commit.patch_id"),
        attribute(&second[0], "git_ai.commit.patch_id")
    );
    fixture.repo.git(&["checkout", "pick-target"]).unwrap();
    fixture.repo.git(&["cherry-pick", "-x", &original]).unwrap();
    fixture
        .repo
        .filename("generated.txt")
        .assert_committed_lines(lines!["AI line".ai()]);
    let picked = fixture
        .repo
        .git(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    assert_ne!(original, picked);
    let third = fixture.wait_for(&picked, 1);
    assert_eq!(
        attribute(&first[0], "git_ai.commit.patch_id"),
        attribute(&third[0], "git_ai.commit.patch_id")
    );
}

#[test]
fn otel_squash_links_both_raw_sessions() {
    let fixture = Fixture::new(NotesBackendKind::GitNotes);
    base(&fixture);
    let main = fixture.repo.current_branch();
    fixture.repo.git(&["checkout", "-b", "feature"]).unwrap();
    let original = fixture.commit();
    fixture.wait_for(&original, 1);
    let path = fixture.repo.path().join("second.txt");
    codex_checkpoint(
        &fixture.repo,
        &path,
        "second-session",
        "PreToolUse",
        "edit-2",
    );
    fs::write(&path, "Second AI line\n").unwrap();
    codex_checkpoint(
        &fixture.repo,
        &path,
        "second-session",
        "PostToolUse",
        "edit-2",
    );
    fixture.repo.stage_all_and_commit("second session").unwrap();
    fixture
        .repo
        .filename("second.txt")
        .assert_committed_lines(lines!["Second AI line".ai()]);
    fixture.repo.git(&["checkout", &main]).unwrap();
    fixture.repo.git(&["merge", "--squash", "feature"]).unwrap();
    fixture.repo.git(&["commit", "-m", "squashed"]).unwrap();
    fixture
        .repo
        .filename("generated.txt")
        .assert_committed_lines(lines!["AI line".ai()]);
    fixture
        .repo
        .filename("second.txt")
        .assert_committed_lines(lines!["Second AI line".ai()]);
    let sha = fixture
        .repo
        .git(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    let records = fixture.wait_for(&sha, 2);
    let mut sessions: Vec<_> = records.iter().map(|r| attribute(r, "session.id")).collect();
    sessions.sort();
    assert_eq!(sessions, ["raw-session-id", "second-session"]);
}
