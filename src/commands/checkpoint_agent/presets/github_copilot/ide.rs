use super::super::parse;
use super::super::{
    ParsedHookEvent, PostBashCall, PostFileEdit, PreBashCall, PreFileEdit, PresetContext,
};
use crate::commands::checkpoint_agent::bash_tool::ToolClass;
use crate::error::GitAiError;
use std::path::PathBuf;

pub(super) fn parse_vscode_native_hooks(
    data: &serde_json::Value,
    hook_event_name: &str,
    trace_id: &str,
) -> Result<Vec<ParsedHookEvent>, GitAiError> {
    let cwd = parse::optional_str_multi(data, &["cwd", "workspace_folder", "workspaceFolder"])
        .ok_or_else(|| GitAiError::PresetError("cwd not found in hook_input".to_string()))?;

    let session_id = super::extract_session_id(data);

    let tool_name =
        parse::optional_str_multi(data, &["tool_name", "toolName"]).unwrap_or("unknown");

    // Enforce tool filtering to avoid creating checkpoints for read/search tools
    if !is_supported_vscode_edit_tool_name(tool_name) {
        return Err(GitAiError::PresetError(format!(
            "Skipping VS Code hook for unsupported tool_name '{}' (non-edit tool).",
            tool_name
        )));
    }

    let tool_input = data.get("tool_input").or_else(|| data.get("toolInput"));
    let tool_response = data
        .get("tool_response")
        .or_else(|| data.get("toolResponse"));

    // Extract file paths from tool_input and tool_response only (not session-level data)
    let extracted_paths =
        super::extract_filepaths_from_vscode_hook_payload(tool_input, tool_response, cwd);

    let transcript_path = transcript_path_from_hook_data(data).map(|s| s.to_string());

    if let Some(ref path) = transcript_path
        && looks_like_claude_transcript_path(path)
    {
        return Err(GitAiError::PresetError(
            "Skipping VS Code hook because transcript_path looks like a Claude transcript path."
                .to_string(),
        ));
    }

    if !is_likely_copilot_native_hook(transcript_path.as_deref()) {
        return Err(GitAiError::PresetError(format!(
            "Skipping VS Code hook for non-Copilot session (tool_name: {}).",
            tool_name,
        )));
    }

    let tool_class = classify_copilot_tool(tool_name);
    let is_bash = tool_class == ToolClass::Bash;
    let bash_command = parse::bash_command_from_hook_input(data);

    let tool_use_id = parse::optional_str_multi(data, &["tool_use_id", "toolUseId"])
        .unwrap_or("unknown")
        .to_string();

    let context = PresetContext {
        agent: "github-copilot".to_string(),
        external_session_id: session_id,
        trace_id: trace_id.to_string(),
        cwd: PathBuf::from(cwd),
    };

    if hook_event_name == "PreToolUse" {
        if is_bash {
            return Ok(vec![ParsedHookEvent::PreBashCall(PreBashCall {
                context,
                tool_use_id,
                command: bash_command,
            })]);
        }

        if tool_name.eq_ignore_ascii_case("create_file") {
            if extracted_paths.is_empty() {
                return Err(GitAiError::PresetError(
                    "No file path found in create_file PreToolUse tool_input".to_string(),
                ));
            }

            return Ok(vec![ParsedHookEvent::PreFileEdit(PreFileEdit {
                context,
                file_paths: extracted_paths,
                tool_use_id: Some(tool_use_id),
            })]);
        }

        if extracted_paths.is_empty() {
            return Err(GitAiError::PresetError(format!(
                "No editable file paths found in VS Code hook input (tool_name: {}). Skipping checkpoint.",
                tool_name
            )));
        }

        return Ok(vec![ParsedHookEvent::PreFileEdit(PreFileEdit {
            context,
            file_paths: extracted_paths,
            tool_use_id: Some(tool_use_id),
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

    if extracted_paths.is_empty() {
        return Err(GitAiError::PresetError(format!(
            "No editable file paths found in VS Code PostToolUse hook input (tool_name: {}). Skipping checkpoint.",
            tool_name
        )));
    }

    Ok(vec![ParsedHookEvent::PostFileEdit(PostFileEdit {
        context,
        file_paths: extracted_paths,
        tool_use_id: Some(tool_use_id),
    })])
}

// ---------------------------------------------------------------------------
// IDE-specific helpers
// ---------------------------------------------------------------------------

fn transcript_path_from_hook_data(data: &serde_json::Value) -> Option<&str> {
    parse::optional_str_multi(
        data,
        &[
            "transcript_path",
            "transcriptPath",
            "chat_session_path",
            "chatSessionPath",
        ],
    )
}

fn looks_like_claude_transcript_path(path: &str) -> bool {
    let normalized = path.replace('\\', "/").to_ascii_lowercase();
    normalized.contains("/.claude/") || normalized.contains("/claude/projects/")
}

fn looks_like_copilot_transcript_path(path: &str) -> bool {
    let normalized = path.replace('\\', "/").to_ascii_lowercase();
    normalized.contains("/github.copilot-chat/transcripts/")
        || normalized.contains("vscode-chat-session")
        || normalized.contains("copilot_session")
        || (normalized.contains("/workspacestorage/") && normalized.contains("/chatsessions/"))
}

fn is_likely_copilot_native_hook(transcript_path: Option<&str>) -> bool {
    let Some(path) = transcript_path else {
        return false;
    };
    if looks_like_claude_transcript_path(path) {
        return false;
    }
    looks_like_copilot_transcript_path(path)
}

fn is_supported_vscode_edit_tool_name(tool_name: &str) -> bool {
    let lower = tool_name.to_ascii_lowercase();

    // Explicit bash/terminal tools
    let bash_tools = ["run_in_terminal"];
    if bash_tools.iter().any(|name| lower == *name) {
        return true;
    }

    let non_edit_keywords = [
        "find", "search", "read", "grep", "glob", "list", "ls", "fetch", "web", "open", "todo",
    ];
    if non_edit_keywords.iter().any(|kw| lower.contains(kw)) {
        return false;
    }

    let exact_edit_tools = [
        "write",
        "edit",
        "multiedit",
        "applypatch",
        "apply_patch",
        "copilot_insertedit",
        "copilot_replacestring",
        "vscode_editfile_internal",
        "create_file",
        "delete_file",
        "rename_file",
        "move_file",
        "replace_string_in_file",
        "insert_edit_into_file",
    ];
    if exact_edit_tools.iter().any(|name| lower == *name) {
        return true;
    }

    lower.contains("edit") || lower.contains("write") || lower.contains("replace")
}

/// Classify GitHub Copilot tool for bash vs file edit handling.
/// GithubCopilot is not in the `Agent` enum, so we implement classification locally.
fn classify_copilot_tool(tool_name: &str) -> ToolClass {
    let lower = tool_name.to_ascii_lowercase();
    match lower.as_str() {
        "run_in_terminal" => ToolClass::Bash,
        "create_file"
        | "replace_string_in_file"
        | "apply_patch"
        | "delete_file"
        | "rename_file"
        | "move_file" => ToolClass::FileEdit,
        _ if lower.contains("edit") || lower.contains("write") || lower.contains("replace") => {
            ToolClass::FileEdit
        }
        _ => ToolClass::Skip,
    }
}
