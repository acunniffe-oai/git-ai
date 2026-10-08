use crate::repos::test_repo::TestRepo;
use git_ai::commands::checkpoint_agent::bash_tool;
use git_ai::commands::checkpoint_agent::presets::{ParsedHookEvent, resolve_preset};
use serde_json::json;
use std::fs;

#[test]
fn v2_checkpoint_claude_accepts_session_without_transcript() {
    let repo = TestRepo::new();
    let path = repo.path().join("edited.txt");
    fs::write(&path, "agent edit\n").unwrap();
    let input = json!({
        "session_id": "external/session-17",
        "cwd": repo.path(),
        "hook_event_name": "PostToolUse",
        "tool_name": "Write",
        "tool_use_id": "write-17",
        "tool_input": {"file_path": path}
    });
    let events = resolve_preset("claude")
        .unwrap()
        .parse(&input.to_string(), "test")
        .unwrap();
    assert!(matches!(&events[..], [ParsedHookEvent::PostFileEdit(event)]
        if event.context.external_session_id == "external/session-17"));
}

#[test]
fn v2_checkpoint_claude_never_infers_session_from_transcript() {
    let repo = TestRepo::new();
    let input = json!({
        "transcript_path": repo.path().join("invented-session.jsonl"),
        "cwd": repo.path(),
        "hook_event_name": "PostToolUse",
        "tool_name": "Write",
        "tool_input": {"file_path": repo.path().join("edited.txt")}
    });
    let events = resolve_preset("claude")
        .unwrap()
        .parse(&input.to_string(), "test")
        .unwrap();
    assert!(
        events.is_empty(),
        "missing external session ID must not create a link"
    );
}

#[test]
fn v2_checkpoint_rejects_removed_human_and_tab_presets() {
    for name in ["human", "known_human", "mock_known_human", "ai_tab"] {
        assert!(
            resolve_preset(name).is_err(),
            "removed preset {name} is still registered"
        );
    }
}

#[test]
fn v2_checkpoint_shell_metadata_includes_deletions_and_binary_writes() {
    let repo = TestRepo::new();
    let path = repo.path().join("binary.dat");
    fs::write(&path, [0_u8, 255, 127]).unwrap();
    let before = bash_tool::snapshot(repo.path()).unwrap();
    assert!(!bash_tool::has_changes(&before).unwrap());
    fs::remove_file(&path).unwrap();
    assert!(bash_tool::has_changes(&before).unwrap());
    let deleted = bash_tool::snapshot(repo.path()).unwrap();
    fs::write(&path, [0_u8, 255, 127]).unwrap();
    assert!(bash_tool::has_changes(&deleted).unwrap());
}

#[test]
fn v2_checkpoint_failed_file_tool_has_no_write_signal() {
    let repo = TestRepo::new();
    let input = json!({
        "session_id": "failed-session",
        "cwd": repo.path(),
        "hook_event_name": "PostToolUse",
        "tool_name": "Write",
        "tool_input": {"file_path": repo.path().join("edited.txt")},
        "tool_response": {"is_error": true}
    });
    assert!(
        resolve_preset("claude")
            .unwrap()
            .parse(&input.to_string(), "test")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn v2_checkpoint_native_agents_preserve_external_identity_without_transcripts() {
    let repo = TestRepo::new();
    let cwd = repo.path();
    let path = cwd.join("native.txt");
    let session = "external.Session/17";
    let cases = [
        (
            "claude",
            "claude",
            json!({"cwd":cwd,"session_id":session,"hook_event_name":"PostToolUse","tool_name":"Write","tool_input":{"file_path":path}}),
        ),
        (
            "codex",
            "codex",
            json!({"cwd":cwd,"thread-id":session,"hook_event_name":"PostToolUse","tool_name":"functions.apply_patch","tool_input":"*** Begin Patch\n*** Add File: native.txt\n+edit\n*** End Patch"}),
        ),
        (
            "gemini",
            "gemini",
            json!({"cwd":cwd,"session_id":session,"hook_event_name":"AfterTool","tool_name":"write_file","tool_input":{"file_path":path}}),
        ),
        (
            "continue-cli",
            "continue-cli",
            json!({"cwd":cwd,"session_id":session,"hook_event_name":"PostToolUse","tool_name":"edit","tool_input":{"file_path":path}}),
        ),
        (
            "amp",
            "amp",
            json!({"cwd":cwd,"thread_id":session,"hook_event_name":"PostToolUse","tool_name":"Edit","tool_input":{"path":path}}),
        ),
        (
            "droid",
            "droid",
            json!({"cwd":cwd,"sessionId":session,"hookEventName":"PostToolUse","toolName":"Write","toolInput":{"filePath":path}}),
        ),
        (
            "opencode",
            "opencode",
            json!({"cwd":cwd,"session_id":session,"hook_event_name":"PostToolUse","tool_name":"write","tool_input":{"filePath":path}}),
        ),
        (
            "windsurf",
            "windsurf",
            json!({"cwd":cwd,"trajectory_id":session,"agent_action_name":"post_write_code","tool_info":{"file_path":path}}),
        ),
        (
            "cursor",
            "cursor",
            json!({"workspace_roots":[cwd],"conversation_id":session,"hook_event_name":"postToolUse","tool_name":"Write","tool_input":{"file_path":path}}),
        ),
        (
            "firebender",
            "firebender",
            json!({"repo_working_dir":cwd,"completion_id":session,"hook_event_name":"postToolUse","tool_name":"Write","tool_input":{"file_path":path}}),
        ),
        (
            "pi",
            "pi",
            json!({"cwd":cwd,"session_id":session,"hook_event_name":"after_edit","tool_name":"write","edited_filepaths":[path]}),
        ),
        (
            "agent-v1",
            "codex",
            json!({"type":"ai_agent","repo_working_dir":cwd,"agent_name":"codex","conversation_id":session,"edited_filepaths":[path]}),
        ),
        (
            "github-copilot",
            "github-copilot-cli",
            json!({"cwd":cwd,"session_id":session,"hook_event_name":"PostToolUse","tool_name":"Write","tool_input":{"file_path":path}}),
        ),
        (
            "github-copilot",
            "github-copilot",
            json!({"cwd":cwd,"session_id":session,"hook_event_name":"PostToolUse","tool_name":"create_file","tool_input":{"file_path":path},"transcript_path":"/missing/github.copilot-chat/transcripts/session.jsonl"}),
        ),
        (
            "cline",
            "cline",
            json!({"hookName":"PostToolUse","clineVersion":"1","taskId":session,"workspaceRoots":[cwd],"postToolUse":{"toolName":"write_to_file","parameters":{"path":path},"success":true}}),
        ),
    ];
    for (preset, agent, input) in cases {
        let events = resolve_preset(preset)
            .unwrap()
            .parse(&input.to_string(), "test")
            .unwrap();
        assert!(
            matches!(&events[..], [ParsedHookEvent::PostFileEdit(event)]
            if event.context.external_session_id == session && event.context.agent == agent),
            "{preset} lost the native write signal: {events:?}"
        );
    }
}

#[test]
fn v2_checkpoint_read_tools_never_emit_write_signals() {
    let repo = TestRepo::new();
    for preset in ["claude", "continue-cli", "opencode"] {
        let input = json!({"cwd":repo.path(),"session_id":"read-session","hook_event_name":"PostToolUse","tool_name":"read","tool_input":{"file_path":"read.txt"}});
        let events = resolve_preset(preset)
            .unwrap()
            .parse(&input.to_string(), "test")
            .unwrap();
        assert!(events.is_empty(), "{preset} treated a read as a write");
    }
    let input = json!({"cwd":repo.path(),"trajectory_id":"read-session","agent_action_name":"post_cascade_response_with_transcript"});
    assert!(
        resolve_preset("windsurf")
            .unwrap()
            .parse(&input.to_string(), "test")
            .unwrap()
            .is_empty()
    );
}

fn claude_hook(
    repo: &TestRepo,
    session: &str,
    event: &str,
    tool: &str,
    invocation: &str,
) -> String {
    let input = json!({
        "cwd":repo.path(), "session_id":session, "hook_event_name":event,
        "tool_name":tool, "tool_use_id":invocation,
        "tool_input":{"file_path":repo.path().join("edited.txt"),"command":"test shell call"}
    });
    repo.git_ai(&["checkpoint", "claude", "--hook-input", &input.to_string()])
        .unwrap()
}

#[test]
fn v2_checkpoint_attaches_once_and_rearms_after_commit() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("edited.txt"), "first\n").unwrap();
    assert!(
        claude_hook(&repo, "session-one", "PostToolUse", "Write", "write-one")
            .contains("checkpoint_requests=1")
    );
    assert!(
        claude_hook(&repo, "session-one", "PostToolUse", "Write", "write-two")
            .contains("checkpoint_requests=0")
    );
    claude_hook(&repo, "session-two", "PostToolUse", "Edit", "write-three");
    let first = repo.stage_all_and_commit("two sessions").unwrap();
    repo.assert_commit_links(
        &first.commit_sha,
        &[("claude", "session-one"), ("claude", "session-two")],
    );

    fs::write(repo.path().join("edited.txt"), "untracked second\n").unwrap();
    let second = repo
        .stage_all_and_commit("no new session contribution")
        .unwrap();
    repo.assert_commit_links(&second.commit_sha, &[]);

    fs::write(repo.path().join("edited.txt"), "third\n").unwrap();
    assert!(
        claude_hook(&repo, "session-one", "PostToolUse", "Write", "write-four")
            .contains("checkpoint_requests=1")
    );
    let third = repo.stage_all_and_commit("session rearmed").unwrap();
    repo.assert_commit_links(&third.commit_sha, &[("claude", "session-one")]);
}

#[test]
fn v2_checkpoint_pre_edit_and_read_only_shell_do_not_attach() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("edited.txt"), "existing\n").unwrap();
    claude_hook(&repo, "read-session", "PreToolUse", "Write", "pre-edit");
    claude_hook(&repo, "read-session", "PreToolUse", "Bash", "read-shell");
    let _ = fs::read(repo.path().join("edited.txt")).unwrap();
    claude_hook(&repo, "read-session", "PostToolUse", "Bash", "read-shell");
    let commit = repo.stage_all_and_commit("only read hooks").unwrap();
    repo.assert_commit_links(&commit.commit_sha, &[]);
}

#[test]
fn v2_checkpoint_shell_links_internal_commits_without_leaking_next_window() {
    let repo = TestRepo::new();
    claude_hook(&repo, "shell-session", "PreToolUse", "Bash", "write-commit");
    fs::write(repo.path().join("edited.txt"), "first shell commit\n").unwrap();
    let first = repo.stage_all_and_commit("shell commit one").unwrap();
    repo.assert_commit_links(&first.commit_sha, &[]);
    fs::write(repo.path().join("edited.txt"), "second shell commit\n").unwrap();
    let second = repo.stage_all_and_commit("shell commit two").unwrap();
    repo.assert_commit_links(&second.commit_sha, &[]);
    claude_hook(
        &repo,
        "shell-session",
        "PostToolUse",
        "Bash",
        "write-commit",
    );
    repo.assert_commit_links(&first.commit_sha, &[("claude", "shell-session")]);
    repo.assert_commit_links(&second.commit_sha, &[("claude", "shell-session")]);

    fs::write(repo.path().join("edited.txt"), "unrelated\n").unwrap();
    let third = repo.stage_all_and_commit("next window").unwrap();
    repo.assert_commit_links(&third.commit_sha, &[]);
}

#[test]
fn v2_checkpoint_shell_deletion_is_a_contribution() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("edited.txt"), "delete me\n").unwrap();
    let first = repo.stage_all_and_commit("initial").unwrap();
    repo.assert_commit_links(&first.commit_sha, &[]);
    claude_hook(&repo, "delete-session", "PreToolUse", "Bash", "delete");
    fs::remove_file(repo.path().join("edited.txt")).unwrap();
    claude_hook(&repo, "delete-session", "PostToolUse", "Bash", "delete");
    let second = repo.stage_all_and_commit("deletion").unwrap();
    repo.assert_commit_links(&second.commit_sha, &[("claude", "delete-session")]);
}

#[test]
fn v2_checkpoint_warm_shell_does_not_capture_a_metadata_snapshot() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("edited.txt"), "agent write\n").unwrap();
    claude_hook(&repo, "warm-session", "PostToolUse", "Write", "write");
    claude_hook(&repo, "warm-session", "PreToolUse", "Bash", "warm-shell");
    repo.sync_daemon();
    let response = git_ai::daemon::send_control_request(
        &repo.daemon_control_socket_path(),
        &git_ai::daemon::ControlRequest::CheckpointInvocation {
            repo_working_dir: repo.path().to_string_lossy().into_owned(),
            session: git_ai::commit_links::store::SessionIdentity::new("claude", "warm-session")
                .unwrap(),
            invocation_id: "warm-shell".into(),
        },
    )
    .unwrap();
    assert!(response.ok);
    let captured: git_ai::commit_links::checkpoint::ShellInvocationSnapshot =
        serde_json::from_value(response.data.unwrap()).unwrap();
    assert!(captured.attached);
    assert!(
        captured.snapshot.is_none(),
        "an attached session must skip filesystem scans"
    );
    let commit = repo.stage_all_and_commit("warm shell commits").unwrap();
    repo.assert_commit_links(&commit.commit_sha, &[("claude", "warm-session")]);
    claude_hook(&repo, "warm-session", "PostToolUse", "Bash", "warm-shell");
    repo.assert_commit_links(&commit.commit_sha, &[("claude", "warm-session")]);
}

#[test]
fn v2_checkpoint_missing_shell_snapshot_does_not_infer_from_dirty_worktree() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("edited.txt"), "unknown writer\n").unwrap();
    let output = claude_hook(&repo, "missing-pre", "PostToolUse", "Bash", "unseen-shell");
    assert!(output.contains("checkpoint_requests=0"));
    let commit = repo.stage_all_and_commit("without shell evidence").unwrap();
    repo.assert_commit_links(&commit.commit_sha, &[]);
}

#[test]
fn v2_checkpoint_cline_failed_write_has_no_contribution() {
    let repo = TestRepo::new();
    let input = json!({"hookName":"PostToolUse","clineVersion":"1","taskId":"failed-cline","workspaceRoots":[repo.path()],"postToolUse":{"toolName":"write_to_file","parameters":{"path":"edited.txt"},"success":false}});
    assert!(
        resolve_preset("cline")
            .unwrap()
            .parse(&input.to_string(), "test")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn v2_checkpoint_opencode_accepts_every_native_edit_tool() {
    let repo = TestRepo::new();
    for tool in [
        "edit",
        "write",
        "patch",
        "multiedit",
        "apply_patch",
        "applypatch",
    ] {
        let input = json!({"cwd":repo.path(),"session_id":"opencode-tools","hook_event_name":"PostToolUse","tool_name":tool,"tool_input":{"file_paths":[repo.path().join("edited.txt")]}});
        let events = resolve_preset("opencode")
            .unwrap()
            .parse(&input.to_string(), "test")
            .unwrap();
        assert!(
            matches!(&events[..], [ParsedHookEvent::PostFileEdit(_)]),
            "native tool {tool} lost its write signal"
        );
    }
}

#[test]
fn v2_checkpoint_gemini_external_scratch_write_does_not_attach_cwd_repository() {
    let repo = TestRepo::new();
    let scratch = repo.test_home_path().join(".gemini/tmp/scratch.txt");
    fs::create_dir_all(scratch.parent().unwrap()).unwrap();
    fs::write(&scratch, "outside repository\n").unwrap();
    let input = json!({"cwd":repo.path(),"session_id":"scratch-only","hook_event_name":"AfterTool","tool_name":"write_file","tool_input":{"file_path":scratch}});
    let output = repo
        .git_ai_with_env(
            &["checkpoint", "gemini", "--hook-input", &input.to_string()],
            &[("GEMINI_CLI_HOME", repo.test_home_path().to_str().unwrap())],
        )
        .unwrap();
    assert!(
        output.contains("checkpoint_requests=0"),
        "external scratch write must not attach cwd: {output}"
    );
    fs::write(repo.path().join("unrelated.txt"), "untracked local edit\n").unwrap();
    let commit = repo.stage_all_and_commit("unrelated local work").unwrap();
    repo.assert_commit_links(&commit.commit_sha, &[]);
}
