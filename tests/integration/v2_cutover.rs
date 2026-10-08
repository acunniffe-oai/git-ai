use crate::repos::test_repo::TestRepo;

#[test]
fn v2_help_advertises_only_linking_and_management_commands() {
    let repo = TestRepo::new();
    let help = repo.git_ai(&["--help"]).unwrap();
    for command in [
        "blame",
        "usage",
        "stats",
        "show-prompt",
        "fetch-notes",
        "dashboard",
    ] {
        assert!(
            !help.contains(command),
            "v2 help still advertises {command}: {help}"
        );
    }
    for command in ["checkpoint", "install", "daemon", "await", "config"] {
        assert!(help.contains(command), "v2 help omits {command}: {help}");
    }
}

#[test]
fn v2_removed_commands_are_unknown() {
    let repo = TestRepo::new();
    for command in [
        "blame",
        "usage",
        "stats",
        "analyze",
        "show",
        "diff",
        "status",
        "log",
        "notes",
        "fetch-notes",
        "show-prompt",
        "reingest",
        "ci",
    ] {
        let output = repo.git_ai(&[command, "--help"]);
        assert!(
            output.is_err(),
            "removed command {command} still succeeds: {output:?}"
        );
        assert!(output.unwrap_err().contains("Unknown git-ai command"));
    }
}

#[test]
fn v2_abandoned_shell_snapshots_are_bounded_and_cannot_link_later_commits() {
    let repo = TestRepo::new_with_daemon_env(&[("GIT_AI_TEST_SHELL_SNAPSHOT_LIMIT", "2")]);
    let hook = |session: &str, phase: &str| {
        let input = serde_json::json!({
            "session_id": session, "cwd": repo.path(), "hook_event_name": phase,
            "tool_name": "Bash", "tool_use_id": session, "tool_input": {"command": "edit"}
        });
        repo.git_ai(&["checkpoint", "claude", "--hook-input", &input.to_string()])
            .unwrap();
    };
    hook("abandoned", "PreToolUse");
    std::fs::write(repo.path().join("edited.txt"), "first\n").unwrap();
    let first = repo.stage_all_and_commit("before eviction").unwrap();
    repo.assert_commit_links(&first.commit_sha, &[]);
    hook("active-one", "PreToolUse");
    hook("active-two", "PreToolUse");
    std::fs::write(repo.path().join("edited.txt"), "later\n").unwrap();
    let later = repo.stage_all_and_commit("after eviction").unwrap();
    repo.assert_commit_links(&later.commit_sha, &[]);
    hook("abandoned", "PostToolUse");
    repo.assert_commit_links(&first.commit_sha, &[]);
    repo.assert_commit_links(&later.commit_sha, &[]);
}

#[test]
fn v2_replacing_a_shell_invocation_keeps_other_live_evidence_at_capacity() {
    let repo = TestRepo::new_with_daemon_env(&[("GIT_AI_TEST_SHELL_SNAPSHOT_LIMIT", "2")]);
    let hook = |session: &str, phase: &str| {
        let input = serde_json::json!({
            "session_id": session, "cwd": repo.path(), "hook_event_name": phase,
            "tool_name": "Bash", "tool_use_id": session, "tool_input": {"command": "edit"}
        });
        repo.git_ai(&["checkpoint", "claude", "--hook-input", &input.to_string()])
            .unwrap();
    };
    hook("survivor", "PreToolUse");
    hook("replaced", "PreToolUse");
    hook("replaced", "PreToolUse");
    std::fs::write(repo.path().join("edited.txt"), "confirmed write\n").unwrap();
    let commit = repo.stage_all_and_commit("live shell evidence").unwrap();
    repo.assert_commit_links(&commit.commit_sha, &[]);
    hook("survivor", "PostToolUse");
    repo.assert_commit_links(&commit.commit_sha, &[("claude-code", "survivor")]);
}
