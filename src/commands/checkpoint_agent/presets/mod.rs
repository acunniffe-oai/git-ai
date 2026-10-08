pub mod parse;

mod agent_v1;
mod amp;
mod claude;
mod cline;
mod codex;
mod continue_cli;
mod cursor;
mod droid;
mod firebender;
mod gemini;
mod github_copilot;
mod opencode;
mod pi;
mod windsurf;

use crate::error::GitAiError;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresetContext {
    pub agent: String,
    pub external_session_id: String,
    pub trace_id: String,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ParsedHookEvent {
    PreFileEdit(PreFileEdit),
    PostFileEdit(PostFileEdit),
    PreBashCall(PreBashCall),
    PostBashCall(PostBashCall),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreFileEdit {
    pub context: PresetContext,
    pub file_paths: Vec<PathBuf>,
    #[serde(default)]
    pub tool_use_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostFileEdit {
    pub context: PresetContext,
    pub file_paths: Vec<PathBuf>,
    #[serde(default)]
    pub tool_use_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreBashCall {
    pub context: PresetContext,
    pub tool_use_id: String,
    #[serde(default)]
    pub command: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostBashCall {
    pub context: PresetContext,
    pub tool_use_id: String,
    #[serde(default)]
    pub command: Option<String>,
}

pub trait AgentPreset {
    fn parse(&self, hook_input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, GitAiError>;
}

fn resolve_native_preset(name: &str) -> Result<Box<dyn AgentPreset>, GitAiError> {
    match name {
        "claude" => Ok(Box::new(claude::ClaudePreset)),
        "cline" => Ok(Box::new(cline::ClinePreset)),
        "codex" => Ok(Box::new(codex::CodexPreset)),
        "gemini" => Ok(Box::new(gemini::GeminiPreset)),
        "windsurf" => Ok(Box::new(windsurf::WindsurfPreset)),
        "continue-cli" => Ok(Box::new(continue_cli::ContinueCliPreset)),
        "cursor" => Ok(Box::new(cursor::CursorPreset)),
        "cursor-background" => Ok(Box::new(cursor::CursorBackgroundPreset)),
        "github-copilot" => Ok(Box::new(github_copilot::GithubCopilotPreset)),
        "amp" => Ok(Box::new(amp::AmpPreset)),
        "firebender" => Ok(Box::new(firebender::FirebenderPreset)),
        "agent-v1" => Ok(Box::new(agent_v1::AgentV1Preset)),
        "droid" => Ok(Box::new(droid::DroidPreset)),
        "opencode" => Ok(Box::new(opencode::OpenCodePreset)),
        "pi" => Ok(Box::new(pi::PiPreset)),
        _ => Err(GitAiError::PresetError(format!("Unknown preset: {}", name))),
    }
}

/// Every native format passes the same identity and failed-tool checks.
struct ValidatedPreset(Box<dyn AgentPreset>);

impl AgentPreset for ValidatedPreset {
    fn parse(&self, hook_input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
        let failed = serde_json::from_str::<serde_json::Value>(hook_input)
            .is_ok_and(|data| hook_failed(&data));
        let mut events = self.0.parse(hook_input, trace_id)?;
        events.retain(|event| {
            // A shell command can write successfully before returning nonzero;
            // its post hook must still close the invocation and compare stats.
            if failed && matches!(event, ParsedHookEvent::PostFileEdit(_)) {
                return false;
            }
            let context = match event {
                ParsedHookEvent::PreFileEdit(e) => &e.context,
                ParsedHookEvent::PostFileEdit(e) => &e.context,
                ParsedHookEvent::PreBashCall(e) => &e.context,
                ParsedHookEvent::PostBashCall(e) => &e.context,
            };
            crate::commit_links::store::SessionIdentity::new(
                &context.agent,
                &context.external_session_id,
            )
            .is_some()
        });
        Ok(events)
    }
}

fn hook_failed(data: &serde_json::Value) -> bool {
    let event = parse::optional_str_multi(data, &["hook_event_name", "hookEventName", "hook_name"]);
    if event.is_some_and(|name| name.to_ascii_lowercase().contains("failure")) {
        return true;
    }
    for value in [
        Some(data),
        data.get("tool_response"),
        data.get("toolResponse"),
        data.get("tool_result"),
        data.get("postToolUse"),
    ] {
        let Some(value) = value else {
            continue;
        };
        if value.get("is_error").and_then(serde_json::Value::as_bool) == Some(true)
            || value.get("success").and_then(serde_json::Value::as_bool) == Some(false)
            || value
                .get("exit_code")
                .and_then(serde_json::Value::as_i64)
                .is_some_and(|code| code != 0)
        {
            return true;
        }
    }
    false
}

pub fn resolve_preset(name: &str) -> Result<Box<dyn AgentPreset>, GitAiError> {
    Ok(Box::new(ValidatedPreset(resolve_native_preset(name)?)))
}
