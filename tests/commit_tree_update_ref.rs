//! Delayed trace replay and immutable object boundaries for commit linking.
#[macro_use]
#[allow(dead_code)]
#[path = "integration/repos/mod.rs"]
mod repos;

use git_ai::daemon::open_local_socket_stream_with_timeout;
use repos::test_repo::{TestRepo, configure_raw_traced_git_env, real_git_executable};
use serde_json::json;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

fn head(repo: &TestRepo) -> String {
    repo.git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_owned()
}

fn checkpoint(repo: &TestRepo, file: &str, session: &str) {
    let hook = json!({"session_id":session, "cwd":repo.canonical_path(), "hook_event_name":"PostToolUse", "tool_name":"apply_patch", "tool_use_id":file, "tool_input":{"patch":format!("*** Update File: {}\n",repo.path().join(file).display())}});
    repo.git_ai(&["checkpoint", "codex", "--hook-input", &hook.to_string()])
        .unwrap();
}

fn raw(repo: &TestRepo, args: &[&str]) -> String {
    repo.git_og_with_env(args, &[("GIT_TRACE2_EVENT", "0")])
        .unwrap()
}

fn trace_to_file(repo: &TestRepo, args: &[&str], trace: &Path) {
    let mut cmd = Command::new(real_git_executable());
    cmd.arg("-C").arg(repo.path()).args(args);
    configure_raw_traced_git_env(&mut cmd, repo);
    cmd.env("GIT_TRACE2_EVENT", trace);
    let output = cmd.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn replay(repo: &TestRepo, trace: &Path) {
    let mut stream = open_local_socket_stream_with_timeout(
        &repo.daemon_trace_socket_path(),
        Duration::from_secs(2),
    )
    .unwrap();
    stream.write_all(&fs::read(trace).unwrap()).unwrap();
    stream.flush().unwrap();
}

fn seed(repo: &TestRepo) {
    fs::write(repo.path().join("base.txt"), "base\n").unwrap();
    let sha = repo.stage_all_and_commit("Base").unwrap().commit_sha;
    repo.assert_commit_links(&sha, &[]);
}

#[test]
fn raw_commit_trace2_does_not_record_created_commit_oid() {
    let repo = TestRepo::new();
    seed(&repo);
    fs::write(repo.path().join("a.txt"), "a\n").unwrap();
    raw(&repo, &["add", "."]);
    let dir = tempfile::tempdir().unwrap();
    let trace = dir.path().join("commit.trace2");
    trace_to_file(&repo, &["commit", "-m", "Trace only"], &trace);
    let sha = head(&repo);
    assert!(!fs::read_to_string(&trace).unwrap().contains(&sha));
    repo.assert_commit_links(&sha, &[]);
}

#[test]
fn delayed_commit_trace_links_the_captured_commit_after_head_moves() {
    let repo = TestRepo::new();
    seed(&repo);
    fs::write(repo.path().join("agent.txt"), "agent\n").unwrap();
    checkpoint(&repo, "agent.txt", "delayed-session");
    raw(&repo, &["add", "."]);
    repo.sync_daemon();
    let baseline = repo.daemon_total_completion_count();
    let dir = tempfile::tempdir().unwrap();
    let trace = dir.path().join("commit.trace2");
    trace_to_file(&repo, &["commit", "-m", "Delayed agent commit"], &trace);
    let captured = head(&repo);
    repo.assert_commit_links(&captured, &[]);
    fs::write(repo.path().join("later.txt"), "later\n").unwrap();
    raw(&repo, &["add", "."]);
    raw(&repo, &["commit", "-m", "Later untraced commit"]);
    let later = head(&repo);
    repo.assert_commit_links(&later, &[]);
    replay(&repo, &trace);
    repo.wait_for_daemon_total_completion_count(baseline, baseline + 1);
    repo.assert_commit_links(&captured, &[("codex", "delayed-session")]);
    repo.assert_commit_links(&later, &[]);
    // Replayed roots cannot consume the pending set of a later commit window.
    fs::write(repo.path().join("next.txt"), "next\n").unwrap();
    checkpoint(&repo, "next.txt", "next-session");
    replay(&repo, &trace);
    let next = repo.stage_all_and_commit("Next window").unwrap().commit_sha;
    repo.assert_commit_links(&next, &[("codex", "next-session")]);
    repo.assert_commit_links(&captured, &[("codex", "delayed-session")]);
}

#[test]
fn commit_tree_update_ref_transfers_the_rewritten_source_session() {
    let repo = TestRepo::new();
    seed(&repo);
    let base = head(&repo);
    repo.git(&["checkout", "-b", "topic"]).unwrap();
    fs::write(repo.path().join("agent.txt"), "agent\n").unwrap();
    checkpoint(&repo, "agent.txt", "restack-session");
    let old = repo.stage_all_and_commit("Topic").unwrap().commit_sha;
    repo.assert_commit_links(&old, &[("codex", "restack-session")]);
    repo.git(&["checkout", "-b", "upstream", &base]).unwrap();
    fs::write(repo.path().join("upstream.txt"), "upstream\n").unwrap();
    let upstream = repo.stage_all_and_commit("Upstream").unwrap().commit_sha;
    repo.assert_commit_links(&upstream, &[]);
    repo.git(&["checkout", "topic"]).unwrap();
    repo.git(&["merge", "--no-commit", "--no-ff", "upstream"])
        .unwrap();
    let tree = repo.git(&["write-tree"]).unwrap();
    let replacement = repo
        .git(&[
            "commit-tree",
            tree.trim(),
            "-p",
            &upstream,
            "-m",
            "Restacked topic",
        ])
        .unwrap();
    let replacement = replacement.trim();
    repo.assert_commit_links(replacement, &[]);
    repo.git(&["merge", "--abort"]).unwrap();
    repo.git(&["update-ref", "refs/heads/topic", replacement, &old])
        .unwrap();
    repo.assert_commit_links(replacement, &[("codex", "restack-session")]);
    repo.assert_commit_links(&old, &[("codex", "restack-session")]);
    repo.assert_commit_links(&upstream, &[]);
}
