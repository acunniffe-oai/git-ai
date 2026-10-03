//! Install native Copilot telemetry without changing the user's launch command.
use crate::error::GitAiError;
use crate::mdm::agent_otel::generate_config_diff;
use crate::mdm::utils::{
    home_dir, settings_paths_for_products, should_process_settings_target, write_private_atomic,
};
use jsonc_parser::cst::{CstInputValue, CstRootNode};
use serde_json::Value;
use std::{collections::BTreeMap, fs, path::PathBuf};

const ENV_KEYS: &[&str] = &[
    "COPILOT_OTEL_ENABLED",
    "COPILOT_OTEL_EXPORTER_TYPE",
    "OTEL_EXPORTER_OTLP_ENDPOINT",
    "OTEL_EXPORTER_OTLP_PROTOCOL",
    "OTEL_EXPORTER_OTLP_HEADERS",
    "OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT",
];
const SETTINGS: &[(&str, bool)] = &[
    ("enabled", true),
    ("exporterType", false),
    ("protocol", false),
    ("otlpEndpoint", false),
    ("captureContent", true),
];
const BEGIN: &str = "# >>> git-ai copilot telemetry >>>";
const END: &str = "# <<< git-ai copilot telemetry <<<";
fn error(message: &str) -> GitAiError {
    GitAiError::Generic(message.into())
}

pub(crate) fn copilot_home() -> PathBuf {
    std::env::var_os("COPILOT_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".copilot"))
}
pub(crate) fn cli_present() -> bool {
    ["settings.json", "config.json", "session-state"]
        .iter()
        .any(|name| copilot_home().join(name).exists())
        || std::env::var_os("PATH").is_some_and(|paths| {
            std::env::split_paths(&paths).any(|p| {
                p.join(if cfg!(windows) {
                    "copilot.exe"
                } else {
                    "copilot"
                })
                .is_file()
                    || cfg!(windows) && p.join("copilot.cmd").is_file()
            })
        })
}
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
fn fish_quote(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}
fn ps_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}
fn overrides() -> Vec<String> {
    let mut keys = vec![
        "COPILOT_OTEL_FILE_EXPORTER_PATH".into(),
        "OTEL_EXPORTER_OTLP_INSECURE".into(),
    ];
    for signal in ["TRACES", "LOGS", "METRICS"] {
        keys.push(format!("OTEL_{signal}_EXPORTER"));
        for setting in ["ENDPOINT", "PROTOCOL", "HEADERS"] {
            keys.push(format!("OTEL_EXPORTER_OTLP_{signal}_{setting}"));
        }
    }
    keys
}
fn input(value: &Value) -> CstInputValue {
    match value {
        Value::Bool(v) => CstInputValue::Bool(*v),
        Value::String(v) => CstInputValue::String(v.clone()),
        Value::Object(v) => {
            CstInputValue::Object(v.iter().map(|(k, v)| (k.clone(), input(v))).collect())
        }
        _ => CstInputValue::Null,
    }
}

fn vscode(agent: &Value, dry_run: bool) -> Result<Vec<String>, GitAiError> {
    let value = agent
        .pointer("/vscode/settings")
        .and_then(Value::as_object)
        .ok_or_else(|| error("invalid Copilot VS Code settings"))?;
    let mut settings = BTreeMap::new();
    for (suffix, boolean) in SETTINGS {
        let key = format!("github.copilot.chat.otel.{suffix}");
        let field = value
            .get(&key)
            .filter(|v| {
                if *boolean {
                    v.is_boolean()
                } else {
                    v.is_string()
                }
            })
            .ok_or_else(|| error("missing or invalid Copilot VS Code setting"))?;
        settings.insert(key, field.clone());
    }
    let headers = value
        .get("github.copilot.chat.otel.headers")
        .and_then(Value::as_object)
        .filter(|h| h.values().all(Value::is_string))
        .ok_or_else(|| error("invalid Copilot VS Code headers"))?;
    settings.insert(
        "github.copilot.chat.otel.headers".into(),
        Value::Object(headers.clone()),
    );
    let mut diffs = Vec::new();
    for path in settings_paths_for_products(&["Code", "Code - Insiders"])
        .into_iter()
        .filter(|p| should_process_settings_target(p))
    {
        let old = if path.exists() {
            fs::read_to_string(&path)?
        } else {
            String::new()
        };
        let text = if old.trim().is_empty() { "{}" } else { &old };
        let parsed = jsonc_parser::parse_to_serde_value::<Option<Value>>(text, &Default::default())
            .map_err(|_| error("could not parse Copilot VS Code settings"))?
            .filter(Value::is_object)
            .ok_or_else(|| error("Copilot VS Code settings must be an object"))?;
        let root = CstRootNode::parse(text, &Default::default())
            .map_err(|_| error("could not parse Copilot VS Code settings"))?;
        let object = root.object_value_or_set();
        let mut changed = false;
        for (key, value) in &settings {
            if parsed.get(key) == Some(value) {
                continue;
            }
            changed = true;
            if let Some(prop) = object.get(key) {
                prop.set_value(input(value));
            } else {
                object.append(key, input(value));
            }
        }
        if changed {
            let new = root.to_string();
            diffs.push(generate_config_diff(&path, &old, &new));
            if !dry_run {
                write_private_atomic(&path, new.as_bytes())?;
            }
        }
    }
    Ok(diffs)
}

#[derive(Clone, Copy)]
enum Shell {
    Posix,
    Fish,
    Pwsh,
}
fn profiles() -> Vec<(PathBuf, Shell)> {
    let home = home_dir();
    let mut candidates = Vec::new();
    #[cfg(unix)]
    {
        for name in [".zshrc", ".bashrc", ".bash_profile"] {
            let path = home.join(name);
            if path.exists() {
                candidates.push((path, Shell::Posix));
            }
        }
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        for (path, shell) in [
            (config.join("fish/config.fish"), Shell::Fish),
            (
                config.join("powershell/Microsoft.PowerShell_profile.ps1"),
                Shell::Pwsh,
            ),
        ] {
            if path.exists() {
                candidates.push((path, shell));
            }
        }
        if candidates.is_empty() {
            let shell = std::env::var("SHELL").unwrap_or_default();
            candidates.push(if shell.ends_with("fish") {
                (config.join("fish/config.fish"), Shell::Fish)
            } else if shell.ends_with("pwsh") {
                (
                    config.join("powershell/Microsoft.PowerShell_profile.ps1"),
                    Shell::Pwsh,
                )
            } else {
                (
                    home.join(if shell.ends_with("bash") {
                        ".bashrc"
                    } else {
                        ".zshrc"
                    }),
                    Shell::Posix,
                )
            });
        }
    }
    #[cfg(windows)]
    {
        for name in ["PowerShell", "WindowsPowerShell"] {
            candidates.push((
                home.join("Documents")
                    .join(name)
                    .join("Microsoft.PowerShell_profile.ps1"),
                Shell::Pwsh,
            ));
        }
    }
    candidates
}
fn source(env: &BTreeMap<String, String>, shell: Shell) -> String {
    let clear = overrides();
    match shell {
        Shell::Posix => format!(
            "unset {}\n{}\n",
            clear.join(" "),
            env.iter()
                .map(|(k, v)| format!("export {k}={}", shell_quote(v)))
                .collect::<Vec<_>>()
                .join("\n")
        ),
        Shell::Fish => format!(
            "function copilot\n    command env {} {} copilot $argv\nend\n",
            clear
                .iter()
                .map(|k| format!("-u {k}"))
                .collect::<Vec<_>>()
                .join(" "),
            env.iter()
                .map(|(k, v)| fish_quote(&format!("{k}={v}")))
                .collect::<Vec<_>>()
                .join(" ")
        ),
        Shell::Pwsh => {
            let keys = clear
                .iter()
                .chain(env.keys())
                .map(|k| ps_quote(k))
                .collect::<Vec<_>>()
                .join(", ");
            let set = clear
                .iter()
                .map(|k| {
                    format!(
                        "[Environment]::SetEnvironmentVariable('{}', $null, 'Process')",
                        k
                    )
                })
                .chain(env.iter().map(|(k, v)| {
                    format!(
                        "[Environment]::SetEnvironmentVariable('{}', {}, 'Process')",
                        k,
                        ps_quote(v)
                    )
                }))
                .collect::<Vec<_>>()
                .join("\n        ");
            format!(
                "function global:copilot {{\n    $gitAiSaved = @{{}}\n    foreach ($gitAiKey in @({keys})) {{ $gitAiSaved[$gitAiKey] = [Environment]::GetEnvironmentVariable($gitAiKey, 'Process') }}\n    try {{\n        {set}\n        & (Get-Command copilot -CommandType Application -ErrorAction Stop | Select-Object -First 1).Source @args\n    }} finally {{\n        foreach ($gitAiKey in $gitAiSaved.Keys) {{ [Environment]::SetEnvironmentVariable($gitAiKey, $gitAiSaved[$gitAiKey], 'Process') }}\n    }}\n}}\n"
            )
        }
    }
}
fn cli(agent: &Value, dry_run: bool) -> Result<Vec<String>, GitAiError> {
    if !cli_present() {
        return Ok(Vec::new());
    }
    let values = agent
        .pointer("/cli/env")
        .and_then(Value::as_object)
        .ok_or_else(|| error("invalid Copilot CLI environment"))?;
    let mut env = BTreeMap::new();
    for key in ENV_KEYS {
        let value = values
            .get(*key)
            .and_then(Value::as_str)
            .filter(|v| !v.contains(['\0', '\n', '\r']))
            .ok_or_else(|| error("missing or invalid Copilot CLI environment setting"))?;
        env.insert((*key).to_owned(), value.to_owned());
    }
    let dir = home_dir().join(".git-ai/copilot");
    let conflict=regex::Regex::new(r"(?im)(?:\bfunction\s+(?:global:)?copilot\b|\bcopilot\s*\(\s*\)|\balias\s+copilot\s*=|\bset-alias\s+copilot\b)").expect("constant regex");
    let mut diffs = Vec::new();
    for (profile, shell) in profiles() {
        let old = if profile.exists() {
            fs::read_to_string(&profile)?
        } else {
            String::new()
        };
        let mut unmanaged = old.clone();
        let range = if let Some(start) = old.find(BEGIN) {
            let end = old[start..].find(END).ok_or_else(|| {
                error("incomplete managed Copilot shell block; repair the profile before retrying")
            })? + start
                + END.len();
            unmanaged.replace_range(start..end, "");
            Some(start..end)
        } else {
            None
        };
        if conflict.is_match(&unmanaged) {
            return Err(error(
                "existing Copilot shell function or alias was preserved; remove the conflict and rerun install-hooks",
            ));
        }
        let file = dir.join(match shell {
            Shell::Posix => "env.sh",
            Shell::Fish => "env.fish",
            Shell::Pwsh => "env.ps1",
        });
        let path = file.to_string_lossy();
        let definition = match shell {
            Shell::Posix => format!(
                "case \"$(type copilot 2>/dev/null)\" in\n  *function*|*alias*) ;;\n  *) function copilot() (\n    . {} || return\n    command copilot \"$@\"\n) ;;\nesac",
                shell_quote(&path)
            ),
            Shell::Fish => format!(
                "if not functions --query copilot\n    source {}\nend",
                fish_quote(&path)
            ),
            Shell::Pwsh => format!(
                "if (-not (Get-Command copilot -CommandType Function,Alias -ErrorAction SilentlyContinue)) {{ . {} }}",
                ps_quote(&path)
            ),
        };
        let block = format!("{BEGIN}\n{definition}\n{END}");
        let new = if let Some(range) = range {
            let mut s = old.clone();
            s.replace_range(range, &block);
            s
        } else {
            format!(
                "{old}{}{block}\n",
                if old.ends_with('\n') || old.is_empty() {
                    ""
                } else {
                    "\n"
                }
            )
        };
        let contents = source(&env, shell);
        if fs::read_to_string(&file).ok().as_deref() != Some(&contents) {
            diffs.push(format!(
                "{}: update private Copilot OTEL environment (credentials redacted)",
                file.display()
            ));
            if !dry_run {
                write_private_atomic(&file, contents.as_bytes())?;
            }
        }
        if old != new {
            diffs.push(format!(
                "{}: configure Copilot OTEL for new shells",
                profile.display()
            ));
            if !dry_run {
                write_private_atomic(&profile, new.as_bytes())?;
            }
        }
    }
    Ok(diffs)
}

pub(crate) fn configure(agent: &Value, dry_run: bool) -> Result<Option<String>, GitAiError> {
    // Each surface can be configured even if the other has a local conflict.
    let mut diffs = Vec::new();
    let mut errors = Vec::new();
    for result in [vscode(agent, dry_run), cli(agent, dry_run)] {
        match result {
            Ok(updates) => diffs.extend(updates),
            Err(err) => errors.push(err.to_string()),
        }
    }
    if !errors.is_empty() {
        return Err(error(&errors.join("; ")));
    }
    Ok((!diffs.is_empty()).then(|| diffs.join("\n")))
}
