//! Cold-cursor and untraced-history boundaries remain relevant without notes.
use crate::repos::test_repo::{DaemonTestScope, TestRepo};
use crate::test_utils::codex_checkpoint;
use std::fs;

const TRACE_DISABLED: [(&str, &str); 3] = [
    ("GIT_TRACE2", "0"),
    ("GIT_TRACE2_EVENT", "0"),
    ("GIT_TRACE2_PERF", "0"),
];

fn raw(repo: &TestRepo, args: &[&str]) -> String {
    repo.git_og_with_env(args, &TRACE_DISABLED).unwrap()
}

fn raw_commit(repo: &TestRepo, name: &str, contents: &str, message: &str) -> String {
    fs::write(repo.path().join(name), contents).unwrap();
    raw(repo, &["add", "."]);
    raw(repo, &["commit", "-m", message]);
    raw(repo, &["rev-parse", "HEAD"]).trim().to_owned()
}

#[test]
fn cold_repo_first_traced_commit_links_only_its_pending_session() {
    let mut repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let old = raw_commit(&repo, "a.txt", "before daemon\n", "Before daemon");
    repo.start_dedicated_daemon_for_test();
    repo.assert_commit_links(&old, &[]);
    let file = repo.path().join("a.txt");
    fs::write(&file, "agent write\n").unwrap();
    codex_checkpoint(&repo, &file, "cold-session", "PostToolUse", "cold-edit");
    let new = repo
        .stage_all_and_commit("First traced commit")
        .unwrap()
        .commit_sha;
    repo.assert_commit_links(&new, &[("codex", "cold-session")]);
    repo.assert_commit_links(&old, &[]);
}

#[test]
fn cold_repo_amend_after_restart_preserves_stored_sessions() {
    let mut repo = TestRepo::new_dedicated_daemon();
    let file = repo.path().join("a.txt");
    fs::write(&file, "original\n").unwrap();
    codex_checkpoint(&repo, &file, "original-session", "PostToolUse", "one");
    let original = repo.stage_all_and_commit("Original").unwrap().commit_sha;
    repo.assert_commit_links(&original, &[("codex", "original-session")]);
    repo.restart_dedicated_daemon_for_test();
    fs::write(&file, "amended\n").unwrap();
    codex_checkpoint(&repo, &file, "amend-session", "PostToolUse", "two");
    repo.git(&["add", "."]).unwrap();
    repo.git(&["commit", "--amend", "--no-edit"]).unwrap();
    let amended = repo.git(&["rev-parse", "HEAD"]).unwrap();
    repo.assert_commit_links(
        amended.trim(),
        &[("codex", "original-session"), ("codex", "amend-session")],
    );
}

#[test]
fn traced_commit_after_untraced_duplicate_message_does_not_link_the_untraced_commit() {
    let repo = TestRepo::new_dedicated_daemon();
    let base = raw_commit(&repo, "base.txt", "base\n", "Base");
    repo.assert_commit_links(&base, &[]);
    let file = repo.path().join("agent.txt");
    fs::write(&file, "agent\n").unwrap();
    codex_checkpoint(&repo, &file, "traced-session", "PostToolUse", "agent");
    let untraced = raw_commit(&repo, "agent.txt", "agent\n", "Identical message");
    repo.assert_commit_links(&untraced, &[]);
    fs::write(repo.path().join("later.txt"), "later\n").unwrap();
    let traced = repo
        .stage_all_and_commit("Identical message")
        .unwrap()
        .commit_sha;
    repo.assert_commit_links(&traced, &[("codex", "traced-session")]);
    repo.assert_commit_links(&untraced, &[]);
}
