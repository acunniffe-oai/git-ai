use crate::repos::test_repo::{DaemonTestScope, TestRepo};
use serde_json::{Value, json};
use std::{fs, path::PathBuf, process::Command};

const TOKEN: &str = "discovery-test-token";
const OLD_TOKEN: &str = "previous-exporter-secret";

struct Fixture {
    repo: TestRepo,
    server: mockito::ServerGuard,
    claude: PathBuf,
    codex: PathBuf,
}

impl Fixture {
    fn new(key: Option<&str>) -> Self {
        let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
        let server = mockito::Server::new();
        let claude = repo.test_home_path().join("custom-claude/settings.json");
        let codex = repo.test_home_path().join("custom-codex/config.toml");
        fs::create_dir_all(claude.parent().unwrap()).unwrap();
        fs::create_dir_all(codex.parent().unwrap()).unwrap();
        fs::create_dir_all(repo.path().join("empty-bin")).unwrap();
        fs::write(
            &claude,
            json!({
                "theme": "dark", "otelHeadersHelper": format!("echo Authorization=Bearer {OLD_TOKEN}"),
                "env": {"KEEP_ME": "yes", "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT": "http://old",
                    "OTEL_EXPORTER_OTLP_TRACES_HEADERS": OLD_TOKEN,
                    "OTEL_EXPORTER_OTLP_HEADERS": format!("Authorization=Bearer {OLD_TOKEN}")}
            })
            .to_string(),
        )
        .unwrap();
        fs::write(&codex, format!(
            "model = \"test-model\"\n[otel]\nenvironment = \"old\"\n[otel.exporter.otlp-grpc]\nendpoint = \"http://old\"\n[otel.exporter.otlp-grpc.headers]\nAuthorization = \"Bearer {OLD_TOKEN}\"\n"
        )).unwrap();
        let f = Self {
            repo,
            server,
            claude,
            codex,
        };
        let path = f.config_path();
        let mut config: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        config["api_base_url"] = json!(format!("{}/api/gitai?ignored=1#fragment", f.server.url()));
        config["api_key"] = json!(key);
        fs::write(path, config.to_string()).unwrap();
        f
    }

    fn config_path(&self) -> PathBuf {
        self.repo.test_home_path().join(".git-ai/config.json")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = self
            .repo
            .git_ai_command_without_pre_sync_for_test(&["install-hooks"], &[]);
        for key in [
            "API_BASE",
            "API_KEY",
            "GIT_AI_API_KEY",
            "GIT_AI_API_BASE_URL",
            "GIT_AI_TEST_CONFIG_PATCH",
        ] {
            command.env_remove(key);
        }
        command
            .args(args)
            .env("PATH", self.repo.path().join("empty-bin"))
            .env("CLAUDE_CONFIG_DIR", self.claude.parent().unwrap())
            .env("CODEX_HOME", self.codex.parent().unwrap())
            .env_remove("COPILOT_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .env("GIT_AI_DEBUG", "0")
            .env("NO_PROXY", "127.0.0.1,localhost");
        command
    }

    fn run(&self, args: &[&str]) -> String {
        run(self.command(args))
    }

    fn response(&self, level: &str, token: &str) -> Value {
        let content = level == "messages";
        let exporter = |signal| {
            json!({"otlp-http": {
                "endpoint": format!("{}/api/otel/codex/v1/{signal}", self.server.url()),
                "protocol": "json", "headers": {"Authorization": format!("Bearer {token}")}
            }})
        };
        json!({"version": 1, "logLevel": level, "agents": {
            "codex": {"enabled": true, "otel": {
                "environment": "production", "log_user_prompt": content,
                "log_agent_responses": content, "log_guardian_assessments": false,
                "tool_result": {"max_bytes": if content { 1048576 } else { 0 }},
                "exporter": exporter("logs"), "trace_exporter": exporter("traces")
            }},
            "claude": {"enabled": true, "env": {
                "CLAUDE_CODE_ENABLE_TELEMETRY": "1", "CLAUDE_CODE_ENHANCED_TELEMETRY_BETA": "1",
                "OTEL_EXPORTER_OTLP_PROTOCOL": "http/json", "OTEL_LOGS_EXPORTER": "otlp",
                "OTEL_METRICS_EXPORTER": "otlp", "OTEL_TRACES_EXPORTER": "otlp",
                "OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE": "delta",
                "OTEL_EXPORTER_OTLP_ENDPOINT": format!("{}/api/otel/claude-code", self.server.url()),
                "OTEL_EXPORTER_OTLP_HEADERS": format!("Authorization=Bearer {token}"),
                "OTEL_LOG_USER_PROMPTS": if content { "1" } else { "0" },
                "OTEL_LOG_ASSISTANT_RESPONSES": if content { "1" } else { "0" },
                "OTEL_LOG_TOOL_DETAILS": if content { "1" } else { "0" },
                "OTEL_LOG_TOOL_CONTENT": if content { "1" } else { "0" },
                "OTEL_LOG_RAW_API_BODIES": "0"
            }},
            "future-agent": {"enabled": true, "something": "ignored"}
        }})
    }

    fn mock(&mut self, body: Value, token: &str, count: usize) -> mockito::Mock {
        self.server
            .mock("GET", "/api/client-config")
            .match_header("authorization", format!("Bearer {token}").as_str())
            .match_header("x-api-key", mockito::Matcher::Missing)
            .expect(count)
            .with_status(200)
            .with_body(body.to_string())
            .create()
    }

    fn configs(&self) -> (Value, toml::Value) {
        (
            serde_json::from_str(&fs::read_to_string(&self.claude).unwrap()).unwrap(),
            toml::from_str(&fs::read_to_string(&self.codex).unwrap()).unwrap(),
        )
    }
}

fn run(mut command: Command) -> String {
    let output = command.output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{text}");
    text
}

#[test]
fn install_hooks_otel_applies_native_settings_and_refreshes_existing_hooks() {
    let mut f = Fixture::new(Some(TOKEN));
    for level in ["messages", "tools", "off"] {
        let response = f.response(level, TOKEN);
        let mock = f.mock(response.clone(), TOKEN, 2);
        let output = f.run(&["--verbose"]);
        assert!(!output.contains(TOKEN), "{output}");
        assert!(!output.contains(OLD_TOKEN), "{output}");
        let (claude, codex) = f.configs();
        assert_eq!(claude["theme"], "dark");
        assert_eq!(claude["env"]["KEEP_ME"], "yes");
        assert!(claude.get("otelHeadersHelper").is_none());
        assert!(
            claude["env"]
                .get("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT")
                .is_none()
        );
        assert!(
            claude["env"]
                .get("OTEL_EXPORTER_OTLP_TRACES_HEADERS")
                .is_none()
        );
        for (key, value) in response["agents"]["claude"]["env"].as_object().unwrap() {
            assert_eq!(&claude["env"][key], value);
        }
        assert!(claude["hooks"].is_object());
        assert_eq!(codex["model"].as_str(), Some("test-model"));
        let actual_otel = serde_json::to_value(&codex["otel"]).unwrap();
        assert_eq!(actual_otel, response["agents"]["codex"]["otel"]);
        let before = (
            fs::metadata(&f.claude).unwrap().modified().unwrap(),
            fs::metadata(&f.codex).unwrap().modified().unwrap(),
        );
        f.run(&[]);
        let after = (
            fs::metadata(&f.claude).unwrap().modified().unwrap(),
            fs::metadata(&f.codex).unwrap().modified().unwrap(),
        );
        assert_eq!(
            before, after,
            "unchanged configuration should not be rewritten"
        );
        mock.assert();
        mock.remove();
    }
}

#[test]
fn install_hooks_otel_without_token_never_fetches_or_changes_otel() {
    for key in [None, Some(""), Some("   ")] {
        let mut f = Fixture::new(key);
        let mock = f
            .server
            .mock("GET", "/api/client-config")
            .expect(0)
            .create();
        let before = f.configs();
        let credentials = f.repo.test_home_path().join(".git-ai/internal/credentials");
        fs::create_dir_all(credentials.parent().unwrap()).unwrap();
        fs::write(credentials, json!({
            "access_token": "oauth-is-not-a-telemetry-key", "refresh_token": "oauth-refresh-token",
            "access_token_expires_at": 4102444800_i64, "refresh_token_expires_at": 4102444800_i64
        }).to_string()).unwrap();
        f.run(&[]);
        mock.assert();
        let after = f.configs();
        assert_eq!(after.0["env"], before.0["env"]);
        assert_eq!(after.1["otel"], before.1["otel"]);
    }
}

#[test]
fn install_hooks_otel_dry_run_fetches_with_install_args_without_writes_or_secrets() {
    let mut f = Fixture::new(None);
    let mock = f.mock(f.response("messages", TOKEN), TOKEN, 1);
    let before = [
        fs::read(&f.claude).unwrap(),
        fs::read(&f.codex).unwrap(),
        fs::read(f.config_path()).unwrap(),
    ];
    let output = f.run(&[
        "--dry-run",
        "--verbose",
        "--api-key",
        TOKEN,
        "--api-base",
        &f.server.url(),
    ]);
    mock.assert();
    assert!(output.contains("OTEL"), "{output}");
    assert!(!output.contains(TOKEN), "{output}");
    assert!(!output.contains(OLD_TOKEN), "{output}");
    let after = [
        fs::read(&f.claude).unwrap(),
        fs::read(&f.codex).unwrap(),
        fs::read(f.config_path()).unwrap(),
    ];
    assert_eq!(before, after);
}

#[test]
fn install_hooks_otel_environment_key_wins_over_installer_and_saved_key() {
    let mut f = Fixture::new(Some("saved-key"));
    let mock = f.mock(f.response("off", TOKEN), TOKEN, 1);
    let mut command = f.command(&["--api-key", "installer-key"]);
    command.env("GIT_AI_API_KEY", TOKEN);
    run(command);
    mock.assert();
    let config: Value =
        serde_json::from_str(&fs::read_to_string(f.config_path()).unwrap()).unwrap();
    assert_eq!(config["api_key"], "installer-key");
}

#[test]
fn install_hooks_positional_token_and_api_base_are_saved_in_either_order() {
    for base_first in [false, true] {
        let mut f = Fixture::new(None);
        let base = f.server.url();
        let mock = f.mock(f.response("off", TOKEN), TOKEN, 1);
        let args = if base_first {
            vec![base.as_str(), TOKEN]
        } else {
            vec![TOKEN, base.as_str()]
        };

        let output = f.run(&args);

        mock.assert();
        assert!(!output.contains(TOKEN), "{output}");
        let config: Value =
            serde_json::from_str(&fs::read_to_string(f.config_path()).unwrap()).unwrap();
        assert_eq!(config["api_key"], TOKEN);
        assert_eq!(config["api_base_url"], base);
    }
}

#[test]
fn install_hooks_accepts_each_positional_credential_on_its_own() {
    let mut token_fixture = Fixture::new(None);
    let token_mock = token_fixture.mock(token_fixture.response("off", TOKEN), TOKEN, 1);
    token_fixture.run(&[TOKEN]);
    token_mock.assert();
    let token_config: Value =
        serde_json::from_str(&fs::read_to_string(token_fixture.config_path()).unwrap()).unwrap();
    assert_eq!(token_config["api_key"], TOKEN);

    let mut base_fixture = Fixture::new(Some(TOKEN));
    let base = base_fixture.server.url();
    let base_mock = base_fixture.mock(base_fixture.response("off", TOKEN), TOKEN, 1);
    base_fixture.run(&[&base]);
    base_mock.assert();
    let base_config: Value =
        serde_json::from_str(&fs::read_to_string(base_fixture.config_path()).unwrap()).unwrap();
    assert_eq!(base_config["api_base_url"], base);
    assert_eq!(base_config["api_key"], TOKEN);
}

#[test]
fn install_hooks_otel_failures_warn_and_preserve_otel_while_installing_hooks() {
    for (status, body) in [
        (401, "private-response"),
        (403, "forbidden"),
        (404, "missing"),
        (503, "unavailable"),
        (200, "not-json"),
        (200, r#"{"version":2,"logLevel":"off","agents":{}}"#),
    ] {
        let mut f = Fixture::new(Some(TOKEN));
        let mock = f
            .server
            .mock("GET", "/api/client-config")
            .with_status(status)
            .with_body(body)
            .expect(1)
            .create();
        let before = f.configs();
        let output = f.run(&[]);
        mock.assert();
        assert!(
            output.contains("Warning") && output.contains("OTEL"),
            "{output}"
        );
        assert!(!output.contains("private-response"));
        let after = f.configs();
        assert_eq!(after.0["env"], before.0["env"]);
        assert_eq!(after.1["otel"], before.1["otel"]);
        assert!(after.0["hooks"].is_object());
    }
}

#[test]
fn install_hooks_otel_disabled_or_missing_agents_are_left_alone() {
    let mut f = Fixture::new(Some(TOKEN));
    let mock = f.mock(
        json!({"version":1,"logLevel":"off","agents":{
            "codex":{"enabled":false},"unknown":{"enabled":true}
        }}),
        TOKEN,
        1,
    );
    let before = f.configs();
    f.run(&[]);
    mock.assert();
    let after = f.configs();
    assert_eq!(after.0["env"], before.0["env"]);
    assert_eq!(after.1["otel"], before.1["otel"]);
}

#[test]
fn install_hooks_otel_invalid_agent_does_not_block_other_agent() {
    let mut f = Fixture::new(Some(TOKEN));
    let mut response = f.response("messages", TOKEN);
    response["agents"]["claude"]["env"] = json!({"OTEL_LOG_USER_PROMPTS": true});
    let mock = f.mock(response.clone(), TOKEN, 1);
    let before = f.configs();
    let output = f.run(&[]);
    mock.assert();
    assert!(output.contains("Warning"), "{output}");
    let after = f.configs();
    assert_eq!(after.0["env"], before.0["env"]);
    assert_eq!(
        serde_json::to_value(&after.1["otel"]).unwrap(),
        response["agents"]["codex"]["otel"]
    );
}

#[test]
fn install_hooks_otel_absent_agents_do_not_create_config_files() {
    let mut f = Fixture::new(Some(TOKEN));
    fs::remove_dir_all(f.claude.parent().unwrap()).unwrap();
    fs::remove_dir_all(f.codex.parent().unwrap()).unwrap();
    let mock = f.mock(f.response("messages", TOKEN), TOKEN, 1);
    f.run(&[]);
    mock.assert();
    assert!(!f.claude.exists());
    assert!(!f.codex.exists());
}

#[cfg(unix)]
#[test]
fn install_hooks_otel_preserves_symlinks_and_uses_private_permissions() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let mut f = Fixture::new(Some(TOKEN));
    let target = f.repo.path().join("claude-settings.json");
    fs::rename(&f.claude, &target).unwrap();
    symlink(&target, &f.claude).unwrap();
    let mock = f.mock(f.response("tools", TOKEN), TOKEN, 2);
    f.run(&[]);
    // Force another hook edit after credentials are present.
    let mut config = f.configs().0;
    config.as_object_mut().unwrap().remove("hooks");
    fs::write(&target, config.to_string()).unwrap();
    let output = f.run(&["--verbose"]);
    mock.assert();
    assert!(!output.contains(TOKEN), "{output}");
    assert!(f.claude.is_symlink());
    for path in [&target, &f.codex] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn install_hooks_otel_installer_credentials_apply_immediately_and_rotate() {
    let mut f = Fixture::new(None);
    for (token, use_environment) in [(TOKEN, true), ("rotated-discovery-key", false)] {
        let mock = f.mock(f.response("messages", token), token, 1);
        let output = if use_environment {
            let mut command = f.command(&["--verbose"]);
            command
                .env("API_KEY", token)
                .env("API_BASE", f.server.url());
            run(command)
        } else {
            f.run(&[
                "--verbose",
                "--api-key",
                token,
                "--api-base",
                &f.server.url(),
            ])
        };
        mock.assert();
        mock.remove();
        assert!(!output.contains(token), "{output}");
        assert!(!output.contains(TOKEN), "{output}");
        let (claude, codex) = f.configs();
        assert_eq!(
            claude["env"]["OTEL_EXPORTER_OTLP_HEADERS"],
            format!("Authorization=Bearer {token}")
        );
        assert_eq!(
            codex["otel"]["exporter"]["otlp-http"]["headers"]["Authorization"].as_str(),
            Some(format!("Bearer {token}").as_str())
        );
        let config: Value =
            serde_json::from_str(&fs::read_to_string(f.config_path()).unwrap()).unwrap();
        assert_eq!(config["api_key"], token);
    }
}

#[test]
fn install_hooks_otel_dry_run_respects_environment_key_and_base_override() {
    let mut f = Fixture::new(Some("saved-key"));
    let mock = f.mock(f.response("tools", TOKEN), TOKEN, 1);
    let before = fs::read(f.config_path()).unwrap();
    let mut command = f.command(&[
        "--dry-run",
        "--api-key",
        "installer-key",
        "--api-base",
        &f.server.url(),
    ]);
    command
        .env("GIT_AI_API_KEY", TOKEN)
        .env("GIT_AI_API_BASE_URL", "http://invalid.invalid");
    run(command);
    mock.assert();
    assert_eq!(before, fs::read(f.config_path()).unwrap());
}

#[test]
fn install_hooks_otel_does_not_follow_redirects_or_retry() {
    let mut f = Fixture::new(Some(TOKEN));
    let redirected = f.server.mock("GET", "/unexpected").expect(0).create();
    let mock = f
        .server
        .mock("GET", "/api/client-config")
        .with_status(302)
        .with_header("location", &format!("{}/unexpected", f.server.url()))
        .expect(1)
        .create();
    let before = f.configs();
    assert!(f.run(&[]).contains("Warning"));
    mock.assert();
    redirected.assert();
    assert_eq!(before.1["otel"], f.configs().1["otel"]);
}

#[test]
fn install_hooks_otel_timeout_warns_and_preserves_configuration() {
    let mut f = Fixture::new(Some(TOKEN));
    let mock = f
        .server
        .mock("GET", "/api/client-config")
        .with_status(200)
        .with_chunked_body(|writer| {
            std::thread::sleep(std::time::Duration::from_secs(6));
            writer.write_all(b"{}").ok();
            Ok(())
        })
        .expect(1)
        .create();
    let before = f.configs();
    let started = std::time::Instant::now();
    assert!(f.run(&[]).contains("Warning"));
    assert!(started.elapsed() < std::time::Duration::from_secs(12));
    mock.assert();
    assert_eq!(before.1["otel"], f.configs().1["otel"]);
}

#[test]
fn install_hooks_otel_keeps_hooks_json_and_unrelated_otel_fields() {
    let mut f = Fixture::new(Some(TOKEN));
    let mut config: Value =
        serde_json::from_str(&fs::read_to_string(f.config_path()).unwrap()).unwrap();
    config["codex_hooks_format"] = json!("hooks_json");
    fs::write(f.config_path(), config.to_string()).unwrap();
    let mut codex = f.configs().1;
    codex["otel"]
        .as_table_mut()
        .unwrap()
        .insert("custom_option".into(), toml::Value::Boolean(true));
    fs::write(&f.codex, toml::to_string_pretty(&codex).unwrap()).unwrap();
    let mock = f.mock(f.response("messages", TOKEN), TOKEN, 2);
    f.run(&[]);
    let hooks_path = f.codex.parent().unwrap().join("hooks.json");
    let hooks = fs::read(&hooks_path).unwrap();
    f.run(&[]);
    mock.assert();
    assert_eq!(hooks, fs::read(hooks_path).unwrap());
    assert_eq!(f.configs().1["otel"]["custom_option"].as_bool(), Some(true));
}

#[test]
fn install_hooks_otel_malformed_local_config_does_not_leak_or_block_other_agent() {
    let mut f = Fixture::new(Some(TOKEN));
    let broken =
        format!("[otel.exporter.otlp-http.headers]\nAuthorization = '{OLD_TOKEN}' garbage\n");
    fs::write(&f.codex, &broken).unwrap();
    let mock = f.mock(f.response("messages", TOKEN), TOKEN, 1);
    let output = f.run(&["--verbose"]);
    mock.assert();
    assert!(!output.contains(OLD_TOKEN), "{output}");
    assert_eq!(fs::read_to_string(&f.codex).unwrap(), broken);
    let claude: Value = serde_json::from_str(&fs::read_to_string(&f.claude).unwrap()).unwrap();
    assert_eq!(claude["env"]["OTEL_LOG_USER_PROMPTS"], "1");
}

#[test]
fn install_hooks_otel_write_failure_leaves_agent_unchanged_and_continues() {
    let mut f = Fixture::new(None);
    f.run(&[]); // Hooks already installed; fail only the optional OTEL write.
    let before = fs::read(&f.claude).unwrap();
    fs::create_dir(f.claude.with_extension("tmp")).unwrap();
    let mock = f.mock(f.response("messages", TOKEN), TOKEN, 1);
    let output = f.run(&["--api-key", TOKEN]);
    mock.assert();
    assert!(output.contains("could not configure OTEL"), "{output}");
    assert_eq!(before, fs::read(&f.claude).unwrap());
    assert_eq!(
        f.configs().1["otel"]["log_user_prompt"].as_bool(),
        Some(true)
    );
}

impl Fixture {
    fn copilot_paths(&self) -> (PathBuf, PathBuf) {
        let home = self.repo.test_home_path();
        #[cfg(target_os = "macos")]
        let settings = home.join("Library/Application Support/Code/User/settings.json");
        #[cfg(all(unix, not(target_os = "macos")))]
        let settings = home.join(".config/Code/User/settings.json");
        #[cfg(windows)]
        let settings = home.join("AppData/Roaming/Code/User/settings.json");
        (settings, home.join(".zshrc"))
    }
    fn prepare_copilot(&self) -> (PathBuf, PathBuf) {
        let (settings, profile) = self.copilot_paths();
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        fs::create_dir_all(self.repo.test_home_path().join(".copilot")).unwrap();
        fs::write(
            self.repo.test_home_path().join(".copilot/settings.json"),
            "{}",
        )
        .unwrap();
        fs::write(&settings, format!("{{\n// keep my comment\n\"editor.fontSize\": 17,\n\"github.copilot.chat.otel.headers\": {{\"X-Key\": \"{OLD_TOKEN}\"}},\n}}\n")).unwrap();
        fs::write(&profile, "# keep my shell settings\n").unwrap();
        (settings, profile)
    }
    fn copilot_response(&self, level: &str, token: &str) -> Value {
        let mut response = self.response(level, token);
        response["agents"]["copilot"] = json!({"enabled":true,"cli":{"env":{
            "COPILOT_OTEL_ENABLED":"true", "COPILOT_OTEL_EXPORTER_TYPE":"otlp-http",
            "OTEL_EXPORTER_OTLP_PROTOCOL":"http/json",
            "OTEL_EXPORTER_OTLP_ENDPOINT":format!("{}/api/otel/github-copilot-cli",self.server.url()),
            "OTEL_EXPORTER_OTLP_HEADERS":format!("Authorization=Bearer {token}"),
            "OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT":(level=="messages").to_string(),
            "UNTRUSTED_SHELL":"$(touch should-not-exist)"
        }},"vscode":{"settings":{
            "github.copilot.chat.otel.enabled":true,
            "github.copilot.chat.otel.exporterType":"otlp-http",
            "github.copilot.chat.otel.protocol":"http/protobuf",
            "github.copilot.chat.otel.otlpEndpoint":format!("{}/api/otel/github-copilot",self.server.url()),
            "github.copilot.chat.otel.headers":{"Authorization":format!("Bearer {token}")},
            "github.copilot.chat.otel.captureContent":level=="messages",
            "tasks.runOn":"untrusted"
        }}});
        response
    }
}

#[test]
fn install_hooks_otel_copilot_native_settings_refresh_and_redact_jsonc() {
    let mut f = Fixture::new(Some(TOKEN));
    let (settings, profile) = f.prepare_copilot();
    for level in ["messages", "tools", "off"] {
        let mock = f.mock(f.copilot_response(level, TOKEN), TOKEN, 2);
        let output = f.run(&["--verbose"]);
        assert!(!output.contains(TOKEN), "{output}");
        assert!(!output.contains(OLD_TOKEN), "{output}");
        let text = fs::read_to_string(&settings).unwrap();
        assert!(text.contains("// keep my comment"));
        let value: Value = jsonc_parser::parse_to_serde_value(&text, &Default::default()).unwrap();
        assert_eq!(value["editor.fontSize"], 17);
        assert_eq!(value["github.copilot.chat.otel.protocol"], "http/protobuf");
        assert_eq!(
            value["github.copilot.chat.otel.captureContent"],
            level == "messages"
        );
        assert_eq!(
            value["github.copilot.chat.otel.headers"]["Authorization"],
            format!("Bearer {TOKEN}")
        );
        assert!(value.get("tasks.runOn").is_none());
        let shell = fs::read_to_string(&profile).unwrap();
        assert!(shell.contains("# keep my shell settings"));
        assert!(shell.contains("git-ai copilot telemetry"), "{shell}");
        assert!(!shell.contains(TOKEN));
        let before = fs::metadata(&settings).unwrap().modified().unwrap();
        f.run(&[]);
        assert_eq!(before, fs::metadata(&settings).unwrap().modified().unwrap());
        assert_eq!(shell, fs::read_to_string(&profile).unwrap());
        mock.assert();
        mock.remove();
    }
}

#[test]
fn install_hooks_otel_copilot_dry_run_does_not_write_credentials_or_profiles() {
    let mut f = Fixture::new(Some(TOKEN));
    let (settings, profile) = f.prepare_copilot();
    let before = (fs::read(&settings).unwrap(), fs::read(&profile).unwrap());
    let mock = f.mock(f.copilot_response("messages", TOKEN), TOKEN, 1);
    let output = f.run(&["--dry-run", "--verbose"]);
    assert!(!output.contains(TOKEN), "{output}");
    assert!(!output.contains(OLD_TOKEN), "{output}");
    assert_eq!(
        before,
        (fs::read(settings).unwrap(), fs::read(profile).unwrap())
    );
    assert!(!f.repo.test_home_path().join(".git-ai/copilot").exists());
    mock.assert();
}

#[cfg(unix)]
#[test]
fn install_hooks_otel_copilot_shell_scopes_env_preserves_arguments_and_exit_status() {
    use std::os::unix::fs::PermissionsExt;
    let mut f = Fixture::new(Some(TOKEN));
    let (_, profile) = f.prepare_copilot();
    let mock = f.mock(f.copilot_response("off", TOKEN), TOKEN, 1);
    f.run(&[]);
    mock.assert();
    let bin = f.repo.path().join("empty-bin");
    let executable = bin.join("copilot");
    fs::write(&executable, "#!/bin/sh\nprintf '%s\\n' \"$OTEL_EXPORTER_OTLP_ENDPOINT\" \"$OTEL_EXPORTER_OTLP_HEADERS\" \"$OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT\" \"$OTEL_EXPORTER_OTLP_TRACES_ENDPOINT\" \"$@\"\nexit 7\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    for shell in ["/bin/bash", "/bin/zsh"] {
        if !std::path::Path::new(shell).exists() {
            continue;
        }
        let out = Command::new(shell).arg("-c").arg(". \"$1\"; copilot 'one two' '$(false)'; result=$?; printf 'status=%s parent=%s override=%s\\n' \"$result\" \"$OTEL_EXPORTER_OTLP_ENDPOINT\" \"$OTEL_EXPORTER_OTLP_TRACES_ENDPOINT\"")
            .arg("test").arg(&profile).env("PATH", &bin).env("HOME",f.repo.test_home_path())
            .env("OTEL_EXPORTER_OTLP_ENDPOINT","parent-value").env("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT","parent-override").output().unwrap();
        let stdout = String::from_utf8(out.stdout).unwrap();
        assert!(
            out.status.success(),
            "{stdout} {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(stdout.contains("/api/otel/github-copilot-cli"), "{stdout}");
        assert!(
            stdout.contains(&format!("Authorization=Bearer {TOKEN}")),
            "{stdout}"
        );
        assert!(stdout.contains("false\n\none two\n$(false)\n"), "{stdout}");
        assert!(
            stdout.contains("status=7 parent=parent-value override=parent-override"),
            "{stdout}"
        );
    }
    let env_path = f.repo.test_home_path().join(".git-ai/copilot/env.sh");
    assert_eq!(
        fs::metadata(env_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn install_hooks_otel_copilot_preserves_unmanaged_shell_function() {
    let mut f = Fixture::new(Some(TOKEN));
    let (_, profile) = f.prepare_copilot();
    let original = "copilot() { echo custom; }\n";
    fs::write(&profile, original).unwrap();
    let mock = f.mock(f.copilot_response("off", TOKEN), TOKEN, 1);
    let output = f.run(&[]);
    assert!(output.contains("Warning"), "{output}");
    assert_eq!(fs::read_to_string(profile).unwrap(), original);
    mock.assert();
}

#[test]
fn install_hooks_otel_copilot_cli_absent_does_not_modify_shell_profile() {
    let mut f = Fixture::new(Some(TOKEN));
    let (_, profile) = f.prepare_copilot();
    fs::remove_file(f.repo.test_home_path().join(".copilot/settings.json")).unwrap();
    let before = fs::read(&profile).unwrap();
    let mock = f.mock(f.copilot_response("off", TOKEN), TOKEN, 1);
    f.run(&[]);
    mock.assert();
    assert_eq!(before, fs::read(profile).unwrap());
    assert!(!f.repo.test_home_path().join(".git-ai/copilot").exists());
}

#[test]
fn install_hooks_otel_copilot_rotation_and_custom_home_preserve_private_files() {
    let mut f = Fixture::new(Some(TOKEN));
    let (settings, profile) = f.prepare_copilot();
    let custom = f.repo.test_home_path().join("copilot home 'quoted'");
    fs::rename(f.repo.test_home_path().join(".copilot"), &custom).unwrap();
    for token in [TOKEN, "rotated-copilot-key"] {
        let mock = f.mock(f.copilot_response("messages", token), token, 1);
        let mut command = f.command(&["--api-key", token, "--verbose"]);
        command.env("COPILOT_HOME", &custom);
        let output = run(command);
        mock.assert();
        mock.remove();
        assert!(!output.contains(token));
        assert!(!output.contains(TOKEN));
        assert!(!output.contains(OLD_TOKEN));
        assert!(custom.join("hooks/git-ai.json").is_file());
        let value: Value = jsonc_parser::parse_to_serde_value(
            &fs::read_to_string(&settings).unwrap(),
            &Default::default(),
        )
        .unwrap();
        assert_eq!(
            value["github.copilot.chat.otel.headers"]["Authorization"],
            format!("Bearer {token}")
        );
        #[cfg(unix)]
        {
            let env =
                fs::read_to_string(f.repo.test_home_path().join(".git-ai/copilot/env.sh")).unwrap();
            assert!(env.contains(token));
            assert!(!env.contains("UNTRUSTED_SHELL"));
            assert!(!fs::read_to_string(&profile).unwrap().contains(token));
        }
    }
}
