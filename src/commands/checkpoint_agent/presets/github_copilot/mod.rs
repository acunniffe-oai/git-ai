use super::parse;
use super::{AgentPreset, ParsedHookEvent};
use crate::error::GitAiError;
use std::path::{Path, PathBuf};

mod cli;
mod ide;

pub struct GithubCopilotPreset;

impl AgentPreset for GithubCopilotPreset {
    fn parse(&self, hook_input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
        let data: serde_json::Value = serde_json::from_str(hook_input)
            .map_err(|e| GitAiError::PresetError(format!("Invalid JSON in hook_input: {}", e)))?;

        let hook_event_name =
            parse::optional_str_multi(&data, &["hook_event_name", "hookEventName"])
                .unwrap_or("after_edit");

        if hook_event_name == "before_edit" || hook_event_name == "after_edit" {
            return Ok(vec![]);
        }

        if hook_event_name == "PreToolUse" || hook_event_name == "PostToolUse" {
            let has_transcript_path = parse::optional_str_multi(
                &data,
                &[
                    "transcript_path",
                    "transcriptPath",
                    "chat_session_path",
                    "chatSessionPath",
                ],
            )
            .is_some();

            if !has_transcript_path {
                return cli::parse_cli_hooks(&data, hook_event_name, trace_id);
            }
            return ide::parse_vscode_native_hooks(&data, hook_event_name, trace_id);
        }

        Err(GitAiError::PresetError(format!(
            "Invalid hook_event_name: {}. Expected one of 'before_edit', 'after_edit', 'PreToolUse', or 'PostToolUse'",
            hook_event_name
        )))
    }
}

// ---------------------------------------------------------------------------
// Shared helpers (used by both ide.rs and cli.rs)
// ---------------------------------------------------------------------------

pub(super) fn extract_session_id(data: &serde_json::Value) -> String {
    parse::optional_str_multi(
        data,
        &[
            "chat_session_id",
            "session_id",
            "chatSessionId",
            "sessionId",
        ],
    )
    .unwrap_or("unknown")
    .to_string()
}

/// Extract file paths from VS Code / CLI hook payload (tool_input + tool_response/tool_result).
/// Only paths from the current tool call are extracted — no session-level data.
pub(super) fn extract_filepaths_from_vscode_hook_payload(
    tool_input: Option<&serde_json::Value>,
    tool_response: Option<&serde_json::Value>,
    cwd: &str,
) -> Vec<PathBuf> {
    let mut raw_paths = Vec::new();
    if let Some(value) = tool_input {
        collect_tool_paths(value, &mut raw_paths);
    }
    if let Some(value) = tool_response {
        collect_tool_paths(value, &mut raw_paths);
    }

    let mut normalized_paths = Vec::new();
    for raw in raw_paths {
        if let Some(path) = normalize_hook_path(&raw, cwd) {
            let pathbuf = PathBuf::from(&path);
            if !normalized_paths.contains(&pathbuf) {
                normalized_paths.push(pathbuf);
            }
        }
    }
    normalized_paths
}

/// Recursively collect path-like values from a JSON value.
pub(super) fn collect_tool_paths(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, val) in map {
                let key_lower = key.to_ascii_lowercase();
                let is_single_path_key = key_lower == "file_path"
                    || key_lower == "filepath"
                    || key_lower == "path"
                    || key_lower == "fspath";

                let is_multi_path_key =
                    key_lower == "files" || key_lower == "filepaths" || key_lower == "file_paths";

                if is_single_path_key {
                    if let Some(path) = val.as_str() {
                        out.push(path.to_string());
                    }
                } else if is_multi_path_key {
                    match val {
                        serde_json::Value::String(path) => out.push(path.to_string()),
                        serde_json::Value::Array(paths) => {
                            for path_value in paths {
                                if let Some(path) = path_value.as_str() {
                                    out.push(path.to_string());
                                }
                            }
                        }
                        _ => {}
                    }
                }
                collect_tool_paths(val, out);
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                collect_tool_paths(item, out);
            }
        }
        serde_json::Value::String(s) => {
            if s.starts_with("file://") {
                out.push(s.to_string());
            }
            parse::collect_apply_patch_paths_from_text(s, out);
        }
        _ => {}
    }
}

/// Normalize a raw path from a hook payload into a canonical absolute path string.
pub(super) fn normalize_hook_path(raw_path: &str, cwd: &str) -> Option<String> {
    let trimmed = raw_path.trim();
    if trimmed.is_empty() {
        return None;
    }

    let path_without_scheme = trimmed
        .strip_prefix("file://localhost")
        .or_else(|| trimmed.strip_prefix("file://"))
        .unwrap_or(trimmed);

    let path = Path::new(path_without_scheme);
    let joined = if path.is_absolute()
        || path_without_scheme.starts_with("\\\\")
        || path_without_scheme
            .as_bytes()
            .get(1)
            .map(|b| *b == b':')
            .unwrap_or(false)
    {
        PathBuf::from(path_without_scheme)
    } else {
        Path::new(cwd).join(path_without_scheme)
    };

    Some(joined.to_string_lossy().replace('\\', "/"))
}
