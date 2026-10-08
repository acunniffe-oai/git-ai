use super::parse;
use super::{
    AgentPreset, ParsedHookEvent, PostBashCall, PostFileEdit, PreBashCall, PreFileEdit,
    PresetContext,
};
use crate::commands::checkpoint_agent::bash_tool::{self, Agent, ToolClass};
use crate::error::GitAiError;
use serde::Deserialize;
use std::path::PathBuf;

pub struct AmpPreset;

#[derive(Debug, Deserialize)]
struct AmpHookInput {
    hook_event_name: String,
    #[serde(default)]
    tool_use_id: Option<String>,
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    edited_filepaths: Option<Vec<String>>,
    #[serde(default)]
    tool_input: Option<serde_json::Value>,
    #[serde(default)]
    tool_name: Option<String>,
}

impl AmpPreset {
    fn extract_file_paths(hook_input: &AmpHookInput, cwd: &str) -> Vec<PathBuf> {
        if let Some(paths) = &hook_input.edited_filepaths
            && !paths.is_empty()
        {
            return paths
                .iter()
                .map(|p| parse::resolve_absolute(p, cwd))
                .collect();
        }

        if let Some(tool_input) = &hook_input.tool_input {
            let mut files = Vec::new();

            for key in ["path", "filePath", "file_path"] {
                if let Some(path) = tool_input.get(key).and_then(|value| value.as_str())
                    && !path.trim().is_empty()
                {
                    files.push(parse::resolve_absolute(path, cwd));
                }
            }

            if let Some(paths) = tool_input.get("paths").and_then(|value| value.as_array()) {
                for path in paths {
                    if let Some(path) = path.as_str()
                        && !path.trim().is_empty()
                    {
                        files.push(parse::resolve_absolute(path, cwd));
                    }
                }
            }

            if !files.is_empty() {
                return files;
            }
        }

        vec![]
    }
}

impl AgentPreset for AmpPreset {
    fn parse(&self, hook_input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
        let hook_input: AmpHookInput = serde_json::from_str(hook_input)
            .map_err(|e| GitAiError::PresetError(format!("Invalid JSON in hook_input: {}", e)))?;

        let tool_class = hook_input
            .tool_name
            .as_deref()
            .map(|name| bash_tool::classify_tool(Agent::Amp, name))
            // Preserve payloads from older Amp integrations that omitted tool_name.
            .unwrap_or(ToolClass::FileEdit);
        if tool_class == ToolClass::Skip {
            return Ok(Vec::new());
        }

        let is_pre = hook_input.hook_event_name == "PreToolUse";

        let is_bash = tool_class == ToolClass::Bash;

        let cwd = hook_input.cwd.as_deref().unwrap_or(".");

        let thread_id = hook_input.thread_id.clone();
        let tool_use_id = hook_input.tool_use_id.clone();
        let tool_use_id_str = tool_use_id.as_deref().unwrap_or("bash").to_string();

        let file_paths = Self::extract_file_paths(&hook_input, cwd);

        let session_id = thread_id.unwrap_or_default();

        let context = PresetContext {
            agent: "amp".to_string(),
            external_session_id: session_id,
            trace_id: trace_id.to_string(),
            cwd: PathBuf::from(cwd),
        };

        let bash_command = hook_input
            .tool_input
            .as_ref()
            .and_then(|tool_input| {
                tool_input
                    .get("command")
                    .or_else(|| tool_input.get("cmd"))
                    .and_then(|v| v.as_str())
            })
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string);
        let event = match (is_pre, is_bash) {
            (true, true) => ParsedHookEvent::PreBashCall(PreBashCall {
                context,
                tool_use_id: tool_use_id_str,
                command: bash_command,
            }),
            (true, false) => ParsedHookEvent::PreFileEdit(PreFileEdit {
                context,
                file_paths,
                tool_use_id: tool_use_id.clone(),
            }),
            (false, true) => ParsedHookEvent::PostBashCall(PostBashCall {
                context,
                tool_use_id: tool_use_id_str,
                command: bash_command,
            }),
            (false, false) => ParsedHookEvent::PostFileEdit(PostFileEdit {
                context,
                file_paths,
                tool_use_id: tool_use_id.clone(),
            }),
        };

        Ok(vec![event])
    }
}
