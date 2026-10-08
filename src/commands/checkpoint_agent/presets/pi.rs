use super::{
    AgentPreset, ParsedHookEvent, PostBashCall, PostFileEdit, PreBashCall, PreFileEdit,
    PresetContext,
};
use crate::commands::checkpoint_agent::bash_tool::{self, Agent, ToolClass};
use crate::error::GitAiError;
use serde::Deserialize;
use std::path::PathBuf;

pub struct PiPreset;

#[derive(Debug, Deserialize)]
struct PiHookInput {
    hook_event_name: String,
    session_id: String,
    cwd: String,
    tool_name: String,
    #[serde(default)]
    will_edit_filepaths: Vec<String>,
    #[serde(default)]
    edited_filepaths: Vec<String>,

    #[serde(default)]
    tool_use_id: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    cmd: Option<String>,
}

#[derive(Debug)]
enum PiHookEvent {
    BeforeEdit,
    AfterEdit,
    BeforeCommand,
    AfterCommand,
}

impl PiHookEvent {
    fn parse(value: &str) -> Result<Self, GitAiError> {
        match value {
            "before_edit" => Ok(Self::BeforeEdit),
            "after_edit" => Ok(Self::AfterEdit),
            "before_command" => Ok(Self::BeforeCommand),
            "after_command" => Ok(Self::AfterCommand),
            other => Err(GitAiError::PresetError(format!(
                "Unsupported Pi hook_event_name: {other}"
            ))),
        }
    }
}

impl PiPreset {
    fn is_bash_tool(tool_name: &str) -> bool {
        bash_tool::classify_tool(Agent::Pi, tool_name) == ToolClass::Bash
    }

    fn validate_tool_name(tool_name: &str) -> Result<(), GitAiError> {
        match bash_tool::classify_tool(Agent::Pi, tool_name) {
            ToolClass::FileEdit | ToolClass::Bash => Ok(()),
            ToolClass::Skip => Err(GitAiError::PresetError(format!(
                "Unsupported Pi tool_name: {tool_name}"
            ))),
        }
    }
}

impl AgentPreset for PiPreset {
    fn parse(&self, hook_input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
        let hook_input: PiHookInput = serde_json::from_str(hook_input)
            .map_err(|e| GitAiError::PresetError(format!("Invalid JSON in hook_input: {e}")))?;

        let PiHookInput {
            hook_event_name,
            session_id,
            cwd,
            tool_name,
            will_edit_filepaths,
            edited_filepaths,
            tool_use_id,
            command,
            cmd,
        } = hook_input;

        let hook_event = PiHookEvent::parse(&hook_event_name)?;
        Self::validate_tool_name(&tool_name)?;
        let is_bash = Self::is_bash_tool(&tool_name);

        // Validate event/tool consistency
        match (&hook_event, is_bash) {
            (PiHookEvent::BeforeEdit | PiHookEvent::AfterEdit, true) => {
                return Err(GitAiError::PresetError(
                    "Pi before_edit/after_edit events cannot be used with bash tools".to_string(),
                ));
            }
            (PiHookEvent::BeforeCommand | PiHookEvent::AfterCommand, false) => {
                return Err(GitAiError::PresetError(
                    "Pi before_command/after_command events require a bash tool".to_string(),
                ));
            }
            _ => {}
        }

        let tool_use_id_str = tool_use_id.as_deref().unwrap_or("bash").to_string();
        let bash_command = command
            .or(cmd)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let context = PresetContext {
            agent: "pi".to_string(),
            external_session_id: session_id,
            trace_id: trace_id.to_string(),
            cwd: PathBuf::from(&cwd),
        };

        let event = match hook_event {
            PiHookEvent::BeforeEdit => {
                if will_edit_filepaths.is_empty() {
                    return Err(GitAiError::PresetError(
                        "Pi before_edit payload requires non-empty will_edit_filepaths".to_string(),
                    ));
                }
                ParsedHookEvent::PreFileEdit(PreFileEdit {
                    context,
                    file_paths: will_edit_filepaths.into_iter().map(PathBuf::from).collect(),
                    tool_use_id: tool_use_id.clone(),
                })
            }
            PiHookEvent::AfterEdit => {
                if edited_filepaths.is_empty() {
                    return Err(GitAiError::PresetError(
                        "Pi after_edit payload requires non-empty edited_filepaths".to_string(),
                    ));
                }
                ParsedHookEvent::PostFileEdit(PostFileEdit {
                    context,
                    file_paths: edited_filepaths.into_iter().map(PathBuf::from).collect(),
                    tool_use_id,
                })
            }
            PiHookEvent::BeforeCommand => ParsedHookEvent::PreBashCall(PreBashCall {
                context,
                tool_use_id: tool_use_id_str,
                command: bash_command,
            }),
            PiHookEvent::AfterCommand => ParsedHookEvent::PostBashCall(PostBashCall {
                context,
                tool_use_id: tool_use_id_str,
                command: bash_command,
            }),
        };

        Ok(vec![event])
    }
}
