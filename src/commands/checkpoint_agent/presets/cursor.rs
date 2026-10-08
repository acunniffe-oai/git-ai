use super::parse;
use super::{
    AgentPreset, ParsedHookEvent, PostBashCall, PostFileEdit, PreBashCall, PreFileEdit,
    PresetContext,
};
use crate::commands::checkpoint_agent::bash_tool::{self, Agent, ToolClass};
use crate::error::GitAiError;
use std::path::PathBuf;

pub struct CursorPreset;

pub struct CursorBackgroundPreset;

impl AgentPreset for CursorBackgroundPreset {
    fn parse(&self, hook_input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
        if std::env::var("HOSTNAME").ok().as_deref() != Some("cursor") {
            return Err(GitAiError::PresetError(
                "Skipping cursor-background hook outside cursor agent environment.".to_string(),
            ));
        }
        CursorPreset.parse(hook_input, trace_id)
    }
}

impl AgentPreset for CursorPreset {
    fn parse(&self, hook_input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
        let data: serde_json::Value = serde_json::from_str(hook_input)
            .map_err(|e| GitAiError::PresetError(format!("Invalid JSON in hook_input: {}", e)))?;

        // conversation_id is required for session_id
        let conversation_id = parse::required_str(&data, "conversation_id")?.to_string();

        // workspace_roots array — first element is default cwd
        let workspace_roots = data
            .get("workspace_roots")
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                GitAiError::PresetError("workspace_roots not found in hook_input".to_string())
            })?
            .iter()
            .filter_map(|v| v.as_str().map(normalize_cursor_path))
            .collect::<Vec<String>>();

        let hook_event_name = parse::required_str(&data, "hook_event_name")?;

        // Legacy hooks no longer installed; return error so orchestrator skips.
        if hook_event_name == "beforeSubmitPrompt" || hook_event_name == "afterFileEdit" {
            return Err(GitAiError::PresetError(
                "Legacy Cursor hook events (beforeSubmitPrompt/afterFileEdit) are no longer supported."
                    .to_string(),
            ));
        }

        // Validate hook_event_name
        if hook_event_name != "preToolUse" && hook_event_name != "postToolUse" {
            return Err(GitAiError::PresetError(format!(
                "Invalid hook_event_name: {}. Expected 'preToolUse' or 'postToolUse'",
                hook_event_name
            )));
        }

        // Classify the tool: file-edit (Write/Delete/StrReplace), bash (Shell), or skip.
        let tool_name = parse::optional_str(&data, "tool_name").unwrap_or("");
        let tool_class = bash_tool::classify_tool(Agent::Cursor, tool_name);
        if tool_class == ToolClass::Skip {
            return Err(GitAiError::PresetError(format!(
                "Skipping Cursor hook for unsupported tool_name '{}'.",
                tool_name
            )));
        }

        // Extract the edited path from Cursor file-edit tool input.
        let file_path = cursor_file_path_from_tool_input(data.get("tool_input"));

        // For ApplyPatch, extract file paths from patch text if no direct file_path.
        let patch_paths = if file_path.is_empty() && tool_name == "ApplyPatch" {
            extract_paths_from_patch(data.get("tool_input"))
        } else {
            vec![]
        };

        // Resolve cwd: match file_path to workspace root, or fall back to first root.
        // For Shell tools `file_path` is empty, so this returns workspace_roots[0].
        let first_path = if !file_path.is_empty() {
            &file_path
        } else {
            patch_paths.first().map(|s| s.as_str()).unwrap_or("")
        };
        let cwd = resolve_repo_cwd(first_path, &workspace_roots).ok_or_else(|| {
            GitAiError::PresetError("No workspace root found in hook_input".to_string())
        })?;

        let file_paths = if !file_path.is_empty() {
            vec![parse::resolve_absolute(&file_path, &cwd)]
        } else if !patch_paths.is_empty() {
            patch_paths
                .iter()
                .map(|p| parse::resolve_absolute(p, &cwd))
                .collect()
        } else {
            vec![]
        };

        let context = PresetContext {
            agent: "cursor".to_string(),
            external_session_id: conversation_id.clone(),
            trace_id: trace_id.to_string(),
            cwd: PathBuf::from(&cwd),
        };

        let is_pre = hook_event_name == "preToolUse";
        let tool_use_id = parse::optional_str(&data, "tool_use_id")
            .unwrap_or("bash")
            .to_string();

        let bash_command = parse::bash_command_from_hook_input(&data);
        let event = match (tool_class, is_pre) {
            (ToolClass::Bash, true) => ParsedHookEvent::PreBashCall(PreBashCall {
                context,
                tool_use_id,
                command: bash_command.clone(),
            }),
            (ToolClass::Bash, false) => ParsedHookEvent::PostBashCall(PostBashCall {
                context,
                tool_use_id,
                command: bash_command,
            }),
            (ToolClass::FileEdit, true) => ParsedHookEvent::PreFileEdit(PreFileEdit {
                context,
                file_paths,
                tool_use_id: Some(tool_use_id),
            }),
            (ToolClass::FileEdit, false) => ParsedHookEvent::PostFileEdit(PostFileEdit {
                context,
                file_paths,
                tool_use_id: Some(tool_use_id),
            }),
            (ToolClass::Skip, _) => unreachable!("Skip handled above"),
        };

        Ok(vec![event])
    }
}

/// Normalize Windows paths that Cursor sends in Unix-style format.
///
/// On Windows, Cursor sometimes sends paths like `/c:/Users/...` instead of `C:\Users\...`.
/// This function converts those paths to proper Windows format.
#[cfg(windows)]
fn normalize_cursor_path(path: &str) -> String {
    let mut chars = path.chars();
    if chars.next() == Some('/')
        && let (Some(drive), Some(':')) = (chars.next(), chars.next())
        && drive.is_ascii_alphabetic()
    {
        let rest: String = chars.collect();
        let normalized_rest = rest.replace('/', "\\");
        return format!("{}:{}", drive.to_ascii_uppercase(), normalized_rest);
    }
    path.to_string()
}

#[cfg(not(windows))]
fn normalize_cursor_path(path: &str) -> String {
    path.to_string()
}

/// Extract file paths from an ApplyPatch tool_input's patch text.
/// Delegates to the shared `parse::collect_apply_patch_paths_from_text` helper,
/// then applies `normalize_cursor_path` so patch-extracted paths get the same
/// Windows `/c:/...` -> `C:\...` normalization as JSON-field paths.
fn extract_paths_from_patch(tool_input: Option<&serde_json::Value>) -> Vec<String> {
    let mut paths = Vec::new();
    let patch_text = tool_input.and_then(|ti| {
        ti.as_str()
            .or_else(|| ti.get("patch").and_then(|v| v.as_str()))
    });
    if let Some(text) = patch_text {
        parse::collect_apply_patch_paths_from_text(text, &mut paths);
    }
    paths
        .into_iter()
        .map(|p| normalize_cursor_path(&p))
        .collect()
}

fn cursor_file_path_from_tool_input(tool_input: Option<&serde_json::Value>) -> String {
    tool_input
        .and_then(|ti| {
            ["file_path", "path", "filePath"]
                .iter()
                .find_map(|key| ti.get(key).and_then(|v| v.as_str()))
        })
        .map(normalize_cursor_path)
        .unwrap_or_default()
}

/// Find the workspace root that matches the given file path.
fn matching_workspace_root(file_path: &str, workspace_roots: &[String]) -> Option<String> {
    workspace_roots
        .iter()
        .find(|root| {
            let root_str = root.as_str();
            file_path.starts_with(root_str)
                && (file_path.len() == root_str.len()
                    || file_path[root_str.len()..].starts_with('/')
                    || file_path[root_str.len()..].starts_with('\\')
                    || root_str.ends_with('/')
                    || root_str.ends_with('\\'))
        })
        .cloned()
}

/// Resolve the cwd for a Cursor hook based on file_path and workspace_roots.
/// Falls back to the first workspace root if no match is found.
fn resolve_repo_cwd(file_path: &str, workspace_roots: &[String]) -> Option<String> {
    if file_path.is_empty() {
        return workspace_roots.first().cloned();
    }
    matching_workspace_root(file_path, workspace_roots).or_else(|| workspace_roots.first().cloned())
}
