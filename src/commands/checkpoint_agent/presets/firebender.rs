use super::parse;
use super::{
    AgentPreset, ParsedHookEvent, PostBashCall, PostFileEdit, PreBashCall, PreFileEdit,
    PresetContext,
};
use crate::commands::checkpoint_agent::bash_tool::{self, Agent, ToolClass};
use crate::error::GitAiError;
use crate::utils::normalize_to_posix;
use serde::Deserialize;
use std::path::PathBuf;

pub struct FirebenderPreset;

#[derive(Debug, Deserialize)]
struct FirebenderHookInput {
    hook_event_name: String,
    repo_working_dir: Option<String>,
    workspace_roots: Option<Vec<String>>,
    tool_name: Option<String>,
    tool_input: Option<serde_json::Value>,
    completion_id: Option<String>,

    tool_use_id: Option<String>,
}

impl FirebenderPreset {
    fn push_unique_path(paths: &mut Vec<String>, candidate: &str) {
        let trimmed = candidate.trim();
        if !trimmed.is_empty() && !paths.iter().any(|path| path == trimmed) {
            paths.push(trimmed.to_string());
        }
    }

    fn normalize_hook_path(raw_path: &str, cwd: &str) -> Option<String> {
        let trimmed = raw_path.trim();
        if trimmed.is_empty() {
            return None;
        }

        let normalized_path = normalize_to_posix(trimmed);
        let normalized_cwd = normalize_to_posix(cwd.trim())
            .trim_end_matches('/')
            .to_string();

        if normalized_cwd.is_empty() {
            return Some(normalized_path);
        }

        let relative = if normalized_path == normalized_cwd {
            String::new()
        } else if let Some(stripped) = normalized_path.strip_prefix(&(normalized_cwd.clone() + "/"))
        {
            stripped.to_string()
        } else {
            normalized_path
        };

        Some(relative)
    }

    fn extract_patch_paths(patch: &str) -> Vec<String> {
        let mut paths = Vec::new();
        parse::collect_apply_patch_paths_from_text(patch, &mut paths);
        paths
    }

    fn extract_file_paths(tool_input: &serde_json::Value) -> Option<Vec<String>> {
        let mut paths = Vec::new();

        match tool_input {
            serde_json::Value::Object(_) => {
                for key in [
                    "file_path",
                    "target_file",
                    "relative_workspace_path",
                    "path",
                ] {
                    if let Some(path) = tool_input.get(key).and_then(|v| v.as_str()) {
                        Self::push_unique_path(&mut paths, path);
                    }
                }

                if let Some(patch) = tool_input.get("patch").and_then(|v| v.as_str()) {
                    for path in Self::extract_patch_paths(patch) {
                        Self::push_unique_path(&mut paths, &path);
                    }
                }
            }
            serde_json::Value::String(raw_patch) => {
                for path in Self::extract_patch_paths(raw_patch) {
                    Self::push_unique_path(&mut paths, &path);
                }
            }
            _ => {}
        }

        if paths.is_empty() { None } else { Some(paths) }
    }
}

impl AgentPreset for FirebenderPreset {
    fn parse(&self, hook_input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
        let hook_input: FirebenderHookInput = serde_json::from_str(hook_input)
            .map_err(|e| GitAiError::PresetError(format!("Invalid JSON in hook_input: {}", e)))?;

        let FirebenderHookInput {
            hook_event_name,
            repo_working_dir,
            workspace_roots,
            tool_name,
            tool_input,
            completion_id,
            tool_use_id,
        } = hook_input;

        // Legacy events that should be silently skipped
        if hook_event_name == "beforeSubmitPrompt" || hook_event_name == "afterFileEdit" {
            return Err(GitAiError::PresetError(format!(
                "Skipping legacy Firebender event: {}",
                hook_event_name
            )));
        }

        if hook_event_name != "preToolUse" && hook_event_name != "postToolUse" {
            return Err(GitAiError::PresetError(format!(
                "Invalid hook_event_name: {}. Expected 'preToolUse' or 'postToolUse'",
                hook_event_name
            )));
        }

        let tool_name = tool_name.unwrap_or_default();
        let tool_class = bash_tool::classify_tool(Agent::Firebender, tool_name.as_str());
        if tool_class == ToolClass::Skip {
            return Err(GitAiError::PresetError(format!(
                "Skipping unsupported Firebender tool: {}",
                tool_name
            )));
        }
        let is_bash = tool_class == ToolClass::Bash;

        let cwd = repo_working_dir
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| workspace_roots.and_then(|roots| roots.into_iter().next()));

        let cwd_str = cwd.as_deref().unwrap_or(".");

        let tool_input = tool_input.unwrap_or(serde_json::Value::Null);
        let file_paths_raw = Self::extract_file_paths(&tool_input).map(|paths| {
            paths
                .into_iter()
                .filter_map(|path| Self::normalize_hook_path(&path, cwd_str))
                .collect::<Vec<String>>()
        });

        let file_paths: Vec<PathBuf> = file_paths_raw
            .unwrap_or_default()
            .into_iter()
            .map(|p| parse::resolve_absolute(&p, cwd_str))
            .collect();

        let session_id = completion_id.unwrap_or_default();

        let context = PresetContext {
            agent: "firebender".to_string(),
            external_session_id: session_id.clone(),
            trace_id: trace_id.to_string(),
            cwd: PathBuf::from(cwd_str),
        };

        let tool_use_id_str = tool_use_id.unwrap_or_else(|| "bash".to_string());
        let bash_command = tool_input
            .get("command")
            .or_else(|| tool_input.get("cmd"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string);

        let event = match (hook_event_name.as_str(), is_bash) {
            ("preToolUse", true) => ParsedHookEvent::PreBashCall(PreBashCall {
                context,
                tool_use_id: tool_use_id_str,
                command: bash_command,
            }),
            ("preToolUse", false) => ParsedHookEvent::PreFileEdit(PreFileEdit {
                context,
                file_paths,
                tool_use_id: Some(tool_use_id_str),
            }),
            (_, true) => ParsedHookEvent::PostBashCall(PostBashCall {
                context,
                tool_use_id: tool_use_id_str,
                command: bash_command,
            }),
            (_, false) => ParsedHookEvent::PostFileEdit(PostFileEdit {
                context,
                file_paths,
                tool_use_id: Some(tool_use_id_str),
            }),
        };

        Ok(vec![event])
    }
}
