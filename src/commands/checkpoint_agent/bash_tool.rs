//! Bounded filesystem metadata evidence for native shell hooks.
//!
//! A cold session needs one positive write signal. Warm sessions never call
//! these walkers; the daemon's current commit window answers that question.

use crate::error::GitAiError;
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

const WALK_TIMEOUT: Duration = Duration::from_millis(1500);
const MAX_TRACKED_FILES: usize = 50_000;

/// Metadata fingerprint for a single file, collected via `lstat()`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatEntry {
    pub exists: bool,
    pub mtime: Option<SystemTime>,
    pub ctime: Option<SystemTime>,
    pub size: u64,
    pub mode: u32,
    pub file_type: StatFileType,
}

/// File type enumeration (symlink-aware, no following).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StatFileType {
    Regular,
    Directory,
    Symlink,
    Other,
}

impl StatEntry {
    /// Build a `StatEntry` from `std::fs::Metadata` (from `symlink_metadata` / `lstat`).
    pub fn from_metadata(meta: &fs::Metadata) -> Self {
        let file_type = if meta.file_type().is_symlink() {
            StatFileType::Symlink
        } else if meta.file_type().is_dir() {
            StatFileType::Directory
        } else if meta.file_type().is_file() {
            StatFileType::Regular
        } else {
            StatFileType::Other
        };

        let mtime = meta.modified().ok();
        let size = meta.len();
        let mode = Self::extract_mode(meta);
        let ctime = Self::extract_ctime(meta);

        StatEntry {
            exists: true,
            mtime,
            ctime,
            size,
            mode,
            file_type,
        }
    }

    #[cfg(unix)]
    fn extract_mode(meta: &fs::Metadata) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode()
    }

    #[cfg(not(unix))]
    fn extract_mode(meta: &fs::Metadata) -> u32 {
        if meta.permissions().readonly() {
            0o444
        } else {
            0o644
        }
    }

    #[cfg(unix)]
    fn extract_ctime(meta: &fs::Metadata) -> Option<SystemTime> {
        use std::os::unix::fs::MetadataExt;
        let ctime_secs = meta.ctime();
        let ctime_nsecs = meta.ctime_nsec() as u32;
        if ctime_secs >= 0 {
            Some(SystemTime::UNIX_EPOCH + std::time::Duration::new(ctime_secs as u64, ctime_nsecs))
        } else {
            None
        }
    }

    #[cfg(not(unix))]
    fn extract_ctime(meta: &fs::Metadata) -> Option<SystemTime> {
        // On Windows, use creation time as a proxy for ctime
        meta.created().ok()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolClass {
    /// A known file-edit tool (Write, Edit, etc.) — handled by existing preset logic.
    FileEdit,
    /// A bash/shell tool — handled by the stat-diff system.
    Bash,
    /// Unrecognized tool — skip checkpoint.
    Skip,
}

// ---------------------------------------------------------------------------
// Tool classification per agent (Section 8.2 of PRD)
// ---------------------------------------------------------------------------

/// Strip the `functions.` namespace prefix that Codex/OpenAI function-call
/// tools add to hook tool names (e.g. `functions.apply_patch` -> `apply_patch`).
fn normalize_tool_name(tool_name: &str) -> &str {
    tool_name.strip_prefix("functions.").unwrap_or(tool_name)
}

/// Classify a tool name for a given agent.
pub fn classify_tool(agent: Agent, tool_name: &str) -> ToolClass {
    match agent {
        Agent::Claude => match tool_name.to_ascii_lowercase().as_str() {
            "write" | "edit" | "multiedit" | "notebookedit" => ToolClass::FileEdit,
            "bash" => ToolClass::Bash,
            _ => ToolClass::Skip,
        },
        Agent::Gemini => match tool_name {
            "write_file" | "replace" | "WriteFile" => ToolClass::FileEdit,
            "shell" | "run_shell_command" => ToolClass::Bash,
            _ => ToolClass::Skip,
        },
        Agent::ContinueCli => match tool_name {
            "edit" => ToolClass::FileEdit,
            "terminal" | "local_shell_call" => ToolClass::Bash,
            _ => ToolClass::Skip,
        },
        Agent::Droid => match tool_name {
            "ApplyPatch" | "Edit" | "Write" | "Create" => ToolClass::FileEdit,
            "Bash" | "Execute" => ToolClass::Bash,
            _ => ToolClass::Skip,
        },
        Agent::Amp => match tool_name {
            "Write" | "Edit" | "create_file" | "edit_file" | "apply_patch" | "undo_edit" => {
                ToolClass::FileEdit
            }
            "Bash" | "shell_command" => ToolClass::Bash,
            _ => ToolClass::Skip,
        },
        Agent::OpenCode => match tool_name.to_ascii_lowercase().as_str() {
            "edit" | "write" | "patch" | "multiedit" | "apply_patch" | "applypatch" => {
                ToolClass::FileEdit
            }
            "bash" | "shell" => ToolClass::Bash,
            _ => ToolClass::Skip,
        },
        Agent::Firebender => match tool_name {
            "Write" | "Edit" | "Delete" | "RenameSymbol" | "DeleteSymbol" => ToolClass::FileEdit,
            "Bash" => ToolClass::Bash,
            _ => ToolClass::Skip,
        },
        Agent::Codex => {
            // Codex Desktop (OpenAI function-calling) prefixes tool names with
            // "functions."; strip it before matching the bare tool name.
            let tool_name = normalize_tool_name(tool_name);
            match tool_name {
                "apply_patch" => ToolClass::FileEdit,
                "Bash" | "exec_command" | "shell" | "shell_command" => ToolClass::Bash,
                "multi_tool_use.parallel" => ToolClass::Bash,
                _ => ToolClass::Skip,
            }
        }
        Agent::Pi => match tool_name {
            "edit" | "write" | "replace" | "rename" => ToolClass::FileEdit,
            "bash" => ToolClass::Bash,
            _ => ToolClass::Skip,
        },
        Agent::Windsurf => match tool_name {
            "code_action" => ToolClass::FileEdit,
            "run_command" => ToolClass::Bash,
            _ => ToolClass::Skip,
        },
        Agent::Cursor => match tool_name {
            "Write" | "Delete" | "StrReplace" | "ApplyPatch" => ToolClass::FileEdit,
            "Shell" => ToolClass::Bash,
            _ => ToolClass::Skip,
        },
        Agent::Cline => {
            let tool_name = normalize_tool_name(tool_name);
            match tool_name {
                "replace_in_file" | "write_to_file" | "apply_patch" | "editor" | "edit"
                | "write" => ToolClass::FileEdit,
                "execute_command" | "bash" | "shell" | "run_commands" | "run_command" => {
                    ToolClass::Bash
                }
                _ => ToolClass::Skip,
            }
        }
    }
}

/// Supported AI agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Gemini,
    ContinueCli,
    Droid,
    Amp,
    OpenCode,
    Firebender,
    Codex,
    Pi,
    Windsurf,
    Cursor,
    Cline,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatSnapshot {
    pub entries: HashMap<PathBuf, StatEntry>,
    pub repo_root: PathBuf,
}

fn walk_files(
    repo_root: &Path,
    mut visit: impl FnMut(PathBuf, StatEntry) -> bool,
) -> Result<bool, GitAiError> {
    let started = Instant::now();
    let walker = WalkBuilder::new(repo_root)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build();
    let mut count = 0;
    for entry in walker {
        if started.elapsed() >= WALK_TIMEOUT {
            return Err(GitAiError::Generic(
                "shell metadata walk exceeded time budget".into(),
            ));
        }
        let entry = entry.map_err(|error| GitAiError::Generic(error.to_string()))?;
        if entry.file_type().is_some_and(|kind| kind.is_dir()) {
            continue;
        }
        count += 1;
        if count > MAX_TRACKED_FILES {
            return Err(GitAiError::Generic(
                "shell metadata walk exceeded file budget".into(),
            ));
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        let relative = entry
            .path()
            .strip_prefix(repo_root)
            .map_err(|error| GitAiError::Generic(error.to_string()))?
            .to_path_buf();
        if visit(relative, StatEntry::from_metadata(&metadata)) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Capture metadata only. A failed/incomplete snapshot cannot prove a write.
pub fn snapshot(repo_root: &Path) -> Result<StatSnapshot, GitAiError> {
    let mut entries = HashMap::new();
    walk_files(repo_root, |path, entry| {
        entries.insert(path, entry);
        false
    })?;
    Ok(StatSnapshot {
        entries,
        repo_root: repo_root.to_path_buf(),
    })
}

/// Stop after the first modification, creation, or deletion. Never read contents
/// and never fall back to Git status when evidence is missing.
pub fn has_changes(pre: &StatSnapshot) -> Result<bool, GitAiError> {
    let started = Instant::now();
    for (path, before) in &pre.entries {
        if started.elapsed() >= WALK_TIMEOUT {
            return Err(GitAiError::Generic(
                "shell metadata comparison exceeded time budget".into(),
            ));
        }
        match fs::symlink_metadata(pre.repo_root.join(path)) {
            Ok(metadata) if *before != StatEntry::from_metadata(&metadata) => return Ok(true),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
            Err(error) => return Err(error.into()),
        }
    }
    walk_files(&pre.repo_root, |path, entry| {
        pre.entries.get(&path) != Some(&entry)
    })
}
