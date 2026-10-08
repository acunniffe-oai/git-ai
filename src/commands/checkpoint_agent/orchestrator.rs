//! Translate native hook write evidence into repository-level session links.
use super::bash_tool;
use super::presets::{ParsedHookEvent, PresetContext};
use crate::commit_links::checkpoint::{CheckpointSignal, LinkCheckpoint};
use crate::commit_links::store::SessionIdentity;
use crate::error::GitAiError;
use crate::git::repo_state::worktree_root_for_path;
use crate::uuid::generate_trace_id;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Parsing and repository discovery never inspect file contents, HEAD, or a
/// transcript. Only the sequenced daemon decides whether a window is attached.
pub fn execute_preset_checkpoint(
    preset_name: &str,
    hook_input: &str,
) -> Result<Vec<LinkCheckpoint>, GitAiError> {
    let request_id = generate_trace_id();
    let events = super::presets::resolve_preset(preset_name)?.parse(hook_input, &request_id)?;
    let mut requests = Vec::new();
    for event in events {
        match event {
            ParsedHookEvent::PreFileEdit(_) => {}
            ParsedHookEvent::PostFileEdit(event) => {
                let Some(session) = identity(&event.context) else {
                    continue;
                };
                let roots = repositories(&event.file_paths, &event.context.cwd);
                for root in roots {
                    let state = crate::daemon::query_checkpoint_attachment(
                        &root.to_string_lossy(),
                        &session,
                    )?;
                    if state.attached {
                        continue;
                    }
                    requests.push(LinkCheckpoint {
                        request_id: generate_trace_id(),
                        repo_working_dir: root,
                        session: session.clone(),
                        generation: state.generation,
                        signal: CheckpointSignal::PostWrite,
                    });
                }
            }
            ParsedHookEvent::PreBashCall(event) => {
                let Some(session) = identity(&event.context) else {
                    continue;
                };
                let Some(root) = repository(&event.context.cwd) else {
                    continue;
                };
                let state =
                    crate::daemon::query_checkpoint_attachment(&root.to_string_lossy(), &session)?;
                let snapshot = if state.attached {
                    None
                } else {
                    bash_tool::snapshot(&root)
                        .map_err(|error| {
                            tracing::debug!("shell write evidence unavailable: {error}");
                            error
                        })
                        .ok()
                };
                requests.push(LinkCheckpoint {
                    request_id: generate_trace_id(),
                    repo_working_dir: root,
                    session,
                    generation: state.generation,
                    signal: CheckpointSignal::ShellStart {
                        invocation_id: event.tool_use_id,
                        snapshot,
                    },
                });
            }
            ParsedHookEvent::PostBashCall(event) => {
                let Some(session) = identity(&event.context) else {
                    continue;
                };
                let Some(root) = repository(&event.context.cwd) else {
                    continue;
                };
                let invocation = crate::daemon::query_checkpoint_invocation(
                    &root.to_string_lossy(),
                    &session,
                    &event.tool_use_id,
                )?;
                let Some(invocation) = invocation else {
                    continue;
                };
                // A warm invocation retains its contribution even if a commit
                // inside the shell call consumed the original pending set.
                let wrote = invocation.attached
                    || invocation
                        .snapshot
                        .as_ref()
                        .is_some_and(|before| bash_tool::has_changes(before).unwrap_or(false));
                requests.push(LinkCheckpoint {
                    request_id: generate_trace_id(),
                    repo_working_dir: root,
                    session,
                    generation: invocation.generation,
                    signal: CheckpointSignal::ShellEnd {
                        invocation_id: event.tool_use_id,
                        wrote,
                    },
                });
            }
        }
    }
    Ok(requests)
}

fn identity(context: &PresetContext) -> Option<SessionIdentity> {
    SessionIdentity::new(&context.agent, &context.external_session_id)
}

fn repository(path: &Path) -> Option<PathBuf> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    worktree_root_for_path(&path).and_then(|root| root.canonicalize().ok())
}

fn repositories(paths: &[PathBuf], cwd: &Path) -> BTreeSet<PathBuf> {
    if paths.is_empty() {
        return repository(cwd).into_iter().collect();
    }
    paths
        .iter()
        .filter_map(|path| {
            if path.is_absolute() {
                repository(path)
            } else {
                repository(&cwd.join(path))
            }
        })
        .collect()
}
