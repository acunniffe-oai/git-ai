use crate::repos::test_repo::{DaemonTestScope, TestRepo};
use git_ai::mdm::agents::get_all_installers;
use git_ai::mdm::utils::update_vscode_chat_hook_settings;

#[test]
fn v2_install_dry_run_keeps_native_hooks_without_ide_extensions() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let home = repo.test_home_path();
    std::fs::create_dir_all(home.join(".vscode")).unwrap();
    let output = repo
        .git_ai_command_without_pre_sync_for_test(&["install-hooks", "--dry-run"], &[])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!home.join(".copilot/hooks/git-ai.json").exists());
    let installers = get_all_installers();
    let ids: Vec<_> = installers.iter().map(|i| i.id()).collect();
    assert!(ids.contains(&"github-copilot"));
    for retired in ["vscode", "jetbrains", "visual-studio"] {
        assert!(
            !ids.contains(&retired),
            "retired extension installer {retired}"
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("extension install"), "{stdout}");
}

#[test]
fn v2_install_chat_hooks_preserve_native_otel_without_local_session_export() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let settings = repo.path().join("settings.json");
    let before = r#"{"chat.useHooks":false,"chat.tools.terminal.autoApprove":false,"github.copilot.chat.otel.endpoint":"https://collector.example/v1/logs","github.copilot.chat.otel.dbSpanExporter.enabled":true}"#;
    std::fs::write(&settings, before).unwrap();
    let diff = update_vscode_chat_hook_settings(&settings, true)
        .unwrap()
        .unwrap();
    assert!(diff.contains("dbSpanExporter"));
    assert_eq!(std::fs::read_to_string(&settings).unwrap(), before);
    update_vscode_chat_hook_settings(&settings, false).unwrap();
    let after: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&settings).unwrap()).unwrap();
    assert_eq!(after["chat.useHooks"], true);
    assert_eq!(after["chat.tools.terminal.autoApprove"], false);
    assert_eq!(
        after["github.copilot.chat.otel.endpoint"],
        "https://collector.example/v1/logs"
    );
    assert!(
        after
            .get("github.copilot.chat.otel.dbSpanExporter.enabled")
            .is_none()
    );
}

#[test]
fn v2_install_rejects_retired_embedded_skills_option() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let result = repo
        .git_ai_command_without_pre_sync_for_test(&["install-hooks", "--dry-run", "--skills"], &[])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("no longer supported"));
    assert!(!repo.test_home_path().join(".git-ai/skills").exists());
}

#[test]
fn v2_install_dry_run_options_preserve_isolated_native_hook_files() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let settings = repo.test_home_path().join(".claude/settings.json");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    let original = r#"{"env":{"MY_SETTING":"unchanged"},"hooks":{}}"#;
    std::fs::write(&settings, original).unwrap();
    for args in [
        vec!["install-hooks", "--dry-run"],
        vec!["install-hooks", "--dry-run=true", "--verbose"],
        vec!["uninstall-hooks", "--dry-run", "-v"],
    ] {
        let output = repo
            .git_ai_command_without_pre_sync_for_test(&args, &[])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), original);
    }
}
