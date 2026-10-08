use super::{AgentPreset, ParsedHookEvent, PostBashCall, PostFileEdit, PreBashCall, PresetContext};
use crate::error::GitAiError;
use serde::Deserialize;
use std::path::PathBuf;

pub struct AgentV1Preset;

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AgentV1Payload {
    AiAgent {
        repo_working_dir: String,
        edited_filepaths: Option<Vec<String>>,

        agent_name: String,
        conversation_id: String,
    },
    PreShellCommand {
        repo_working_dir: String,
        agent_name: String,
        conversation_id: String,
        tool_use_id: Option<String>,
        #[serde(default)]
        command: Option<String>,
    },
    PostShellCommand {
        repo_working_dir: String,
        agent_name: String,
        conversation_id: String,
        tool_use_id: Option<String>,
        #[serde(default)]
        command: Option<String>,
    },
}

fn resolve_paths(paths: Option<Vec<String>>, repo_working_dir: &str) -> Vec<PathBuf> {
    paths
        .unwrap_or_default()
        .into_iter()
        .map(|p| super::parse::resolve_absolute(&p, repo_working_dir))
        .collect()
}

fn agent_context(
    repo_working_dir: &str,
    agent_name: String,
    conversation_id: String,
    trace_id: &str,
    _command: Option<String>,
) -> PresetContext {
    PresetContext {
        agent: agent_name,
        external_session_id: conversation_id,
        trace_id: trace_id.to_string(),
        cwd: PathBuf::from(repo_working_dir),
    }
}

impl AgentPreset for AgentV1Preset {
    fn parse(&self, hook_input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
        let payload: AgentV1Payload = serde_json::from_str(hook_input).map_err(|e| {
            GitAiError::PresetError(format!(
                "Invalid AgentV1Input JSON. Format is documented here: https://usegitai.com/docs/cli/add-your-agent: \n\n Error: {}",
                e
            ))
        })?;

        let event = match payload {
            AgentV1Payload::AiAgent {
                repo_working_dir,
                edited_filepaths,
                agent_name,
                conversation_id,
            } => {
                let file_paths = resolve_paths(edited_filepaths, &repo_working_dir);
                ParsedHookEvent::PostFileEdit(PostFileEdit {
                    context: agent_context(
                        &repo_working_dir,
                        agent_name,
                        conversation_id,
                        trace_id,
                        None,
                    ),
                    file_paths,
                    tool_use_id: None,
                })
            }
            AgentV1Payload::PreShellCommand {
                repo_working_dir,
                agent_name,
                conversation_id,
                tool_use_id,
                command,
            } => ParsedHookEvent::PreBashCall(PreBashCall {
                context: agent_context(
                    &repo_working_dir,
                    agent_name,
                    conversation_id,
                    trace_id,
                    command.clone(),
                ),
                tool_use_id: tool_use_id.unwrap_or_else(|| "shell".to_string()),
                command,
            }),
            AgentV1Payload::PostShellCommand {
                repo_working_dir,
                agent_name,
                conversation_id,
                tool_use_id,
                command,
            } => ParsedHookEvent::PostBashCall(PostBashCall {
                context: agent_context(
                    &repo_working_dir,
                    agent_name,
                    conversation_id,
                    trace_id,
                    command.clone(),
                ),
                tool_use_id: tool_use_id.unwrap_or_else(|| "shell".to_string()),
                command,
            }),
        };

        Ok(vec![event])
    }
}
