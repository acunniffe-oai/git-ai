use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
pub enum ControlRequest {
    #[serde(rename = "ping")]
    Ping,
    #[serde(rename = "checkpoint.run")]
    CheckpointRun { body_bytes: u64 },
    #[serde(rename = "checkpoint.attachment")]
    CheckpointAttachment {
        repo_working_dir: String,
        session: crate::commit_links::store::SessionIdentity,
    },
    #[serde(rename = "checkpoint.invocation")]
    CheckpointInvocation {
        repo_working_dir: String,
        session: crate::commit_links::store::SessionIdentity,
        invocation_id: String,
    },
    #[serde(rename = "sync.family")]
    SyncFamily { repo_working_dir: String },
    #[serde(rename = "status.family")]
    StatusFamily { repo_working_dir: String },
    #[serde(rename = "telemetry.submit")]
    SubmitTelemetry { envelopes: Vec<TelemetryEnvelope> },
    /// Report ingest loss counters (dropped trace payloads/connections).
    #[serde(rename = "stats.ingest")]
    StatsIngest,
    /// Snapshot of the daemon's attribution pipeline: per-family pending work
    /// and fence state, open trace roots, and loss counters.
    #[serde(rename = "status.daemon")]
    StatusDaemon,
    /// Wait for the daemon to finish all in-flight work and flush telemetry.
    #[serde(rename = "await")]
    Await { timeout_secs: u64 },
    #[serde(rename = "shutdown")]
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ControlResponse {
    pub fn ok(seq: Option<u64>, data: Option<Value>) -> Self {
        Self {
            ok: true,
            seq,
            data,
            error: None,
        }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            seq: None,
            data: None,
            error: Some(msg.into()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FamilyStatus {
    pub family_key: String,
    pub latest_seq: u64,
    pub last_error: Option<String>,
}

/// A telemetry envelope sent from client to daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TelemetryEnvelope {
    Error {
        timestamp: String,
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        context: Option<Value>,
    },
    Performance {
        timestamp: String,
        operation: String,
        duration_ms: u128,
        #[serde(skip_serializing_if = "Option::is_none")]
        context: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tags: Option<std::collections::HashMap<String, String>>,
    },
    Message {
        timestamp: String,
        message: String,
        level: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        context: Option<Value>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_daemon_request_uses_stable_wire_shape() {
        assert_eq!(
            serde_json::to_value(ControlRequest::StatusDaemon).unwrap(),
            serde_json::json!({ "method": "status.daemon" })
        );
    }
}
