use super::super::parse;
use super::super::{
    ParsedHookEvent, PostBashCall, PostFileEdit, PreBashCall, PreFileEdit, PresetContext,
};
use crate::commands::checkpoint_agent::bash_tool::ToolClass;
use crate::error::GitAiError;
use std::path::PathBuf;

pub(super) fn parse_cli_hooks(
    data: &serde_json::Value,
    hook_event_name: &str,
    trace_id: &str,
) -> Result<Vec<ParsedHookEvent>, GitAiError> {
    let cwd = parse::optional_str_multi(data, &["cwd", "workspace_folder", "workspaceFolder"])
        .ok_or_else(|| GitAiError::PresetError("cwd not found in hook_input".to_string()))?;

    let session_id = super::extract_session_id(data);
    let tool_name =
        parse::optional_str_multi(data, &["tool_name", "toolName"]).unwrap_or("unknown");

    let class = classify_cli_tool(tool_name);
    if class == ToolClass::Skip {
        return Err(GitAiError::PresetError(format!(
            "Skipping CopilotCLI hook for non-edit tool '{}'.",
            tool_name
        )));
    }

    let tool_input = data.get("tool_input").or_else(|| data.get("toolInput"));
    let tool_result = data
        .get("tool_result")
        .or_else(|| data.get("toolResult"))
        .or_else(|| data.get("tool_response"));

    let extracted_paths =
        super::extract_filepaths_from_vscode_hook_payload(tool_input, tool_result, cwd);

    // tool_use_id is absent in CopilotCLI payloads; synthesize a stable id from session+tool_name.
    // CLI bash invocations are sync (one in flight per session) so this id is enough for Pre/Post
    // pairing within the same session.
    let tool_use_id = parse::optional_str_multi(data, &["tool_use_id", "toolUseId"])
        .map(str::to_string)
        .unwrap_or_else(|| format!("cli-{}-{}", session_id, tool_name));

    let context = PresetContext {
        agent: "github-copilot-cli".to_string(),
        external_session_id: session_id.clone(),
        trace_id: trace_id.to_string(),
        cwd: PathBuf::from(cwd),
    };

    let bash_command = parse::bash_command_from_hook_input(data);

    match (hook_event_name, class) {
        ("PreToolUse", ToolClass::Bash) => Ok(vec![ParsedHookEvent::PreBashCall(PreBashCall {
            context,
            tool_use_id,
            command: bash_command,
        })]),
        ("PostToolUse", ToolClass::Bash) => Ok(vec![ParsedHookEvent::PostBashCall(PostBashCall {
            context,
            tool_use_id,
            command: bash_command,
        })]),
        ("PreToolUse", ToolClass::FileEdit) => {
            // Full-file creation tools synthesize an empty pre-edit baseline
            // (mirrors the IDE `create_file` behavior).
            if is_create_like_cli_tool(tool_name) {
                if extracted_paths.is_empty() {
                    return Err(GitAiError::PresetError(format!(
                        "No path in CopilotCLI {tool_name} tool_input"
                    )));
                }
                return Ok(vec![ParsedHookEvent::PreFileEdit(PreFileEdit {
                    context,
                    file_paths: extracted_paths,
                    tool_use_id: Some(tool_use_id),
                })]);
            }
            if extracted_paths.is_empty() {
                return Err(GitAiError::PresetError(format!(
                    "No file paths in CopilotCLI {} PreToolUse tool_input",
                    tool_name
                )));
            }
            Ok(vec![ParsedHookEvent::PreFileEdit(PreFileEdit {
                context,
                file_paths: extracted_paths,
                tool_use_id: Some(tool_use_id),
            })])
        }
        ("PostToolUse", ToolClass::FileEdit) => {
            if extracted_paths.is_empty() {
                return Err(GitAiError::PresetError(format!(
                    "No file paths in CopilotCLI {} PostToolUse tool_input",
                    tool_name
                )));
            }
            Ok(vec![ParsedHookEvent::PostFileEdit(PostFileEdit {
                context,
                file_paths: extracted_paths,
                tool_use_id: Some(tool_use_id),
            })])
        }
        _ => unreachable!("hook_event_name pre-validated by mod.rs fork"),
    }
}

fn classify_cli_tool(tool: &str) -> ToolClass {
    match tool {
        // Legacy lowercase tool names (older Copilot CLI builds).
        "bash" | "powershell" => ToolClass::Bash,
        "create" | "str_replace" | "edit" | "str_replace_editor" | "apply_patch"
        | "git_apply_patch" => ToolClass::FileEdit,
        // Claude-format tool names. Copilot CLI >= 1.0.62 emits Claude-style PascalCase
        // tool names in its hook payloads (e.g. `Bash` rather than `bash`, `Edit` rather
        // than `str_replace`). We support both the old and new names side by side.
        // For `Edit`/`Write`, `tool_input` is an apply-patch string (`*** Add File: ...` /
        // `*** Update File: ...`); path extraction handles that via collect_apply_patch_paths.
        "Bash" | "PowerShell" | "Powershell" => ToolClass::Bash,
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => ToolClass::FileEdit,
        // Skip read-only and control tools:
        //   view, read_file, grep, glob, semantic_search — read-only (legacy lowercase).
        //   Read, Grep, Glob, LS, Task, TodoWrite, WebFetch, WebSearch — read-only/control (Claude-format).
        //   report_intent, task_complete, ask_user, update_todo — metadata/control.
        //   read_bash / write_bash / stop_bash / list_bash — control ops on async shell.
        //   read_powershell / write_powershell / stop_powershell / list_powershell — same for PS.
        _ => ToolClass::Skip,
    }
}

fn is_create_like_cli_tool(tool: &str) -> bool {
    matches!(tool, "create" | "Write")
}
