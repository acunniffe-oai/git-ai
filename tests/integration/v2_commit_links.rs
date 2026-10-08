//! Production trace2 tests for the repository-level v2 session signal.
use crate::repos::test_repo::TestRepo;
use crate::test_utils::codex_checkpoint;
use git_ai::commit_links::store::LinkStore;
use std::fs;

struct Fixture {
    repo: TestRepo,
    db: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self::with_env(&[])
    }

    fn with_env(extra: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("otel-db");
        let mut env = vec![("GIT_AI_TEST_OTEL_DB_PATH", db.to_str().unwrap())];
        env.extend_from_slice(extra);
        let repo = TestRepo::new_with_daemon_env(&env);
        Self {
            repo,
            db,
            _dir: dir,
        }
    }

    fn edit(&self, name: &str, content: &str, session: &str) {
        let file = self.repo.path().join(name);
        codex_checkpoint(&self.repo, &file, session, "PreToolUse", name);
        fs::write(&file, content).unwrap();
        codex_checkpoint(&self.repo, &file, session, "PostToolUse", name);
    }

    fn head(&self) -> String {
        self.repo
            .git(&["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_owned()
    }

    fn commit(&self, message: &str, sessions: &[&str]) -> String {
        self.repo.stage_all_and_commit(message).unwrap();
        let head = self.head();
        self.assert_links(&head, sessions);
        head
    }

    fn assert_links(&self, sha: &str, sessions: &[&str]) {
        self.repo.sync_daemon();
        let family = self
            .repo
            .git(&["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .unwrap()
            .trim()
            .to_owned();
        let links = LinkStore::open_at(&self.db)
            .unwrap()
            .sessions_for_commit(&family, sha)
            .unwrap();
        let actual: std::collections::BTreeSet<_> =
            links.into_iter().map(|s| (s.agent, s.id)).collect();
        let expected = sessions
            .iter()
            .map(|s| ("codex".to_string(), (*s).to_string()))
            .collect();
        assert_eq!(actual, expected, "links for {sha}");
    }
}

#[test]
fn v2_commit_consumes_sessions_and_preserves_a_reverted_write_signal() {
    let f = Fixture::new();
    f.edit("a.txt", "first\n", "session-one");
    f.edit("b.txt", "second\n", "session-two");
    fs::remove_file(f.repo.path().join("b.txt")).unwrap();
    f.commit(
        "One repository-level signal per session",
        &["session-one", "session-two"],
    );
    fs::write(f.repo.path().join("a.txt"), "human\n").unwrap();
    f.commit("Consumed sessions do not follow unrelated commits", &[]);
    f.edit("a.txt", "again\n", "session-one");
    f.commit("The next write rearms a session", &["session-one"]);
    assert!(
        f.repo
            .git(&["show-ref", "--verify", "refs/notes/ai"])
            .is_err()
    );
}

#[test]
fn v2_amend_unions_existing_and_new_sessions() {
    let f = Fixture::new();
    f.edit("a.txt", "first\n", "first-session");
    let original = f.commit("First", &["first-session"]);
    f.edit("a.txt", "second\n", "second-session");
    f.repo.git(&["add", "."]).unwrap();
    f.repo.git(&["commit", "--amend", "--no-edit"]).unwrap();
    f.assert_links(&f.head(), &["first-session", "second-session"]);
    f.assert_links(&original, &["first-session"]);
}

#[test]
fn v2_rebase_propagates_each_commit_session_without_upstream_sessions() {
    let f = Fixture::new();
    fs::write(f.repo.path().join("base.txt"), "base\n").unwrap();
    let base = f.commit("Base", &[]);
    f.repo.git(&["checkout", "-b", "topic"]).unwrap();
    f.edit("a.txt", "a\n", "topic-a");
    f.commit("Topic A", &["topic-a"]);
    f.edit("b.txt", "b\n", "topic-b");
    f.commit("Topic B", &["topic-b"]);
    f.repo.git(&["checkout", "-b", "upstream", &base]).unwrap();
    f.edit("upstream.txt", "upstream\n", "upstream-session");
    f.commit("Upstream", &["upstream-session"]);
    f.repo.git(&["checkout", "topic"]).unwrap();
    f.repo.git(&["rebase", "upstream"]).unwrap();
    let commits = f
        .repo
        .git(&["rev-list", "--reverse", "upstream..topic"])
        .unwrap();
    let commits: Vec<_> = commits.lines().collect();
    assert_eq!(commits.len(), 2);
    f.assert_links(commits[0], &["topic-a"]);
    f.assert_links(commits[1], &["topic-b"]);
}

#[test]
fn v2_cherry_pick_and_soft_reset_restore_local_session_sets() {
    let f = Fixture::new();
    fs::write(f.repo.path().join("base.txt"), "base\n").unwrap();
    let base = f.commit("Base", &[]);
    f.edit("a.txt", "a\n", "source-session");
    let source = f.commit("Source", &["source-session"]);
    f.repo.git(&["checkout", "-b", "target", &base]).unwrap();
    fs::write(f.repo.path().join("target.txt"), "target\n").unwrap();
    let target = f.commit("Target", &[]);
    f.repo.git(&["cherry-pick", &source]).unwrap();
    f.assert_links(&f.head(), &["source-session"]);
    f.repo.git(&["reset", "--soft", &target]).unwrap();
    f.commit("Recommit", &["source-session"]);
    f.repo.git(&["reset", "--hard", &target]).unwrap();
    fs::write(f.repo.path().join("human.txt"), "human\n").unwrap();
    f.commit("Hard reset does not restore sessions", &[]);
}

#[test]
fn v2_stash_restores_pending_sessions() {
    let f = Fixture::new();
    fs::write(f.repo.path().join("a.txt"), "base\n").unwrap();
    f.commit("Base", &[]);
    f.edit("a.txt", "agent\n", "stashed-session");
    f.repo.git(&["stash", "push"]).unwrap();
    fs::write(f.repo.path().join("unrelated.txt"), "human\n").unwrap();
    f.commit("While stashed", &[]);
    f.repo.git(&["stash", "pop"]).unwrap();
    f.commit("Restored", &["stashed-session"]);
}

#[test]
fn v2_failed_commit_retains_pending_sessions_and_partial_staging_consumes_them() {
    let f = Fixture::new();
    fs::write(f.repo.path().join("a.txt"), "base\n").unwrap();
    f.commit("Base", &[]);
    f.edit("a.txt", "agent a\n", "session-a");
    f.edit("b.txt", "agent b\n", "session-b");
    assert!(f.repo.git(&["commit", "-m", "Nothing staged"]).is_err());
    f.repo.git(&["add", "a.txt"]).unwrap();
    f.repo.git(&["commit", "-m", "Partial staging"]).unwrap();
    f.assert_links(&f.head(), &["session-a", "session-b"]);
    f.commit("Remaining file has no fresh write signal", &[]);
}

#[test]
fn v2_cherry_pick_no_commit_defers_sources_until_the_next_commit() {
    let f = Fixture::new();
    fs::write(f.repo.path().join("base.txt"), "base\n").unwrap();
    let base = f.commit("Base", &[]);
    f.edit("a.txt", "a\n", "source-session");
    let source = f.commit("Source", &["source-session"]);
    f.repo.git(&["checkout", "-b", "target", &base]).unwrap();
    f.repo
        .git(&["cherry-pick", "--no-commit", &source])
        .unwrap();
    f.assert_links(&base, &[]);
    f.commit("Deferred cherry-pick", &["source-session"]);
}

#[test]
fn v2_merge_and_revert_do_not_inherit_ancestor_sessions() {
    let f = Fixture::new();
    fs::write(f.repo.path().join("base.txt"), "base\n").unwrap();
    let base = f.commit("Base", &[]);
    f.repo.git(&["checkout", "-b", "topic"]).unwrap();
    f.edit("topic.txt", "topic\n", "topic-session");
    let source = f.commit("Source", &["topic-session"]);
    f.repo.git(&["checkout", "-b", "target", &base]).unwrap();
    fs::write(f.repo.path().join("target.txt"), "target\n").unwrap();
    f.commit("Target", &[]);
    f.edit("uncommitted.txt", "write signal\n", "merge-session");
    f.repo
        .git(&["merge", "--no-ff", "topic", "-m", "Merge"])
        .unwrap();
    f.assert_links(&f.head(), &["merge-session"]);
    f.repo.git(&["revert", "--no-edit", &source]).unwrap();
    f.assert_links(&f.head(), &[]);
}

#[test]
fn v2_hard_reset_of_the_same_head_clears_pending_sessions() {
    let f = Fixture::new();
    fs::write(f.repo.path().join("a.txt"), "base\n").unwrap();
    f.commit("Base", &[]);
    f.edit("a.txt", "discarded\n", "discarded-session");
    f.repo.git(&["reset", "--hard", "HEAD"]).unwrap();
    fs::write(f.repo.path().join("a.txt"), "human\n").unwrap();
    f.commit("After reset", &[]);
}

#[test]
fn v2_rebase_git_process_count_does_not_scale_with_commit_count() {
    fn count(n: usize) -> usize {
        let log_dir = tempfile::tempdir().unwrap();
        let log = log_dir.path().join("git-spawns");
        let f = Fixture::with_env(&[("GIT_AI_SPAWN_LOG", log.to_str().unwrap())]);
        fs::write(f.repo.path().join("base.txt"), "base\n").unwrap();
        let base = f.commit("Base", &[]);
        f.repo.git(&["checkout", "-b", "topic"]).unwrap();
        for index in 0..n {
            let name = format!("a-{index}.txt");
            f.edit(&name, "agent\n", "scaling-session");
            f.commit(&name, &["scaling-session"]);
        }
        f.repo.git(&["checkout", "-b", "upstream", &base]).unwrap();
        fs::write(f.repo.path().join("upstream.txt"), "upstream\n").unwrap();
        f.commit("Upstream", &[]);
        f.repo.git(&["checkout", "topic"]).unwrap();
        f.repo.sync_daemon();
        let before = fs::read_to_string(&log).unwrap_or_default().lines().count();
        f.repo.git(&["rebase", "upstream"]).unwrap();
        f.repo.sync_daemon();
        let after = fs::read_to_string(&log).unwrap().lines().count();
        for sha in f
            .repo
            .git(&["rev-list", "upstream..topic"])
            .unwrap()
            .lines()
        {
            f.assert_links(sha, &["scaling-session"]);
        }
        after - before
    }
    let small = count(2);
    let large = count(8);
    assert!(small > 0, "must exercise the Git rewrite processing");
    assert_eq!(
        large, small,
        "Git processes must remain constant per rewrite"
    );
}

#[test]
fn v2_rebase_autosquash_unions_sessions_from_each_source_commit() {
    let f = Fixture::new();
    fs::write(f.repo.path().join("base.txt"), "base\n").unwrap();
    let base = f.commit("Base", &[]);
    f.repo.git(&["checkout", "-b", "topic"]).unwrap();
    f.edit("a.txt", "a\n", "original-session");
    f.commit("Topic", &["original-session"]);
    f.edit("b.txt", "b\n", "fixup-session");
    f.commit("fixup! Topic", &["fixup-session"]);
    f.repo.git(&["checkout", "-b", "upstream", &base]).unwrap();
    fs::write(f.repo.path().join("upstream.txt"), "upstream\n").unwrap();
    f.commit("Upstream", &[]);
    f.repo.git(&["checkout", "topic"]).unwrap();
    f.repo
        .git_with_env(
            &["rebase", "-i", "--autosquash", "upstream"],
            &[("GIT_SEQUENCE_EDITOR", "true"), ("GIT_EDITOR", "true")],
            None,
        )
        .unwrap();
    f.assert_links(&f.head(), &["original-session", "fixup-session"]);
}

#[test]
fn v2_squash_merge_defers_all_source_sessions() {
    let f = Fixture::new();
    fs::write(f.repo.path().join("base.txt"), "base\n").unwrap();
    let base = f.commit("Base", &[]);
    f.repo.git(&["checkout", "-b", "topic"]).unwrap();
    f.edit("a.txt", "a\n", "source-one");
    f.commit("One", &["source-one"]);
    f.edit("b.txt", "b\n", "source-two");
    f.commit("Two", &["source-two"]);
    f.repo.git(&["checkout", "-b", "target", &base]).unwrap();
    f.repo.git(&["merge", "--squash", "topic"]).unwrap();
    f.assert_links(&base, &[]);
    f.commit("Squashed", &["source-one", "source-two"]);
}

#[test]
fn v2_rebase_conflict_resolution_unions_the_new_agent_session() {
    let f = Fixture::new();
    fs::write(f.repo.path().join("a.txt"), "base\n").unwrap();
    let base = f.commit("Base", &[]);
    f.repo.git(&["checkout", "-b", "topic"]).unwrap();
    f.edit("a.txt", "topic\n", "topic-session");
    f.commit("Topic", &["topic-session"]);
    f.repo.git(&["checkout", "-b", "upstream", &base]).unwrap();
    fs::write(f.repo.path().join("a.txt"), "upstream\n").unwrap();
    let upstream = f.commit("Upstream", &[]);
    f.repo.git(&["checkout", "topic"]).unwrap();
    f.edit("transient.txt", "transient\n", "pending-session");
    fs::remove_file(f.repo.path().join("transient.txt")).unwrap();
    assert!(f.repo.git(&["rebase", "upstream"]).is_err());
    f.assert_links(&upstream, &[]);
    f.edit("a.txt", "topic and upstream\n", "resolution-session");
    f.repo.git(&["add", "a.txt"]).unwrap();
    f.repo
        .git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();
    f.assert_links(
        &f.head(),
        &["topic-session", "resolution-session", "pending-session"],
    );
}

#[test]
fn v2_partial_stash_retains_sessions_for_the_remaining_worktree() {
    let f = Fixture::new();
    fs::write(f.repo.path().join("a.txt"), "base\n").unwrap();
    fs::write(f.repo.path().join("b.txt"), "base\n").unwrap();
    f.commit("Base", &[]);
    f.edit("a.txt", "agent a\n", "a-session");
    f.edit("b.txt", "agent b\n", "b-session");
    f.repo.git(&["stash", "push", "a.txt"]).unwrap();
    f.commit("Remaining work", &["a-session", "b-session"]);
    f.repo.git(&["stash", "pop"]).unwrap();
    f.commit("Stashed work", &["a-session", "b-session"]);
}

#[test]
fn v2_linked_worktrees_have_separate_pending_sessions() {
    let f = Fixture::new();
    fs::write(f.repo.path().join("base.txt"), "base\n").unwrap();
    f.commit("Base", &[]);
    let linked = f._dir.path().join("linked");
    f.repo
        .git(&["worktree", "add", "-b", "linked", linked.to_str().unwrap()])
        .unwrap();
    f.edit("main.txt", "main\n", "main-session");
    let linked_file = linked.join("linked.txt");
    codex_checkpoint(
        &f.repo,
        &linked_file,
        "linked-session",
        "PreToolUse",
        "linked-edit",
    );
    fs::write(&linked_file, "linked\n").unwrap();
    codex_checkpoint(
        &f.repo,
        &linked_file,
        "linked-session",
        "PostToolUse",
        "linked-edit",
    );
    f.commit("Main worktree", &["main-session"]);
    f.repo.git_from_working_dir(&linked, &["add", "."]).unwrap();
    f.repo
        .git_from_working_dir(&linked, &["commit", "-m", "Linked worktree"])
        .unwrap();
    let linked_head = f
        .repo
        .git_from_working_dir(&linked, &["rev-parse", "HEAD"])
        .unwrap();
    f.assert_links(linked_head.trim(), &["linked-session"]);
}

#[test]
fn v2_commits_from_a_subdirectory_share_the_root_pending_set() {
    let f = Fixture::new();
    let subdir = f.repo.path().join("nested");
    fs::create_dir_all(&subdir).unwrap();
    f.edit("nested/a.txt", "agent\n", "subdir-session");
    f.repo.git_from_working_dir(&subdir, &["add", "."]).unwrap();
    f.repo
        .git_from_working_dir(&subdir, &["commit", "-m", "Subdirectory"])
        .unwrap();
    f.assert_links(&f.head(), &["subdir-session"]);
}

#[test]
fn v2_cherry_pick_conflict_preserves_completed_and_remaining_source_links() {
    let mut f = Fixture::new();
    fs::write(f.repo.path().join("a.txt"), "base\n").unwrap();
    let base = f.commit("Base", &[]);
    f.repo.git(&["checkout", "-b", "source"]).unwrap();
    f.edit("b.txt", "b\n", "first-session");
    let first = f.commit("First", &["first-session"]);
    f.edit("a.txt", "source\n", "second-session");
    let second = f.commit("Second", &["second-session"]);
    f.repo.git(&["checkout", "-b", "target", &base]).unwrap();
    fs::write(f.repo.path().join("a.txt"), "target\n").unwrap();
    f.commit("Target", &[]);
    assert!(f.repo.git(&["cherry-pick", &first, &second]).is_err());
    f.assert_links(&f.head(), &["first-session"]);
    let db = f.db.clone();
    f.repo.restart_dedicated_daemon_with_env_for_test(&[(
        "GIT_AI_TEST_OTEL_DB_PATH",
        db.to_str().unwrap(),
    )]);
    f.edit("a.txt", "source and target\n", "resolution-session");
    f.repo.git(&["add", "a.txt"]).unwrap();
    f.repo
        .git_with_env(
            &["cherry-pick", "--continue"],
            &[("GIT_EDITOR", "true")],
            None,
        )
        .unwrap();
    f.assert_links(&f.head(), &["second-session", "resolution-session"]);
}

fn remote_writer(f: &Fixture) -> (TestRepo, TestRepo) {
    let remote =
        TestRepo::new_bare_with_daemon_scope(crate::repos::test_repo::DaemonTestScope::NoDaemon);
    f.repo
        .git(&["remote", "add", "origin", remote.path().to_str().unwrap()])
        .unwrap();
    f.repo.git(&["push", "origin", "HEAD:main"]).unwrap();
    let writer = TestRepo::new();
    writer
        .git(&["remote", "add", "origin", remote.path().to_str().unwrap()])
        .unwrap();
    writer.git(&["fetch", "origin", "main"]).unwrap();
    writer
        .git(&["checkout", "-B", "main", "FETCH_HEAD"])
        .unwrap();
    (remote, writer)
}

#[test]
fn v2_pull_rebase_propagates_local_sessions_without_linking_upstream() {
    let f = Fixture::new();
    fs::write(f.repo.path().join("base.txt"), "base\n").unwrap();
    f.commit("Base", &[]);
    let (_remote, writer) = remote_writer(&f);
    f.edit("local.txt", "local\n", "local-session");
    let source = f.commit("Local", &["local-session"]);
    fs::write(writer.path().join("upstream.txt"), "upstream\n").unwrap();
    let upstream = writer.stage_all_and_commit("Upstream").unwrap().commit_sha;
    writer.assert_commit_links(&upstream, &[]);
    writer.git(&["push", "origin", "main"]).unwrap();
    f.repo.git(&["pull", "--rebase", "origin", "main"]).unwrap();
    assert_ne!(f.head(), source);
    f.assert_links(&f.head(), &["local-session"]);
    f.assert_links(&upstream, &[]);
}

#[test]
fn v2_pull_fast_forward_preserves_pending_sessions_for_the_next_local_commit() {
    let f = Fixture::new();
    fs::write(f.repo.path().join("base.txt"), "base\n").unwrap();
    f.commit("Base", &[]);
    let (_remote, writer) = remote_writer(&f);
    fs::write(writer.path().join("upstream.txt"), "upstream\n").unwrap();
    let upstream = writer.stage_all_and_commit("Upstream").unwrap().commit_sha;
    writer.assert_commit_links(&upstream, &[]);
    writer.git(&["push", "origin", "main"]).unwrap();
    f.edit("pending.txt", "pending\n", "pending-session");
    f.repo
        .git(&["pull", "--ff-only", "origin", "main"])
        .unwrap();
    assert_eq!(f.head(), upstream);
    f.assert_links(&upstream, &[]);
    f.commit("Local work after fast-forward", &["pending-session"]);
}

#[test]
fn v2_cherry_pick_skip_after_restart_discards_only_the_skipped_source() {
    let mut f = Fixture::new();
    fs::write(f.repo.path().join("a.txt"), "base\n").unwrap();
    let base = f.commit("Base", &[]);
    f.repo.git(&["checkout", "-b", "source"]).unwrap();
    f.edit("a.txt", "source\n", "skipped-session");
    let skipped = f.commit("Skipped", &["skipped-session"]);
    f.edit("b.txt", "b\n", "remaining-session");
    let remaining = f.commit("Remaining", &["remaining-session"]);
    f.repo.git(&["checkout", "-b", "target", &base]).unwrap();
    fs::write(f.repo.path().join("a.txt"), "target\n").unwrap();
    let target = f.commit("Target", &[]);
    assert!(f.repo.git(&["cherry-pick", &skipped, &remaining]).is_err());
    f.assert_links(&target, &[]);
    let db = f.db.clone();
    f.repo.restart_dedicated_daemon_with_env_for_test(&[(
        "GIT_AI_TEST_OTEL_DB_PATH",
        db.to_str().unwrap(),
    )]);
    f.repo.git(&["cherry-pick", "--skip"]).unwrap();
    f.assert_links(&f.head(), &["remaining-session"]);
}

#[test]
fn v2_cherry_pick_abort_after_restart_clears_sources_and_keeps_fresh_writes() {
    let mut f = Fixture::new();
    fs::write(f.repo.path().join("a.txt"), "base\n").unwrap();
    let base = f.commit("Base", &[]);
    f.edit("a.txt", "source\n", "aborted-session");
    let source = f.commit("Source", &["aborted-session"]);
    f.repo.git(&["checkout", "-b", "target", &base]).unwrap();
    fs::write(f.repo.path().join("a.txt"), "target\n").unwrap();
    let target = f.commit("Target", &[]);
    assert!(f.repo.git(&["cherry-pick", &source]).is_err());
    f.assert_links(&target, &[]);
    let db = f.db.clone();
    f.repo.restart_dedicated_daemon_with_env_for_test(&[(
        "GIT_AI_TEST_OTEL_DB_PATH",
        db.to_str().unwrap(),
    )]);
    f.edit("fresh.txt", "fresh\n", "fresh-session");
    f.repo.git(&["cherry-pick", "--abort"]).unwrap();
    f.commit("Fresh work after abort", &["fresh-session"]);
}

#[test]
fn v2_repeated_no_commit_cherry_picks_accumulate_source_sessions() {
    let f = Fixture::new();
    fs::write(f.repo.path().join("base.txt"), "base\n").unwrap();
    let base = f.commit("Base", &[]);
    f.edit("a.txt", "a\n", "first-source");
    let first = f.commit("First source", &["first-source"]);
    f.edit("b.txt", "b\n", "second-source");
    let second = f.commit("Second source", &["second-source"]);
    f.repo.git(&["checkout", "-b", "target", &base]).unwrap();
    f.repo.git(&["cherry-pick", "--no-commit", &first]).unwrap();
    f.repo
        .git(&["cherry-pick", "--no-commit", &second])
        .unwrap();
    f.commit("Combined sources", &["first-source", "second-source"]);
}

#[test]
fn v2_default_merge_fast_forward_to_an_existing_merge_keeps_pending_sessions() {
    let f = Fixture::new();
    fs::write(f.repo.path().join("base.txt"), "base\n").unwrap();
    let base = f.commit("Base", &[]);
    let main = f.repo.current_branch();
    f.repo.git(&["checkout", "-b", "side"]).unwrap();
    fs::write(f.repo.path().join("side.txt"), "side\n").unwrap();
    f.commit("Side", &[]);
    f.repo.git(&["checkout", &main]).unwrap();
    f.repo
        .git(&["merge", "--no-ff", "side", "-m", "Published merge"])
        .unwrap();
    let existing = f.head();
    f.assert_links(&existing, &[]);
    f.repo.git(&["checkout", "-b", "behind", &base]).unwrap();
    f.edit("pending.txt", "pending\n", "pending-session");
    f.repo.git(&["merge", &main]).unwrap();
    assert_eq!(f.head(), existing);
    f.assert_links(&existing, &[]);
    f.commit("Later local commit", &["pending-session"]);
}

#[test]
fn v2_default_pull_fast_forward_to_an_existing_merge_keeps_pending_sessions() {
    let f = Fixture::new();
    fs::write(f.repo.path().join("base.txt"), "base\n").unwrap();
    f.commit("Base", &[]);
    let (_remote, writer) = remote_writer(&f);
    writer.git(&["checkout", "-b", "side"]).unwrap();
    fs::write(writer.path().join("side.txt"), "side\n").unwrap();
    let side = writer.stage_all_and_commit("Side").unwrap().commit_sha;
    writer.assert_commit_links(&side, &[]);
    writer.git(&["checkout", "main"]).unwrap();
    writer
        .git(&["merge", "--no-ff", "side", "-m", "Published merge"])
        .unwrap();
    let existing = writer
        .git(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_owned();
    writer.assert_commit_links(&existing, &[]);
    writer.git(&["push", "origin", "main"]).unwrap();
    f.edit("pending.txt", "pending\n", "pending-session");
    f.repo
        .git(&["pull", "--no-rebase", "origin", "main"])
        .unwrap();
    assert_eq!(f.head(), existing);
    f.assert_links(&existing, &[]);
    f.commit("Later local commit", &["pending-session"]);
}

#[test]
fn v2_pull_rebase_autostash_keeps_uncommitted_sessions_for_their_later_commit() {
    let f = Fixture::new();
    fs::write(f.repo.path().join("base.txt"), "base\n").unwrap();
    f.commit("Base", &[]);
    let (_remote, writer) = remote_writer(&f);
    f.edit("local.txt", "local\n", "committed-session");
    f.commit("Local", &["committed-session"]);
    fs::write(writer.path().join("upstream.txt"), "upstream\n").unwrap();
    let upstream = writer.stage_all_and_commit("Upstream").unwrap().commit_sha;
    writer.assert_commit_links(&upstream, &[]);
    writer.git(&["push", "origin", "main"]).unwrap();
    f.edit("base.txt", "pending\n", "pending-session");
    f.repo
        .git(&["pull", "--rebase", "--autostash", "origin", "main"])
        .unwrap();
    f.assert_links(&f.head(), &["committed-session"]);
    f.assert_links(&upstream, &[]);
    assert_eq!(
        fs::read_to_string(f.repo.path().join("base.txt")).unwrap(),
        "pending\n"
    );
    f.commit("Pending work after rebase", &["pending-session"]);
}

#[test]
fn v2_conflicted_stash_pop_restores_sessions_and_retains_the_saved_set() {
    let f = Fixture::new();
    fs::write(f.repo.path().join("a.txt"), "base\n").unwrap();
    f.commit("Base", &[]);
    f.edit("a.txt", "stashed agent change\n", "stashed-session");
    f.repo.git(&["stash", "push"]).unwrap();
    let stash = f
        .repo
        .git(&["rev-parse", "refs/stash"])
        .unwrap()
        .trim()
        .to_owned();
    fs::write(f.repo.path().join("a.txt"), "conflicting human change\n").unwrap();
    let conflict_base = f.commit("Conflicting human change", &[]);
    assert!(f.repo.git(&["stash", "pop"]).is_err());
    fs::write(
        f.repo.path().join("a.txt"),
        "human resolution including agent change\n",
    )
    .unwrap();
    f.commit(
        "Resolved stash without another agent hook",
        &["stashed-session"],
    );
    assert_eq!(
        f.repo.git(&["rev-parse", "refs/stash"]).unwrap().trim(),
        stash
    );
    // Failed pop must keep the saved set so an additional apply can restore it.
    f.repo.git(&["reset", "--hard", &conflict_base]).unwrap();
    assert!(f.repo.git(&["stash", "apply", &stash]).is_err());
    fs::write(f.repo.path().join("a.txt"), "second human resolution\n").unwrap();
    f.commit("Resolved retained stash", &["stashed-session"]);
}

#[test]
fn v2_stash_restore_flags_preserve_captured_target_sessions() {
    let f = Fixture::new();
    fs::write(f.repo.path().join("a.txt"), "base\n").unwrap();
    f.commit("Base", &[]);
    f.edit("a.txt", "staged agent change\n", "indexed-session");
    f.repo.git(&["add", "a.txt"]).unwrap();
    f.repo.git(&["stash", "push"]).unwrap();
    f.repo.git(&["stash", "pop", "--index"]).unwrap();
    // Commit without staging again: --index restored the staged file too.
    f.repo.git(&["commit", "-m", "Restored index"]).unwrap();
    f.assert_links(&f.head(), &["indexed-session"]);
    f.edit("a.txt", "quiet agent change\n", "quiet-session");
    f.repo.git(&["stash", "push"]).unwrap();
    let stash = f
        .repo
        .git(&["rev-parse", "refs/stash"])
        .unwrap()
        .trim()
        .to_owned();
    f.repo.git(&["stash", "apply", "-q", &stash]).unwrap();
    f.commit("Quiet explicit stash", &["quiet-session"]);
}
