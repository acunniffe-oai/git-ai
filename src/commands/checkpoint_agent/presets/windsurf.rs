use super::parse;
use super::{
    AgentPreset, ParsedHookEvent, PostBashCall, PostFileEdit, PreBashCall, PreFileEdit,
    PresetContext,
};
use crate::error::GitAiError;
use std::path::PathBuf;

pub struct WindsurfPreset;

/// Escape raw ASCII control characters (0x00..=0x1F) that appear inside JSON
/// string literals so the input parses under strict serde_json. Bytes outside
/// string literals, already-escaped sequences, and non-control bytes are left
/// untouched. This is a byte-level pass; it does not validate the rest of the
/// JSON grammar.
fn escape_control_chars_in_json_strings(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut in_string = false;
    let mut escaped = false;
    for &b in bytes {
        if in_string {
            if escaped {
                out.push(b);
                escaped = false;
            } else if b == b'\\' {
                out.push(b);
                escaped = true;
            } else if b == b'"' {
                out.push(b);
                in_string = false;
            } else if b < 0x20 {
                match b {
                    b'\n' => out.extend_from_slice(b"\\n"),
                    b'\r' => out.extend_from_slice(b"\\r"),
                    b'\t' => out.extend_from_slice(b"\\t"),
                    0x08 => out.extend_from_slice(b"\\b"),
                    0x0C => out.extend_from_slice(b"\\f"),
                    _ => out.extend_from_slice(format!("\\u{:04x}", b).as_bytes()),
                }
            } else {
                out.push(b);
            }
        } else {
            if b == b'"' {
                in_string = true;
            }
            out.push(b);
        }
    }
    // Safe: input was valid UTF-8, and we only inserted ASCII escape sequences.
    String::from_utf8(out).unwrap_or_else(|_| input.to_string())
}

impl AgentPreset for WindsurfPreset {
    fn parse(&self, hook_input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
        // Windsurf sometimes emits raw control characters (unescaped newlines, tabs, etc.)
        // inside JSON string values (e.g. captured command output in `tool_info`). Strict
        // serde_json rejects those, so escape them inside string literals before parsing.
        let sanitized = escape_control_chars_in_json_strings(hook_input);
        let data: serde_json::Value = serde_json::from_str(&sanitized)
            .map_err(|e| GitAiError::PresetError(format!("Invalid JSON in hook_input: {}", e)))?;

        let trajectory_id = parse::required_str(&data, "trajectory_id")?.to_string();
        let agent_action = parse::optional_str(&data, "agent_action_name");
        if !matches!(
            agent_action,
            Some("pre_write_code" | "post_write_code" | "pre_run_command" | "post_run_command")
        ) {
            return Ok(vec![]);
        }

        let tool_info = data.get("tool_info");
        let cwd = tool_info
            .and_then(|ti| ti.get("cwd"))
            .and_then(|v| v.as_str())
            .or_else(|| parse::optional_str(&data, "cwd"));

        // cwd is optional: prefer tool_info.cwd, fall back to top-level cwd, then
        // current working directory as last resort.
        let cwd_path = cwd
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let cwd_str = cwd_path.to_string_lossy().to_string();

        let context = PresetContext {
            agent: "windsurf".to_string(),
            external_session_id: trajectory_id,
            trace_id: trace_id.to_string(),
            cwd: cwd_path,
        };

        let is_bash = matches!(
            agent_action,
            Some("pre_run_command") | Some("post_run_command")
        );
        let is_pre_bash = matches!(agent_action, Some("pre_run_command"));
        let is_pre_write = matches!(agent_action, Some("pre_write_code"));

        let execution_id = tool_info
            .and_then(|ti| ti.get("execution_id"))
            .and_then(|v| v.as_str())
            .or_else(|| parse::optional_str(&data, "execution_id"))
            .unwrap_or("bash")
            .to_string();
        let bash_command = tool_info
            .and_then(|ti| ti.get("command"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
            .or_else(|| parse::bash_command_from_hook_input(&data));

        let event = if is_bash {
            if is_pre_bash {
                ParsedHookEvent::PreBashCall(PreBashCall {
                    context,
                    tool_use_id: execution_id,
                    command: bash_command,
                })
            } else {
                ParsedHookEvent::PostBashCall(PostBashCall {
                    context,
                    tool_use_id: execution_id,
                    command: bash_command,
                })
            }
        } else if is_pre_write {
            let file_path = tool_info
                .and_then(|ti| ti.get("file_path"))
                .and_then(|v| v.as_str())
                .map(|p| vec![parse::resolve_absolute(p, &cwd_str)])
                .unwrap_or_default();

            ParsedHookEvent::PreFileEdit(PreFileEdit {
                context,
                file_paths: file_path,
                tool_use_id: Some(execution_id.clone()),
            })
        } else {
            let file_path = tool_info
                .and_then(|ti| ti.get("file_path"))
                .and_then(|v| v.as_str())
                .map(|p| vec![parse::resolve_absolute(p, &cwd_str)])
                .unwrap_or_default();

            ParsedHookEvent::PostFileEdit(PostFileEdit {
                context,
                file_paths: file_path,
                tool_use_id: Some(execution_id),
            })
        };

        Ok(vec![event])
    }
}
