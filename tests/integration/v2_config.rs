use crate::repos::test_repo::TestRepo;

#[test]
fn v2_config_removes_retired_keys_and_rejects_legacy_flags() {
    let repo = TestRepo::new();
    let all: serde_json::Value = serde_json::from_str(&repo.git_ai(&["config"]).unwrap()).unwrap();
    for key in [
        "prompt_storage",
        "notes_backend",
        "author",
        "custom_attributes",
        "git_ai_hooks",
        "transcript_streaming_lookback_days",
        "max_checkpoint_file_size_bytes",
        "untraced_fixup_ignored_paths",
    ] {
        assert!(all.get(key).is_none(), "retired key {key} still visible");
        assert!(
            repo.git_ai(&["config", "set", key, "true"]).is_err(),
            "accepted {key}"
        );
    }
    for flag in [
        "lite_mode",
        "transcript_streaming",
        "transcript_sweep",
        "token_usage_metrics",
        "rewrite_metrics_events",
        "untraced_commit_fixup",
        "bash_checkpoints_v2",
        "checkpoint_debug_log",
    ] {
        assert!(all["feature_flags"].get(flag).is_none());
        assert!(
            repo.git_ai(&["config", "set", &format!("feature_flags.{flag}"), "true"])
                .is_err()
        );
    }
    assert!(all["feature_flags"]["auth_keyring"].is_boolean());
    assert!(all["feature_flags"]["daemon_log_upload"].is_boolean());
}

#[test]
fn v2_config_ignores_legacy_json_and_env_overrides_current_file_settings() {
    let repo = TestRepo::new();
    let path = repo.test_home_path().join(".git-ai/config.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    value["api_base_url"] = serde_json::json!("https://file.example");
    value["prompt_storage"] = serde_json::json!({"legacy":"ignored"});
    value["feature_flags"] =
        serde_json::json!({"daemon_log_upload":false,"transcript_streaming":"ignored"});
    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    let output = repo
        .git_ai_command_without_pre_sync_for_test(
            &["config"],
            &[
                ("GIT_AI_API_BASE_URL", "https://env.example"),
                ("GIT_AI_DAEMON_LOG_UPLOAD", "true"),
            ],
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let all: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(all["api_base_url"], "https://env.example");
    assert_eq!(all["feature_flags"]["daemon_log_upload"], true);
    assert!(all.get("prompt_storage").is_none());
}

#[test]
fn v2_whoami_reports_link_delivery_without_opening_a_metrics_database() {
    let repo = TestRepo::new();
    let output = repo
        .git_ai_command_without_pre_sync_for_test(
            &["whoami"],
            &[("GIT_AI_API_KEY", "isolated-test-key")],
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("Commit-link delivery: on"));
    assert!(!text.contains("Local metrics database"));
    assert!(!text.contains("isolated-test-key"));
}
