use super::parse;
use super::{
    AgentPreset, ParsedHookEvent, PostBashCall, PostFileEdit, PreBashCall, PreFileEdit,
    PresetContext,
};
use crate::commands::checkpoint_agent::bash_tool::{self, Agent, ToolClass};
use crate::error::GitAiError;
use std::path::PathBuf;

pub struct GeminiPreset;

impl AgentPreset for GeminiPreset {
    fn parse(&self, hook_input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
        let data: serde_json::Value = serde_json::from_str(hook_input)
            .map_err(|e| GitAiError::PresetError(format!("Invalid JSON in hook_input: {}", e)))?;

        let tool_class = parse::optional_str_multi(&data, &["tool_name", "toolName"])
            .map(|name| bash_tool::classify_tool(Agent::Gemini, name))
            // Preserve payloads from older Gemini integrations that omitted tool_name.
            .unwrap_or(ToolClass::FileEdit);
        if tool_class == ToolClass::Skip {
            return Ok(Vec::new());
        }

        let cwd = parse::required_str(&data, "cwd")?;
        let session_id = parse::required_str(&data, "session_id")?.to_string();
        let hook_event = parse::optional_str_multi(&data, &["hook_event_name", "hookEventName"]);
        let tool_use_id = parse::str_or_default_multi(&data, &["tool_use_id", "toolUseId"], "bash");

        let is_bash = tool_class == ToolClass::Bash;
        let file_paths = parse::file_paths_from_tool_input(&data, cwd);

        let context = PresetContext {
            agent: "gemini".to_string(),
            external_session_id: session_id,
            trace_id: trace_id.to_string(),
            cwd: PathBuf::from(cwd),
        };

        let is_pre = matches!(hook_event, Some("BeforeTool") | Some("PreToolUse"));

        let bash_command = parse::bash_command_from_hook_input(&data);
        let event = match (is_pre, is_bash) {
            (true, true) => ParsedHookEvent::PreBashCall(PreBashCall {
                context,
                tool_use_id: tool_use_id.to_string(),
                command: bash_command,
            }),
            (true, false) => ParsedHookEvent::PreFileEdit(PreFileEdit {
                context,
                file_paths,
                tool_use_id: Some(tool_use_id.to_string()),
            }),
            (false, true) => ParsedHookEvent::PostBashCall(PostBashCall {
                context,
                tool_use_id: tool_use_id.to_string(),
                command: bash_command,
            }),
            (false, false) => ParsedHookEvent::PostFileEdit(PostFileEdit {
                context,
                file_paths,
                tool_use_id: Some(tool_use_id.to_string()),
            }),
        };

        Ok(vec![event])
    }
}
