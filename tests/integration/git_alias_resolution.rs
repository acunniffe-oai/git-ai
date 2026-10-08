use crate::repos::test_repo::TestRepo;
use git_ai::commands::git_handlers::resolve_alias_invocation;
use git_ai::git::cli_parser::{ParsedGitInvocation, parse_git_cli_args};
use git_ai::git::find_repository_in_path;

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

fn resolve(repo: &TestRepo, argv: &[&str]) -> Option<ParsedGitInvocation> {
    let parsed = parse_git_cli_args(&args(argv));
    let git_repo =
        find_repository_in_path(repo.path().to_str().unwrap()).expect("expected to find git repo");
    resolve_alias_invocation(&parsed, &git_repo)
}

// ─── Unit-style alias resolution tests ───────────────────────────────────────

#[test]
fn alias_with_args_resolves_command_for_hooks() {
    let repo = TestRepo::new();
    repo.git(&["config", "alias.ci", "commit -v"]).unwrap();

    let resolved = resolve(&repo, &["ci", "-m", "msg"]).expect("expected alias resolution");

    assert_eq!(resolved.command.as_deref(), Some("commit"));
    assert_eq!(
        resolved.command_args,
        vec!["-v".to_string(), "-m".to_string(), "msg".to_string()]
    );
}

#[test]
fn alias_chain_resolves_to_final_command() {
    let repo = TestRepo::new();
    repo.git(&["config", "alias.lg", "log --oneline"]).unwrap();
    repo.git(&["config", "alias.l", "lg -5"]).unwrap();

    let resolved = resolve(&repo, &["l"]).expect("expected alias resolution");

    assert_eq!(resolved.command.as_deref(), Some("log"));
    assert_eq!(
        resolved.command_args,
        vec!["--oneline".to_string(), "-5".to_string()]
    );
}

#[test]
fn alias_cycle_returns_none() {
    let repo = TestRepo::new();
    repo.git(&["config", "alias.a", "b"]).unwrap();
    repo.git(&["config", "alias.b", "a"]).unwrap();

    assert!(resolve(&repo, &["a"]).is_none());
}

#[test]
fn alias_self_recursive_with_args_returns_none() {
    let repo = TestRepo::new();
    repo.git(&["config", "alias.ls", "ls -la"]).unwrap();

    assert!(resolve(&repo, &["ls"]).is_none());
}

#[test]
fn shell_alias_returns_none() {
    let repo = TestRepo::new();
    repo.git(&["config", "alias.root", "!git rev-parse --show-toplevel"])
        .unwrap();

    assert!(resolve(&repo, &["root"]).is_none());
}

#[test]
fn alias_parsing_respects_quotes() {
    let repo = TestRepo::new();
    repo.git(&[
        "config",
        "alias.pretty",
        "log --pretty='format:%h %s' --abbrev-commit",
    ])
    .unwrap();

    let resolved = resolve(&repo, &["pretty"]).expect("expected alias resolution");

    assert_eq!(resolved.command.as_deref(), Some("log"));
    assert_eq!(
        resolved.command_args,
        vec![
            "--pretty=format:%h %s".to_string(),
            "--abbrev-commit".to_string(),
        ]
    );
}

#[test]
fn non_alias_passthrough() {
    let repo = TestRepo::new();
    // No aliases configured — "commit" should pass through unchanged
    let resolved = resolve(&repo, &["commit", "-m", "msg"]).expect("expected passthrough");

    assert_eq!(resolved.command.as_deref(), Some("commit"));
    assert_eq!(
        resolved.command_args,
        vec!["-m".to_string(), "msg".to_string()]
    );
}

#[test]
fn global_args_preserved_after_alias_resolution() {
    let repo = TestRepo::new();
    repo.git(&["config", "alias.ci", "commit"]).unwrap();

    let resolved =
        resolve(&repo, &["-c", "user.name=Test", "ci", "-m", "msg"]).expect("expected resolution");

    assert_eq!(resolved.command.as_deref(), Some("commit"));
    assert!(
        resolved.global_args.contains(&"-c".to_string()),
        "global args should contain -c, got: {:?}",
        resolved.global_args
    );
    assert!(
        resolved.global_args.contains(&"user.name=Test".to_string()),
        "global args should contain user.name=Test, got: {:?}",
        resolved.global_args
    );
    assert_eq!(
        resolved.command_args,
        vec!["-m".to_string(), "msg".to_string()]
    );
}

#[test]
fn alias_to_non_hooked_command() {
    let repo = TestRepo::new();
    repo.git(&["config", "alias.s", "status"]).unwrap();

    let resolved = resolve(&repo, &["s", "--short"]).expect("expected resolution");

    assert_eq!(resolved.command.as_deref(), Some("status"));
    assert_eq!(resolved.command_args, vec!["--short".to_string()]);
}

#[test]
fn alias_with_no_extra_args() {
    let repo = TestRepo::new();
    repo.git(&["config", "alias.ci", "commit"]).unwrap();

    let resolved = resolve(&repo, &["ci"]).expect("expected resolution");

    assert_eq!(resolved.command.as_deref(), Some("commit"));
    assert!(resolved.command_args.is_empty());
}

#[test]
fn alias_with_double_quotes() {
    let repo = TestRepo::new();
    repo.git(&[
        "config",
        "alias.lg",
        r#"log "--format=%H %s" --abbrev-commit"#,
    ])
    .unwrap();

    let resolved = resolve(&repo, &["lg"]).expect("expected resolution");

    assert_eq!(resolved.command.as_deref(), Some("log"));
    assert_eq!(
        resolved.command_args,
        vec!["--format=%H %s".to_string(), "--abbrev-commit".to_string(),]
    );
}

#[test]
fn aliased_commit_triggers_session_linking() {
    let repo = TestRepo::new();
    repo.git(&["config", "alias.ci", "commit"]).unwrap();
    let path = repo.path().join("feature.rs");
    std::fs::write(&path, "fn hello() {}\n").unwrap();
    crate::test_utils::codex_checkpoint(&repo, &path, "alias-session", "PostToolUse", "write-1");
    repo.git(&["add", "-A"]).unwrap();
    repo.git(&["ci", "-m", "aliased commit"]).unwrap();
    let sha = repo.git(&["rev-parse", "HEAD"]).unwrap();
    repo.assert_commit_links(sha.trim(), &[("codex", "alias-session")]);
}

#[test]
fn aliased_commit_with_extra_flags_triggers_session_linking() {
    let repo = TestRepo::new();
    repo.git(&["config", "alias.ci", "commit -v"]).unwrap();
    let path = repo.path().join("feature.rs");
    std::fs::write(&path, "fn hello() {}\n").unwrap();
    crate::test_utils::codex_checkpoint(&repo, &path, "alias-session", "PostToolUse", "write-1");
    repo.git(&["add", "-A"]).unwrap();
    repo.git(&["ci", "-m", "aliased commit with flags"])
        .unwrap();
    let sha = repo.git(&["rev-parse", "HEAD"]).unwrap();
    repo.assert_commit_links(sha.trim(), &[("codex", "alias-session")]);
}

#[test]
fn aliased_push_preserves_local_links() {
    let (local, _upstream) = TestRepo::new_with_remote();
    local.git(&["config", "alias.p", "push"]).unwrap();
    let path = local.path().join("module.py");
    std::fs::write(&path, "print('hello')\n").unwrap();
    crate::test_utils::codex_checkpoint(&local, &path, "push-session", "PostToolUse", "write-1");
    let commit = local.stage_all_and_commit("local contribution").unwrap();
    local.assert_commit_links(&commit.commit_sha, &[("codex", "push-session")]);
    local.git(&["p", "-u", "origin", "HEAD"]).unwrap();
    local.assert_commit_links(&commit.commit_sha, &[("codex", "push-session")]);
}

crate::reuse_tests_in_worktree!(
    alias_with_args_resolves_command_for_hooks,
    alias_chain_resolves_to_final_command,
    alias_cycle_returns_none,
    alias_self_recursive_with_args_returns_none,
    shell_alias_returns_none,
    alias_parsing_respects_quotes,
    non_alias_passthrough,
    global_args_preserved_after_alias_resolution,
    alias_to_non_hooked_command,
    alias_with_no_extra_args,
    alias_with_double_quotes,
    aliased_commit_triggers_session_linking,
    aliased_commit_with_extra_flags_triggers_session_linking,
    aliased_push_preserves_local_links,
);
