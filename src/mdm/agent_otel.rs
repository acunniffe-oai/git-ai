//! Install-time discovery of native agent telemetry settings. Never used on the
//! daemon ingestion path; the server owns all prompt/content capture policy.
use crate::{error::GitAiError, http};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, path::Path};

#[derive(Deserialize)]
pub(crate) struct ClientConfiguration {
    version: u32,
    #[serde(rename = "logLevel")]
    _log_level: LogLevel,
    agents: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum LogLevel {
    Off,
    Tools,
    Messages,
}

fn invalid(message: &str) -> GitAiError {
    GitAiError::Generic(message.to_owned())
}

impl ClientConfiguration {
    /// OAuth credentials must never be consulted here. A missing key also
    /// avoids URL parsing and transport creation entirely.
    pub(crate) fn fetch(base: &str, key: Option<&str>) -> Result<Option<Self>, GitAiError> {
        let Some(key) = key.filter(|key| !key.trim().is_empty()) else {
            return Ok(None);
        };
        let mut url = url::Url::parse(base).map_err(|_| invalid("invalid API base URL"))?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return Err(invalid("API base URL must use HTTP or HTTPS"));
        }
        let _ = url.set_username("");
        let _ = url.set_password(None);
        url.set_path("/api/client-config");
        url.set_query(None);
        url.set_fragment(None);
        // Reuse native TLS/proxy behavior, without ApiContext's OAuth refresh or
        // persistent distinct-ID creation (especially important for dry runs).
        let mut response = http::build_agent(Some(5))
            .get(url.as_str())
            .config()
            .max_redirects(0)
            .build()
            .header(
                "User-Agent",
                &format!("git-ai/{}", env!("CARGO_PKG_VERSION")),
            )
            .header("Accept", "application/json")
            .header("Authorization", &format!("Bearer {key}"))
            .call()
            .map_err(|_| invalid("client configuration request failed or timed out"))?;
        if response.status().as_u16() != 200 {
            return Err(invalid(&format!(
                "client configuration returned HTTP {}",
                response.status().as_u16()
            )));
        }
        let body = response
            .body_mut()
            .with_config()
            .limit(256 * 1024)
            .read_to_vec()
            .map_err(|_| invalid("could not read client configuration response"))?;
        let config: Self = serde_json::from_slice(&body)
            .map_err(|_| invalid("invalid client configuration response"))?;
        if config.version != 1 {
            return Err(invalid("unsupported client configuration version"));
        }
        Ok(Some(config))
    }

    pub(crate) fn for_installer(&self, id: &str) -> Result<Option<&Value>, GitAiError> {
        let key = match id {
            "codex" => "codex",
            "claude-code" => "claude",
            "github-copilot" => "copilot",
            _ => return Ok(None),
        };
        let Some(agent) = self.agents.get(key) else {
            return Ok(None);
        };
        match agent.get("enabled").and_then(Value::as_bool) {
            Some(true) => Ok(Some(agent)),
            Some(false) => Ok(None),
            None => Err(invalid("invalid agent enabled flag")),
        }
    }
}

// Deserialize only native OTEL settings, never arbitrary config/hooks from a
// remote response. Unknown fields remain forward-compatible within version 1.
#[derive(Deserialize, Serialize)]
struct CodexOtel {
    environment: String,
    log_user_prompt: bool,
    log_agent_responses: bool,
    log_guardian_assessments: bool,
    tool_result: ToolResult,
    exporter: HttpExporter,
    trace_exporter: HttpExporter,
}

#[derive(Deserialize, Serialize)]
struct ToolResult {
    max_bytes: u32,
}

#[derive(Deserialize, Serialize)]
struct HttpExporter {
    #[serde(rename = "otlp-http")]
    http: HttpSettings,
}

#[derive(Deserialize, Serialize)]
struct HttpSettings {
    endpoint: String,
    protocol: HttpProtocol,
    headers: BTreeMap<String, String>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum HttpProtocol {
    Json,
    Binary,
}

pub(crate) fn codex_otel(agent: &Value) -> Result<toml::Value, GitAiError> {
    let otel: CodexOtel = serde_json::from_value(agent.get("otel").cloned().unwrap_or(Value::Null))
        .map_err(|_| invalid("invalid Codex OTEL settings"))?;
    toml::Value::try_from(otel).map_err(|_| invalid("could not encode Codex OTEL settings"))
}

const CLAUDE_ENV_KEYS: &[&str] = &[
    "CLAUDE_CODE_ENABLE_TELEMETRY",
    "CLAUDE_CODE_ENHANCED_TELEMETRY_BETA",
    "OTEL_TRACES_EXPORTER",
    "OTEL_METRICS_EXPORTER",
    "OTEL_LOGS_EXPORTER",
    "OTEL_LOG_USER_PROMPTS",
    "OTEL_LOG_ASSISTANT_RESPONSES",
    "OTEL_LOG_TOOL_DETAILS",
    "OTEL_LOG_TOOL_CONTENT",
    "OTEL_LOG_RAW_API_BODIES",
    "OTEL_EXPORTER_OTLP_PROTOCOL",
    "OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE",
    "OTEL_EXPORTER_OTLP_ENDPOINT",
    "OTEL_EXPORTER_OTLP_HEADERS",
];

pub(crate) fn claude_env(agent: &Value) -> Result<serde_json::Map<String, Value>, GitAiError> {
    let env = agent
        .get("env")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("invalid Claude OTEL environment"))?;
    let mut settings = serde_json::Map::new();
    for &key in CLAUDE_ENV_KEYS {
        let value = env
            .get(key)
            .filter(|v| v.is_string())
            .ok_or_else(|| invalid("missing or invalid Claude OTEL environment setting"))?;
        settings.insert(key.to_owned(), value.clone());
    }
    Ok(settings)
}

/// Hook updates also display the whole agent file. Redact structured header
/// values before diffing so existing credentials are protected during rotation
/// and even when discovery is disabled or unavailable.
pub(crate) fn generate_config_diff(path: &Path, old: &str, new: &str) -> String {
    crate::mdm::utils::generate_diff(path, &redacted_config(old), &redacted_config(new))
}

fn redact(value: &mut Value, header_values: bool) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let key = key.to_ascii_lowercase();
                redact(
                    child,
                    header_values
                        || key == "headers"
                        || key == "authorization"
                        || key == "otelheadershelper"
                        || key.ends_with("_headers")
                        || key.ends_with(".headers"),
                );
            }
        }
        Value::Array(values) => {
            for child in values {
                redact(child, header_values);
            }
        }
        Value::String(_) if header_values => *value = Value::String("[REDACTED]".into()),
        _ => {}
    }
}

fn redacted_config(content: &str) -> String {
    if let Ok(Some(mut json)) =
        jsonc_parser::parse_to_serde_value::<Option<Value>>(content, &Default::default())
    {
        let before = json.clone();
        redact(&mut json, false);
        if before != json {
            return serde_json::to_string_pretty(&json).unwrap_or_default();
        }
    } else if let Ok(toml) = toml::from_str::<toml::Value>(content)
        && let Ok(mut json) = serde_json::to_value(&toml)
    {
        let before = json.clone();
        redact(&mut json, false);
        if before != json
            && let Ok(redacted) = toml::Value::try_from(json)
            && let Ok(text) = toml::to_string_pretty(&redacted)
        {
            return text;
        }
    }
    content.to_owned()
}
