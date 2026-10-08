use super::parse;
use super::{
    AgentPreset, ParsedHookEvent, PostBashCall, PostFileEdit, PreBashCall, PreFileEdit,
    PresetContext,
};
use crate::commands::checkpoint_agent::bash_tool::{self, Agent, ToolClass};
use crate::error::GitAiError;
use std::path::PathBuf;

pub struct ClaudePreset;

impl ClaudePreset {
    fn is_vscode_copilot_hook_payload(data: &serde_json::Value) -> bool {
        if let Some(path) = parse::optional_str(data, "transcript_path") {
            let lower = path.to_lowercase();
            (lower.contains("github copilot") || lower.contains("github.copilot"))
                && !lower.contains(".claude")
        } else {
            data.get("extensionId").is_some()
        }
    }

    fn is_cursor_hook_payload(data: &serde_json::Value) -> bool {
        data.get("cursor_version").is_some()
            || parse::optional_str(data, "transcript_path")
                .map(|p| p.contains(".cursor"))
                .unwrap_or(false)
    }
}

impl AgentPreset for ClaudePreset {
    fn parse(&self, hook_input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
        let data: serde_json::Value = serde_json::from_str(hook_input)
            .map_err(|e| GitAiError::PresetError(format!("Invalid JSON in hook_input: {}", e)))?;

        if Self::is_vscode_copilot_hook_payload(&data) {
            return Err(GitAiError::PresetError(
                "Skipping VS Code hook payload in Claude preset; use github-copilot hooks."
                    .to_string(),
            ));
        }
        if Self::is_cursor_hook_payload(&data) {
            return Err(GitAiError::PresetError(
                "Skipping Cursor hook payload in Claude preset; use cursor hooks.".to_string(),
            ));
        }

        let tool_class = parse::optional_str_multi(&data, &["tool_name", "toolName"])
            .map(|name| bash_tool::classify_tool(Agent::Claude, name))
            // Preserve legacy Claude payloads that predate tool_name.
            .unwrap_or(ToolClass::FileEdit);
        if tool_class == ToolClass::Skip {
            return Ok(Vec::new());
        }

        let cwd = parse::required_str(&data, "cwd")?;
        let Some(session_id) = parse::optional_str(&data, "session_id").map(str::to_string) else {
            return Ok(vec![]);
        };

        let hook_event = parse::optional_str_multi(&data, &["hook_event_name", "hookEventName"]);
        let tool_use_id = parse::str_or_default_multi(&data, &["tool_use_id", "toolUseId"], "bash");

        let is_bash = tool_class == ToolClass::Bash;

        let context = PresetContext {
            agent: "claude".to_string(),
            external_session_id: session_id.clone(),
            trace_id: trace_id.to_string(),
            cwd: PathBuf::from(cwd),
        };

        let bash_command = parse::bash_command_from_hook_input(&data);
        let event = match (hook_event, is_bash) {
            (Some("PreToolUse"), true) => ParsedHookEvent::PreBashCall(PreBashCall {
                context,
                tool_use_id: tool_use_id.to_string(),
                command: bash_command,
            }),
            (Some("PreToolUse"), false) => ParsedHookEvent::PreFileEdit(PreFileEdit {
                context,
                file_paths: parse::file_paths_from_tool_input(&data, cwd),
                tool_use_id: Some(tool_use_id.to_string()),
            }),
            (_, true) => ParsedHookEvent::PostBashCall(PostBashCall {
                context,
                tool_use_id: tool_use_id.to_string(),
                command: bash_command,
            }),
            (_, false) => ParsedHookEvent::PostFileEdit(PostFileEdit {
                context,
                file_paths: parse::file_paths_from_tool_input(&data, cwd),
                tool_use_id: Some(tool_use_id.to_string()),
            }),
        };

        Ok(vec![event])
    }
}
