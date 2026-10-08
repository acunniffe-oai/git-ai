//! Content-free evidence submitted by native agent hooks.

use crate::commands::checkpoint_agent::bash_tool::StatSnapshot;
use crate::commit_links::store::SessionIdentity;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkCheckpoint {
    pub request_id: String,
    pub repo_working_dir: PathBuf,
    pub session: SessionIdentity,
    pub generation: i64,
    pub signal: CheckpointSignal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CheckpointSignal {
    PostWrite,
    ShellStart {
        invocation_id: String,
        snapshot: Option<StatSnapshot>,
    },
    ShellEnd {
        invocation_id: String,
        wrote: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellInvocationSnapshot {
    pub generation: i64,
    pub attached: bool,
    pub snapshot: Option<StatSnapshot>,
}
