use super::{
    AgentPreset, ParsedHookEvent, PostBashCall, PostFileEdit, PreBashCall, PreFileEdit,
    PresetContext,
};
use crate::commands::checkpoint_agent::bash_tool::{self, Agent, ToolClass};
use crate::error::GitAiError;
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub struct OpenCodePreset;

#[derive(Debug, Deserialize)]
struct OpenCodeHookInput {
    hook_event_name: String,
    session_id: String,
    cwd: String,
    tool_input: Option<serde_json::Value>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default, alias = "toolUseId")]
    tool_use_id: Option<String>,
}

impl OpenCodePreset {
    pub(crate) fn extract_filepaths_from_tool_input(
        tool_input: Option<&serde_json::Value>,
        cwd: &str,
    ) -> Vec<PathBuf> {
        let mut raw_paths = Vec::new();

        if let Some(value) = tool_input {
            Self::collect_tool_paths(value, &mut raw_paths);
        }

        let mut normalized_paths = Vec::new();
        for raw in raw_paths {
            if let Some(path) = Self::normalize_hook_path(&raw, cwd) {
                let pb = PathBuf::from(&path);
                if !normalized_paths.contains(&pb) {
                    normalized_paths.push(pb);
                }
            }
        }

        normalized_paths
    }

    fn collect_tool_paths(value: &serde_json::Value, out: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, val) in map {
                    let key_lower = key.to_ascii_lowercase();
                    let is_single_path_key = key_lower == "file_path"
                        || key_lower == "filepath"
                        || key_lower == "path"
                        || key_lower == "fspath";

                    let is_multi_path_key = key_lower == "files"
                        || key_lower == "filepaths"
                        || key_lower == "file_paths";

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

                    Self::collect_tool_paths(val, out);
                }
            }
            serde_json::Value::Array(arr) => {
                for item in arr {
                    Self::collect_tool_paths(item, out);
                }
            }
            serde_json::Value::String(s) => {
                if s.starts_with("file://") {
                    out.push(s.to_string());
                }
                super::parse::collect_apply_patch_paths_from_text(s, out);
            }
            _ => {}
        }
    }

    fn normalize_hook_path(raw_path: &str, cwd: &str) -> Option<String> {
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
}

impl AgentPreset for OpenCodePreset {
    fn parse(&self, hook_input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
        let hook_input: OpenCodeHookInput = serde_json::from_str(hook_input)
            .map_err(|e| GitAiError::PresetError(format!("Invalid JSON in hook_input: {}", e)))?;

        let tool_class = hook_input
            .tool_name
            .as_deref()
            .map(|name| bash_tool::classify_tool(Agent::OpenCode, name))
            .unwrap_or(ToolClass::FileEdit);
        if tool_class == ToolClass::Skip {
            return Ok(vec![]);
        }
        let is_bash = tool_class == ToolClass::Bash;

        let is_pre = hook_input.hook_event_name == "PreToolUse";

        let OpenCodeHookInput {
            hook_event_name: _,
            session_id,
            cwd,
            tool_input,
            tool_name: _,
            tool_use_id,
        } = hook_input;

        let file_paths = Self::extract_filepaths_from_tool_input(tool_input.as_ref(), &cwd);
        let bash_command = tool_input
            .as_ref()
            .and_then(|value| {
                value
                    .get("command")
                    .or_else(|| value.get("cmd"))
                    .and_then(|v| v.as_str())
            })
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string);
        let tool_use_id_str = tool_use_id.as_deref().unwrap_or("bash").to_string();

        let context = PresetContext {
            agent: "opencode".to_string(),
            external_session_id: session_id,
            trace_id: trace_id.to_string(),
            cwd: PathBuf::from(&cwd),
        };

        let event = match (is_pre, is_bash) {
            (true, true) => ParsedHookEvent::PreBashCall(PreBashCall {
                context,
                tool_use_id: tool_use_id_str,
                command: bash_command,
            }),
            (true, false) => ParsedHookEvent::PreFileEdit(PreFileEdit {
                context,
                file_paths,
                tool_use_id: Some(tool_use_id_str),
            }),
            (false, true) => ParsedHookEvent::PostBashCall(PostBashCall {
                context,
                tool_use_id: tool_use_id_str,
                command: bash_command,
            }),
            (false, false) => ParsedHookEvent::PostFileEdit(PostFileEdit {
                context,
                file_paths,
                tool_use_id: Some(tool_use_id_str),
            }),
        };

        Ok(vec![event])
    }
}
