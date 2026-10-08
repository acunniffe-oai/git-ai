//! Configuration reads and writes use an isolated TestRepo home.
use crate::repos::test_repo::TestRepo;
use serde_json::Value;

fn run_config(repo: &TestRepo, args: &[&str]) -> std::process::Output {
    repo.git_ai_command_without_pre_sync_for_test(args, &[])
        .output()
        .unwrap_or_else(|e| panic!("git-ai {args:?} failed to run: {e}"))
}

fn get_json(repo: &TestRepo, key: &str) -> Value {
    let out = repo
        .git_ai(&["config", key])
        .unwrap_or_else(|e| panic!("config get {key} failed: {e}"));
    serde_json::from_str(out.trim())
        .unwrap_or_else(|e| panic!("config get {key} returned non-JSON {out:?}: {e}"))
}

#[test]
fn test_config_set_and_get_normal_output_uses_stdout() {
    let repo = TestRepo::new();

    for args in [
        ["config", "set", "allow_superuser", "true"].as_slice(),
        ["config", "allow_superuser"].as_slice(),
        ["config", "unset", "allow_superuser"].as_slice(),
    ] {
        let output = run_config(&repo, args);
        assert!(
            output.status.success(),
            "git-ai {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !output.stdout.is_empty(),
            "git-ai {args:?} should write normal output to stdout"
        );
        assert!(
            output.stderr.is_empty(),
            "git-ai {args:?} wrote normal output to stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn test_config_allow_superuser_set_get_unset() {
    let repo = TestRepo::new();

    // Default is false.
    assert_eq!(get_json(&repo, "allow_superuser"), Value::Bool(false));

    repo.git_ai(&["config", "set", "allow_superuser", "true"])
        .expect("set allow_superuser");
    assert_eq!(get_json(&repo, "allow_superuser"), Value::Bool(true));

    repo.git_ai(&["config", "unset", "allow_superuser"])
        .expect("unset allow_superuser");
    assert_eq!(get_json(&repo, "allow_superuser"), Value::Bool(false));
}

#[test]
fn test_config_daemon_memory_limit_set_get_unset() {
    let repo = TestRepo::new();

    assert_eq!(
        get_json(&repo, "daemon_memory_limit_mb"),
        Value::Null,
        "daemon memory monitoring should be disabled by default"
    );

    repo.git_ai(&["config", "set", "daemon_memory_limit_mb", "1024"])
        .expect("set daemon_memory_limit_mb");
    assert_eq!(
        get_json(&repo, "daemon_memory_limit_mb"),
        Value::Number(1024.into())
    );

    assert!(
        repo.git_ai(&["config", "set", "daemon_memory_limit_mb", "0"])
            .is_err(),
        "zero must be rejected; unset disables the limit"
    );
    assert!(
        repo.git_ai(&["config", "set", "daemon_memory_limit_mb", "-1"])
            .is_err(),
        "negative limits must be rejected"
    );

    repo.git_ai(&["config", "unset", "daemon_memory_limit_mb"])
        .expect("unset daemon_memory_limit_mb");
    assert_eq!(get_json(&repo, "daemon_memory_limit_mb"), Value::Null);
}

#[test]
fn test_config_show_all_includes_new_keys() {
    let repo = TestRepo::new();
    let out = repo.git_ai(&["config"]).expect("show all config");
    let value: Value =
        serde_json::from_str(out.trim()).expect("config show-all should emit valid JSON");

    assert!(value.get("allow_superuser").is_some());
    assert!(value.get("daemon_memory_limit_mb").is_some());
}
