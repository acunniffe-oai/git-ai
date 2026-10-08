use crate::repos::test_repo::{DaemonTestScope, TestRepo};
use git_ai::commit_links::checkpoint::ShellInvocationSnapshot;
use git_ai::commit_links::store::{LinkStore, PendingState, SessionIdentity};
use git_ai::daemon::{ControlRequest, send_control_request};
use git_ai::git::repository::discover_repository_in_path_no_git_exec;
use serde_json::json;
use std::collections::BTreeSet;
use std::fs;
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

const SESSION: &str = "independent-native-session";

fn hook(owner: &TestRepo, repo: &TestRepo, event: &str, tool: &str, invocation: &str) -> String {
    let input = json!({
        "cwd":repo.path(),"session_id":SESSION,"hook_event_name":event,
        "tool_name":tool,"tool_use_id":invocation,
        "tool_input":{"file_path":repo.path().join("edited.txt"),"command":"native shell"}
    });
    let control = owner
        .daemon_control_socket_path()
        .to_string_lossy()
        .into_owned();
    let trace = owner
        .daemon_trace_socket_path()
        .to_string_lossy()
        .into_owned();
    repo.git_ai_with_env_without_pre_sync_for_test(
        &["checkpoint", "claude", "--hook-input", &input.to_string()],
        &[
            ("GIT_AI_DAEMON_CONTROL_SOCKET", &control),
            ("GIT_AI_DAEMON_TRACE_SOCKET", &trace),
        ],
    )
    .unwrap()
}

fn attachment(owner: &TestRepo, repo: &TestRepo) -> PendingState {
    let response = send_control_request(
        &owner.daemon_control_socket_path(),
        &ControlRequest::CheckpointAttachment {
            repo_working_dir: repo.path().to_string_lossy().into_owned(),
            session: SessionIdentity::new("claude", SESSION).unwrap(),
        },
    )
    .unwrap();
    assert!(response.ok, "{response:?}");
    serde_json::from_value(response.data.unwrap()).unwrap()
}

fn wait_for_delay(owner: &TestRepo) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !owner
        .daemon_stderr_contents()
        .contains("test side-effect delay started")
    {
        assert!(
            Instant::now() < deadline,
            "side-effect delay was never entered"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn commit_without_sync(owner: &TestRepo, repo: &TestRepo, message: &str) -> String {
    let path = repo.path().to_str().unwrap();
    owner
        .git_without_test_sync_for_test(&["-C", path, "add", "edited.txt"], &[])
        .unwrap();
    owner
        .git_without_test_sync_for_test(&["-C", path, "commit", "-m", message], &[])
        .unwrap();
    owner
        .git_without_test_sync_for_test(&["-C", path, "rev-parse", "HEAD"], &[])
        .unwrap()
        .trim()
        .into()
}

fn assert_links(owner: &TestRepo, repo: &TestRepo, sha: &str, expected: &[&str]) {
    let repository = discover_repository_in_path_no_git_exec(repo.path()).unwrap();
    let family = repository
        .common_dir()
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let expected: BTreeSet<_> = expected
        .iter()
        .map(|id| SessionIdentity::new("claude", id).unwrap())
        .collect();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let actual = LinkStore::open_at(&owner.link_store_path_for_test())
            .unwrap()
            .sessions_for_commit(&family, sha)
            .unwrap();
        if actual == expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "unexpected links for {sha}: {actual:?}, expected {expected:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn v2_checkpoint_queries_do_not_wait_for_another_familys_rebase() {
    check_independent_queries(false);
}

#[test]
fn v2_checkpoint_queries_and_effects_do_not_share_a_stalled_collector_pool() {
    check_independent_queries(true);
}

fn check_independent_queries(stall_collector: bool) {
    let mut owner = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let db = owner.link_store_path_for_test();
    let mut server = mockito::Server::new();
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    struct Release(Arc<(Mutex<bool>, Condvar)>);
    impl Drop for Release {
        fn drop(&mut self) {
            *self.0.0.lock().unwrap() = true;
            self.0.1.notify_all();
        }
    }
    let release_gate = gate.clone();
    let (started_tx, started_rx) = mpsc::channel();
    let _mock = server
        .mock("POST", "/api/otel/git-ai/v1/logs")
        .with_status(200)
        .with_body_from_request(move |_| {
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
            b"{}".to_vec()
        })
        .create();
    let release = Release(release_gate);
    let base_url = server.url();
    let mut env = vec![(
        "GIT_AI_TEST_DELAY_SIDE_EFFECT_MS_FOR_COMMAND",
        "rebase=8000",
    )];
    if stall_collector {
        env.extend([
            ("GIT_AI_API_KEY", "isolated-collector-key"),
            ("GIT_AI_API_BASE_URL", base_url.as_str()),
            ("GIT_AI_TEST_OTEL_DB_PATH", db.to_str().unwrap()),
            ("GIT_AI_DAEMON_LOG_UPLOAD", "false"),
        ]);
    }
    owner.start_dedicated_daemon_with_env_for_test(&env);
    let independent = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    fs::write(owner.path().join("base.txt"), "base\n").unwrap();
    let base = owner.stage_all_and_commit("base").unwrap();
    owner.assert_commit_links(&base.commit_sha, &[]);
    let main = owner.current_branch();
    owner.git(&["checkout", "-b", "topic"]).unwrap();
    fs::write(owner.path().join("edited.txt"), "topic\n").unwrap();
    if stall_collector {
        hook(&owner, &owner, "PostToolUse", "Write", "topic-agent");
    }
    let topic = owner.stage_all_and_commit("topic").unwrap();
    let expected_topic: &[(&str, &str)] = if stall_collector {
        &[("claude", SESSION)]
    } else {
        &[]
    };
    owner.assert_commit_links(&topic.commit_sha, expected_topic);
    if stall_collector {
        started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    }
    owner.git(&["checkout", &main]).unwrap();
    fs::write(owner.path().join("upstream.txt"), "upstream\n").unwrap();
    let upstream = owner.stage_all_and_commit("upstream").unwrap();
    owner.assert_commit_links(&upstream.commit_sha, &[]);
    owner.git(&["checkout", "topic"]).unwrap();

    fs::write(independent.path().join("edited.txt"), "agent write\n").unwrap();
    assert!(
        hook(
            &owner,
            &independent,
            "PostToolUse",
            "Write",
            "initial-write"
        )
        .contains("checkpoint_requests=1")
    );
    // Setup Git activity can still leave a conservatively open trace root.
    // Establish the warm-state precondition before starting the measured grind.
    let warm_deadline = Instant::now() + Duration::from_secs(5);
    let initial = loop {
        let state = attachment(&owner, &independent);
        if state.attached {
            break state;
        }
        assert!(
            Instant::now() < warm_deadline,
            "initial contribution never became attached: {}\n{}",
            owner.daemon_status(),
            owner.daemon_stderr_contents()
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    owner
        .git_without_test_sync_for_test(&["rebase", &main], &[])
        .unwrap();
    wait_for_delay(&owner);

    let started = Instant::now();
    let warm_write = hook(&owner, &independent, "PostToolUse", "Write", "warm-write");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "warm file hook waited {:?} for another family's rebase: {warm_write}",
        started.elapsed()
    );
    assert!(
        warm_write.contains("checkpoint_requests=0"),
        "warm write lost its attached fast path: {warm_write}"
    );
    hook(&owner, &independent, "PreToolUse", "Bash", "warm-shell");
    let response = send_control_request(
        &owner.daemon_control_socket_path(),
        &ControlRequest::CheckpointInvocation {
            repo_working_dir: independent.path().to_string_lossy().into_owned(),
            session: SessionIdentity::new("claude", SESSION).unwrap(),
            invocation_id: "warm-shell".into(),
        },
    )
    .unwrap();
    assert!(response.ok, "{response:?}");
    let snapshot: ShellInvocationSnapshot = serde_json::from_value(response.data.unwrap()).unwrap();
    assert!(snapshot.attached);
    assert!(
        snapshot.snapshot.is_none(),
        "warm shell must do no metadata scan"
    );
    hook(&owner, &independent, "PostToolUse", "Bash", "warm-shell");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "warm hooks in B waited {:?} for unrelated A rebase",
        started.elapsed()
    );

    let first = commit_without_sync(&owner, &independent, "first independent commit");
    let consumed = attachment(&owner, &independent);
    assert!(!consumed.attached);
    assert!(consumed.generation > initial.generation);
    assert_links(&owner, &independent, &first, &[SESSION]);
    fs::write(independent.path().join("edited.txt"), "next-window write\n").unwrap();
    assert!(
        hook(&owner, &independent, "PostToolUse", "Write", "next-write")
            .contains("checkpoint_requests=1")
    );
    let second = commit_without_sync(&owner, &independent, "second independent commit");
    attachment(&owner, &independent);
    assert_links(&owner, &independent, &second, &[SESSION]);

    // The global sync command keeps its existing all-work barrier semantics.
    let response = send_control_request(
        &owner.daemon_control_socket_path(),
        &ControlRequest::SyncFamily {
            repo_working_dir: owner.path().to_string_lossy().into_owned(),
        },
    )
    .unwrap();
    assert!(response.ok, "{response:?}");
    let rebased = owner.git(&["rev-parse", "HEAD"]).unwrap().trim().to_owned();
    owner.assert_commit_links(&rebased, expected_topic);
    drop(release);
    if stall_collector {
        let response = send_control_request(
            &owner.daemon_control_socket_path(),
            &ControlRequest::Await { timeout_secs: 20 },
        )
        .unwrap();
        assert_eq!(response.data.unwrap()["done"], true);
    }
}

#[test]
fn v2_checkpoint_query_waits_for_its_own_commit_before_skipping_a_write() {
    let owner = TestRepo::new_with_daemon_env(&[(
        "GIT_AI_TEST_DELAY_SIDE_EFFECT_MS_FOR_COMMAND",
        "commit=1000",
    )]);
    fs::write(owner.path().join("edited.txt"), "first\n").unwrap();
    hook(&owner, &owner, "PostToolUse", "Write", "first-write");
    assert!(attachment(&owner, &owner).attached);
    let first = commit_without_sync(&owner, &owner, "delayed first commit");
    wait_for_delay(&owner);
    fs::write(owner.path().join("edited.txt"), "later agent write\n").unwrap();
    assert!(
        hook(&owner, &owner, "PostToolUse", "Write", "later-write")
            .contains("checkpoint_requests=1"),
        "query must see the consumed commit window"
    );
    assert_links(&owner, &owner, &first, &[SESSION]);
    let second = commit_without_sync(&owner, &owner, "later commit");
    attachment(&owner, &owner);
    assert_links(&owner, &owner, &second, &[SESSION]);
}
