use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApiErrorResponse {
    pub error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

pub const DAEMON_LOGS_UPLOAD_VERSION: u8 = 1;

/// Kind of daemon diagnostic event accepted by `/worker/logs/upload`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DaemonLogKind {
    Log,
    Heartbeat,
}

/// Log level accepted by `/worker/logs/upload`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DaemonLogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

/// Primitive daemon log field value.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum DaemonLogFieldValue {
    String(String),
    Number(serde_json::Number),
    Bool(bool),
    Null,
}

impl From<String> for DaemonLogFieldValue {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<&str> for DaemonLogFieldValue {
    fn from(value: &str) -> Self {
        Self::String(value.to_string())
    }
}

impl From<u64> for DaemonLogFieldValue {
    fn from(value: u64) -> Self {
        Self::Number(serde_json::Number::from(value))
    }
}

impl From<i64> for DaemonLogFieldValue {
    fn from(value: i64) -> Self {
        Self::Number(serde_json::Number::from(value))
    }
}

impl From<bool> for DaemonLogFieldValue {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

/// Single daemon diagnostic event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DaemonLogEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub kind: DaemonLogKind,
    pub timestamp: String,
    pub level: DaemonLogLevel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default)]
    pub message: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: BTreeMap<String, DaemonLogFieldValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_ai_version: Option<String>,
}

/// Request body for uploading daemon diagnostics to the HTTP backend.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DaemonLogsUploadRequest {
    pub version: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_ai_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_url: Option<String>,
    pub events: Vec<DaemonLogEvent>,
}

/// Error entry returned from daemon log upload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DaemonLogsUploadError {
    pub index: Option<usize>,
    pub error: String,
}

/// Response from daemon log upload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DaemonLogsUploadResponse {
    pub accepted: usize,
    pub dropped: usize,
    pub enqueued: bool,
    #[serde(default)]
    pub errors: Vec<DaemonLogsUploadError>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn daemon_logs_upload_request_serializes_contract_fields() {
        let mut fields = BTreeMap::new();
        fields.insert(
            "uptime_seconds".to_string(),
            DaemonLogFieldValue::from(900_u64),
        );
        fields.insert("healthy".to_string(), DaemonLogFieldValue::from(true));

        let request = DaemonLogsUploadRequest {
            version: DAEMON_LOGS_UPLOAD_VERSION,
            git_ai_version: Some("1.2.3".to_string()),
            daemon_id: Some("daemon-1".to_string()),
            install_id: Some("install-1".to_string()),
            repo_url: None,
            events: vec![DaemonLogEvent {
                id: Some("event-1".to_string()),
                kind: DaemonLogKind::Heartbeat,
                timestamp: "2026-06-26T12:00:00.000Z".to_string(),
                level: DaemonLogLevel::Info,
                target: Some("git_ai::daemon".to_string()),
                message: "alive".to_string(),
                fields,
                repo_url: None,
                git_ai_version: None,
            }],
        };

        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value["version"], 1);
        assert_eq!(value["git_ai_version"], "1.2.3");
        assert_eq!(value["daemon_id"], "daemon-1");
        assert_eq!(value["events"][0]["kind"], "heartbeat");
        assert_eq!(value["events"][0]["level"], "info");
        assert_eq!(value["events"][0]["fields"]["uptime_seconds"], 900);
        assert_eq!(value["events"][0]["fields"]["healthy"], true);
    }

    #[test]
    fn daemon_logs_upload_response_accepts_null_error_index() {
        let response: DaemonLogsUploadResponse = serde_json::from_str(
            r#"{"accepted":0,"dropped":0,"enqueued":false,"errors":[{"index":null,"error":"bad"}]}"#,
        )
        .unwrap();

        assert_eq!(response.errors[0].index, None);
    }
}
