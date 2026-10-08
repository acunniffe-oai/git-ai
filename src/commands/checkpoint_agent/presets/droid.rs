use super::parse;
use super::{
    AgentPreset, ParsedHookEvent, PostBashCall, PostFileEdit, PreBashCall, PreFileEdit,
    PresetContext,
};
use crate::commands::checkpoint_agent::bash_tool::{self, Agent, ToolClass};
use crate::error::GitAiError;
use std::path::PathBuf;

pub struct DroidPreset;

impl AgentPreset for DroidPreset {
    fn parse(&self, hook_input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
        let data: serde_json::Value = serde_json::from_str(hook_input)
            .map_err(|e| GitAiError::PresetError(format!("Invalid JSON in hook_input: {}", e)))?;

        let tool_name = parse::optional_str_multi(&data, &["tool_name", "toolName"]);
        let tool_class = tool_name
            .map(|name| bash_tool::classify_tool(Agent::Droid, name))
            // Preserve payloads from older Droid integrations that omitted tool_name.
            .unwrap_or(ToolClass::FileEdit);
        if tool_class == ToolClass::Skip {
            return Ok(Vec::new());
        }

        let session_id = parse::optional_str_multi(&data, &["session_id", "sessionId"])
            .unwrap_or_default()
            .to_string();

        let cwd = parse::required_str(&data, "cwd")?;

        let hook_event_name =
            parse::optional_str_multi(&data, &["hookEventName", "hook_event_name"]).ok_or_else(
                || GitAiError::PresetError("hookEventName not found in hook_input".to_string()),
            )?;

        // Extract file paths from tool_input
        let tool_input = data.get("tool_input").or_else(|| data.get("toolInput"));

        let mut file_paths: Vec<PathBuf> = tool_input
            .and_then(|ti| {
                ti.get("file_path")
                    .or_else(|| ti.get("filePath"))
                    .and_then(|v| v.as_str())
                    .map(|path| vec![parse::resolve_absolute(path, cwd)])
            })
            .unwrap_or_default();

        // For ApplyPatch, extract file paths from the patch text
        if file_paths.is_empty() && tool_name == Some("ApplyPatch") {
            let mut patch_paths = Vec::new();

            if let Some(ti) = tool_input {
                let patch_text = ti
                    .as_str()
                    .or_else(|| ti.get("patch").and_then(|v| v.as_str()));

                if let Some(text) = patch_text {
                    let mut raw_paths = Vec::new();
                    parse::collect_apply_patch_paths_from_text(text, &mut raw_paths);
                    patch_paths.extend(raw_paths.iter().map(|p| parse::resolve_absolute(p, cwd)));
                }
            }

            // For PostToolUse, also try parsing tool_response for file_path
            if patch_paths.is_empty()
                && hook_event_name == "PostToolUse"
                && let Some(tool_response) = data
                    .get("tool_response")
                    .or_else(|| data.get("toolResponse"))
            {
                let response_obj = if let Some(s) = tool_response.as_str() {
                    serde_json::from_str::<serde_json::Value>(s).ok()
                } else {
                    Some(tool_response.clone())
                };
                if let Some(obj) = response_obj
                    && let Some(path) = obj
                        .get("file_path")
                        .or_else(|| obj.get("filePath"))
                        .and_then(|v| v.as_str())
                {
                    patch_paths.push(parse::resolve_absolute(path, cwd));
                }
            }

            file_paths = patch_paths;
        }

        // Determine if this is a bash tool invocation
        let is_bash = tool_class == ToolClass::Bash;

        let tool_use_id = parse::optional_str_multi(&data, &["tool_use_id", "toolUseId"])
            .unwrap_or("bash")
            .to_string();

        let context = PresetContext {
            agent: "droid".to_string(),
            external_session_id: session_id,
            trace_id: trace_id.to_string(),
            cwd: PathBuf::from(cwd),
        };

        let bash_command = parse::bash_command_from_hook_input(&data);

        // PreToolUse
        if hook_event_name == "PreToolUse" {
            if is_bash {
                return Ok(vec![ParsedHookEvent::PreBashCall(PreBashCall {
                    context,
                    tool_use_id,
                    command: bash_command,
                })]);
            }
            return Ok(vec![ParsedHookEvent::PreFileEdit(PreFileEdit {
                context,
                file_paths,
                tool_use_id: Some(tool_use_id.clone()),
            })]);
        }

        // PostToolUse
        if is_bash {
            return Ok(vec![ParsedHookEvent::PostBashCall(PostBashCall {
                context,
                tool_use_id,
                command: bash_command,
            })]);
        }

        Ok(vec![ParsedHookEvent::PostFileEdit(PostFileEdit {
            context,
            file_paths,
            tool_use_id: Some(tool_use_id.clone()),
        })])
    }
}
