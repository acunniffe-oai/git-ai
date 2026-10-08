use crate::config::{FileConfig, MAX_DAEMON_MEMORY_LIMIT_MB};
use crate::git::repository::find_repository_in_path;
use serde_json::{Value, json};

/// Determines the type of pattern value provided
#[derive(Debug, PartialEq)]
enum PatternType {
    /// Global wildcard pattern like "*"
    GlobalWildcard,
    /// URL or git protocol (http://, https://, git@, ssh://, etc.)
    UrlOrGitProtocol,
    /// File path that should be resolved to a repository
    FilePath,
}

/// Detect the type of pattern value
fn detect_pattern_type(value: &str) -> PatternType {
    let trimmed = value.trim();

    // Check for global wildcard
    if trimmed == "*" {
        return PatternType::GlobalWildcard;
    }

    // Check for URL or git protocol patterns
    if trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
        || trimmed.starts_with("git@")
        || trimmed.starts_with("ssh://")
        || trimmed.starts_with("git://")
        || trimmed.contains("://")
        || (trimmed.contains('@') && trimmed.contains(':') && !trimmed.starts_with('/'))
    {
        return PatternType::UrlOrGitProtocol;
    }

    // Check for glob patterns with wildcards (but not just "*")
    // These are patterns like "https://github.com/org/*" or "*@github.com:*"
    if trimmed.contains('*') || trimmed.contains('?') || trimmed.contains('[') {
        return PatternType::UrlOrGitProtocol;
    }

    // Otherwise, treat as file path
    PatternType::FilePath
}

/// Resolve a file path to repository remote URLs
/// Returns the remote URLs for the repository at the given path
fn resolve_path_to_remotes(path: &str) -> Result<Vec<String>, String> {
    // Expand ~ to home directory
    let expanded_path = if path.starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            format!("{}{}", home.to_string_lossy(), &path[1..])
        } else {
            path.to_string()
        }
    } else {
        path.to_string()
    };

    // Try to find repository at path
    let repo = find_repository_in_path(&expanded_path).map_err(|_| {
        format!(
            "No git repository found at path '{}'. Provide a valid repository path, URL, or glob pattern.",
            path
        )
    })?;

    // Get remotes with URLs
    let remotes = repo
        .remotes_with_urls()
        .map_err(|e| format!("Failed to get remotes for repository at '{}': {}", path, e))?;

    if remotes.is_empty() {
        return Err(format!(
            "Repository at '{}' has no remotes configured. Add a remote first or use a glob pattern.",
            path
        ));
    }

    // Return all remote URLs
    Ok(remotes.into_iter().map(|(_, url)| url).collect())
}

const STRING_KEYS: &[&str] = &[
    "git_path",
    "api_base_url",
    "api_key",
    "telemetry_enterprise_dsn",
];
const BOOLEAN_KEYS: &[&str] = &[
    "quiet",
    "allow_superuser",
    "disable_version_checks",
    "disable_auto_updates",
];
const ARRAY_KEYS: &[&str] = &["allow_repositories", "exclude_repositories"];
const FLAG_KEYS: &[&str] = &["auth_keyring", "daemon_log_upload"];

fn validate_key(key: &str) -> Result<(), String> {
    if STRING_KEYS.contains(&key)
        || BOOLEAN_KEYS.contains(&key)
        || ARRAY_KEYS.contains(&key)
        || [
            "telemetry_oss",
            "telemetry_oss_disabled",
            "update_channel",
            "codex_hooks_format",
            "daemon_memory_limit_mb",
            "feature_flags",
        ]
        .contains(&key)
        || key
            .strip_prefix("feature_flags.")
            .is_some_and(|k| FLAG_KEYS.contains(&k))
    {
        Ok(())
    } else {
        Err(format!("Unknown config key: {key}"))
    }
}

pub fn handle_config(args: &[String]) {
    if args
        .iter()
        .any(|a| matches!(a.as_str(), "--help" | "-h" | "help"))
    {
        println!(
            "Usage: git-ai config [<key> | set <key> <value> | unset <key> | --add <key> <value>]"
        );
        println!(
            "Keys: git_path, api_base_url, api_key, quiet, allow_superuser, allow_repositories, exclude_repositories, telemetry_oss, telemetry_enterprise_dsn, disable_version_checks, disable_auto_updates, update_channel, codex_hooks_format, daemon_memory_limit_mb, feature_flags.auth_keyring, feature_flags.daemon_log_upload"
        );
        return;
    }
    let add = args.iter().any(|a| a == "--add");
    let args: Vec<&str> = args
        .iter()
        .filter(|a| a.as_str() != "--add")
        .map(String::as_str)
        .collect();
    let result = match args.as_slice() {
        [] if !add => show_config(None),
        [key] if !add => show_config(Some(key)),
        ["set", key, value] => set_config(key, value, add),
        ["unset", key] if !add => unset_config(key),
        [key, value] if add => set_config(key, value, true),
        _ => Err(
            "Usage: git-ai config [<key> | set <key> <value> | unset <key> | --add <key> <value>]"
                .into(),
        ),
    };
    if let Err(error) = result {
        eprintln!("Error: {error}");
        std::process::exit(1);
    }
}

fn show_config(key: Option<&str>) -> Result<(), String> {
    let runtime = crate::config::Config::fresh();
    let mut value = serde_json::to_value(&runtime).map_err(|e| e.to_string())?;
    if let Some(key) = key {
        validate_key(key)?;
        value = if key == "telemetry_oss" {
            json!(if runtime.is_telemetry_oss_disabled() {
                "off"
            } else {
                "on"
            })
        } else {
            key.split('.')
                .try_fold(&value, |current, key| current.get(key))
                .cloned()
                .ok_or_else(|| format!("Unknown config key: {key}"))?
        };
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?
    );
    Ok(())
}

fn repository_patterns(value: &str) -> Result<Vec<String>, String> {
    if value.trim_start().starts_with('[') {
        let values: Vec<String> = serde_json::from_str(value)
            .map_err(|e| format!("Invalid repository pattern array: {e}"))?;
        if values
            .iter()
            .any(|v| detect_pattern_type(v) == PatternType::FilePath)
        {
            return Err(
                "Use repository URLs or globs in arrays; add repository paths one at a time".into(),
            );
        }
        Ok(values)
    } else {
        match detect_pattern_type(value) {
            PatternType::FilePath => resolve_path_to_remotes(value),
            _ => Ok(vec![value.into()]),
        }
    }
}

fn set_config(key: &str, raw: &str, add: bool) -> Result<(), String> {
    validate_key(key)?;
    if add && !ARRAY_KEYS.contains(&key) && !key.starts_with("feature_flags.") {
        return Err("--add is supported for repository lists and individual feature flags".into());
    }
    let file = crate::config::load_file_config_public()?;
    let mut value = serde_json::to_value(&file).map_err(|e| e.to_string())?;
    let (storage_key, parsed) = if STRING_KEYS.contains(&key) {
        (key, json!(raw))
    } else if BOOLEAN_KEYS.contains(&key) {
        (key, json!(parse_bool(raw)?))
    } else if ARRAY_KEYS.contains(&key) {
        let mut patterns = repository_patterns(raw)?;
        for pattern in &patterns {
            glob::Pattern::new(pattern).map_err(|e| format!("Invalid repository pattern: {e}"))?;
        }
        if add {
            let mut previous: Vec<String> =
                serde_json::from_value(value.get(key).cloned().unwrap_or_else(|| json!([])))
                    .map_err(|e| e.to_string())?;
            for pattern in patterns {
                if !previous.contains(&pattern) {
                    previous.push(pattern);
                }
            }
            patterns = previous;
        }
        (key, json!(patterns))
    } else if let Some(flag) = key.strip_prefix("feature_flags.") {
        let flags = value
            .as_object_mut()
            .unwrap()
            .entry("feature_flags")
            .or_insert_with(|| json!({}));
        if !flags.is_object() {
            *flags = json!({});
        }
        flags[flag] = json!(parse_bool(raw)?);
        let file: FileConfig = serde_json::from_value(value).map_err(|e| e.to_string())?;
        crate::config::save_file_config(&file)?;
        println!("Updated {key}");
        return Ok(());
    } else {
        match key {
            "telemetry_oss" => {
                if !matches!(raw, "on" | "off") {
                    return Err("telemetry_oss must be on or off".into());
                }
                (key, json!(raw))
            }
            "telemetry_oss_disabled" => (
                "telemetry_oss",
                json!(if parse_bool(raw)? { "off" } else { "on" }),
            ),
            "update_channel" => {
                if !matches!(
                    raw,
                    "latest" | "next" | "enterprise-latest" | "enterprise-next"
                ) {
                    return Err("Invalid update channel".into());
                }
                (key, json!(raw))
            }
            "codex_hooks_format" => {
                let format = match raw {
                    "config_toml" | "config-toml" => "config_toml",
                    "hooks_json" | "hooks-json" => "hooks_json",
                    _ => return Err("Invalid Codex hook format".into()),
                };
                (key, json!(format))
            }
            "daemon_memory_limit_mb" => {
                let limit = raw
                    .parse::<u64>()
                    .map_err(|_| "daemon_memory_limit_mb must be a positive integer")?;
                if limit == 0 || limit > MAX_DAEMON_MEMORY_LIMIT_MB {
                    return Err("Invalid daemon memory limit".into());
                }
                (key, json!(limit))
            }
            "feature_flags" => {
                let flags: Value = serde_json::from_str(raw).map_err(|e| e.to_string())?;
                let object = flags.as_object().ok_or("feature_flags must be an object")?;
                for (name, value) in object {
                    if !FLAG_KEYS.contains(&name.as_str()) || !value.is_boolean() {
                        return Err(format!("Invalid feature flag: {name}"));
                    }
                }
                (key, flags)
            }
            _ => return Err(format!("Unknown config key: {key}")),
        }
    };
    value[storage_key] = parsed;
    let file: FileConfig = serde_json::from_value(value).map_err(|e| e.to_string())?;
    crate::config::save_file_config(&file)?;
    println!("Updated {key}");
    if key == "daemon_memory_limit_mb" {
        println!("Run `git-ai daemon restart` for changes to take effect.");
    }
    Ok(())
}

fn unset_config(key: &str) -> Result<(), String> {
    validate_key(key)?;
    let file = crate::config::load_file_config_public()?;
    let mut value = serde_json::to_value(file).map_err(|e| e.to_string())?;
    if let Some(flag) = key.strip_prefix("feature_flags.") {
        if let Some(flags) = value
            .get_mut("feature_flags")
            .and_then(Value::as_object_mut)
        {
            flags.remove(flag);
        }
    } else {
        value
            .as_object_mut()
            .unwrap()
            .remove(if key == "telemetry_oss_disabled" {
                "telemetry_oss"
            } else {
                key
            });
    }
    let file: FileConfig = serde_json::from_value(value).map_err(|e| e.to_string())?;
    crate::config::save_file_config(&file)?;
    println!("Unset {key}");
    Ok(())
}

fn parse_bool(value: &str) -> Result<bool, String> {
    match value.to_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => Err(format!(
            "Invalid boolean value: '{}'. Expected true/false",
            value
        )),
    }
}
