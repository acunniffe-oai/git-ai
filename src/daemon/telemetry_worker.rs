//! Batched operational diagnostics, independent of durable OTEL commit links.
use crate::api::logs::daemon_logs_upload_allowed;
use crate::api::types::{
    DAEMON_LOGS_UPLOAD_VERSION, DaemonLogEvent, DaemonLogFieldValue, DaemonLogKind, DaemonLogLevel,
    DaemonLogsUploadRequest,
};
use crate::api::{ApiClient, ApiContext};
use crate::config::{Config, get_or_create_distinct_id};
use crate::daemon::control_api::TelemetryEnvelope;
use crate::error::GitAiError;
use crate::version::GIT_AI_VERSION;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant, sleep_until};

const FLUSH_INTERVAL: Duration = Duration::from_secs(3);
const DAEMON_LOG_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15 * 60);
const MAX_DAEMON_LOG_EVENTS_PER_UPLOAD: usize = 1000;
const MAX_DAEMON_LOG_BUFFER_EVENTS: usize = 5000;

static DAEMON_RUN_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
static DAEMON_LOG_UPLOAD_IN_FLIGHT: std::sync::OnceLock<Arc<AtomicBool>> =
    std::sync::OnceLock::new();

/// Diagnostic delivery remains best effort; durable counts belong to OTEL.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FlushStatus;

struct FlushRequest {
    completion: tokio::sync::oneshot::Sender<FlushStatus>,
}

/// Accumulated telemetry events waiting to be flushed.
struct TelemetryBuffer {
    errors: Vec<ErrorEvent>,
    performances: Vec<PerformanceEvent>,
    messages: Vec<MessageEvent>,
    daemon_logs: Vec<DaemonLogEvent>,
}

struct ErrorEvent {
    timestamp: String,
    message: String,
    context: Option<Value>,
}

struct PerformanceEvent {
    timestamp: String,
    operation: String,
    duration_ms: u128,
    context: Option<Value>,
    tags: Option<std::collections::HashMap<String, String>>,
}

struct MessageEvent {
    timestamp: String,
    message: String,
    level: String,
    context: Option<Value>,
}

impl TelemetryBuffer {
    fn new() -> Self {
        Self {
            errors: Vec::new(),
            performances: Vec::new(),
            messages: Vec::new(),
            daemon_logs: Vec::new(),
        }
    }

    fn is_empty(&self) -> bool {
        self.errors.is_empty()
            && self.performances.is_empty()
            && self.messages.is_empty()
            && self.daemon_logs.is_empty()
    }

    fn ingest_envelopes(&mut self, envelopes: Vec<TelemetryEnvelope>) {
        for envelope in envelopes {
            match envelope {
                TelemetryEnvelope::Error {
                    timestamp,
                    message,
                    context,
                } => {
                    self.errors.push(ErrorEvent {
                        timestamp,
                        message,
                        context,
                    });
                }
                TelemetryEnvelope::Performance {
                    timestamp,
                    operation,
                    duration_ms,
                    context,
                    tags,
                } => {
                    self.performances.push(PerformanceEvent {
                        timestamp,
                        operation,
                        duration_ms,
                        context,
                        tags,
                    });
                }
                TelemetryEnvelope::Message {
                    timestamp,
                    message,
                    level,
                    context,
                } => {
                    self.messages.push(MessageEvent {
                        timestamp,
                        message,
                        level,
                        context,
                    });
                }
            }
        }
    }

    fn ingest_daemon_logs(&mut self, events: Vec<DaemonLogEvent>) {
        self.daemon_logs.extend(events);
        self.cap_daemon_logs();
    }

    fn requeue_failed_daemon_logs(&mut self, mut failed_events: Vec<DaemonLogEvent>) {
        failed_events.append(&mut self.daemon_logs);
        self.daemon_logs = failed_events;
        self.cap_daemon_logs();
    }

    fn cap_daemon_logs(&mut self) {
        let overflow = self
            .daemon_logs
            .len()
            .saturating_sub(MAX_DAEMON_LOG_BUFFER_EVENTS);
        if overflow > 0 {
            self.daemon_logs.drain(0..overflow);
        }
    }

    fn take(&mut self) -> TelemetryBuffer {
        TelemetryBuffer {
            errors: std::mem::take(&mut self.errors),
            performances: std::mem::take(&mut self.performances),
            messages: std::mem::take(&mut self.messages),
            daemon_logs: std::mem::take(&mut self.daemon_logs),
        }
    }
}

#[derive(Clone)]
pub struct DaemonTelemetryWorkerHandle {
    buffer: Arc<Mutex<TelemetryBuffer>>,
    flush_tx: tokio::sync::mpsc::UnboundedSender<FlushRequest>,
}

impl DaemonTelemetryWorkerHandle {
    #[cfg(test)]
    pub fn new_noop() -> Self {
        let (flush_tx, _) = tokio::sync::mpsc::unbounded_channel();
        Self {
            buffer: Arc::new(Mutex::new(TelemetryBuffer::new())),
            flush_tx,
        }
    }

    pub async fn submit_telemetry(&self, envelopes: Vec<TelemetryEnvelope>) {
        self.buffer.lock().await.ingest_envelopes(envelopes);
    }

    pub fn submit_telemetry_sync(&self, envelopes: Vec<TelemetryEnvelope>) {
        if let Ok(mut buffer) = self.buffer.try_lock() {
            buffer.ingest_envelopes(envelopes);
        }
    }

    pub async fn submit_daemon_logs(&self, events: Vec<DaemonLogEvent>) {
        self.buffer.lock().await.ingest_daemon_logs(events);
    }

    pub fn submit_daemon_logs_sync(&self, events: Vec<DaemonLogEvent>) {
        if let Ok(mut buffer) = self.buffer.try_lock() {
            buffer.ingest_daemon_logs(events);
        }
    }

    pub async fn flush_and_wait(&self) -> Result<FlushStatus, GitAiError> {
        let (completion, reply) = tokio::sync::oneshot::channel();
        self.flush_tx
            .send(FlushRequest { completion })
            .map_err(|_| GitAiError::Generic("diagnostics worker stopped".into()))?;
        reply
            .await
            .map_err(|_| GitAiError::Generic("diagnostics flush cancelled".into()))
    }
}

/// Global handle for the daemon's in-process telemetry worker.
///
/// Set once when the daemon spawns its telemetry worker, allowing
/// `observability::log_*()` functions to route events directly into
/// the worker buffer when running inside the daemon process.
static DAEMON_INTERNAL_TELEMETRY: std::sync::OnceLock<DaemonTelemetryWorkerHandle> =
    std::sync::OnceLock::new();

/// Register the daemon's in-process telemetry worker handle.
/// Called once during daemon startup after `spawn_telemetry_worker()`.
pub fn set_daemon_internal_telemetry(handle: DaemonTelemetryWorkerHandle) {
    let _ = DAEMON_INTERNAL_TELEMETRY.set(handle);
}

/// Supplies the pipeline-health fields folded into each heartbeat.
pub type HeartbeatFieldsProvider =
    Arc<dyn Fn() -> BTreeMap<String, DaemonLogFieldValue> + Send + Sync>;

/// Registered once at daemon startup by the coordinator, so the worker
/// carries no coordinator dependency and the 15-minute cadence stays here.
static DAEMON_HEARTBEAT_FIELDS_PROVIDER: std::sync::OnceLock<HeartbeatFieldsProvider> =
    std::sync::OnceLock::new();

pub fn set_daemon_heartbeat_fields_provider(provider: HeartbeatFieldsProvider) {
    let _ = DAEMON_HEARTBEAT_FIELDS_PROVIDER.set(provider);
}

fn daemon_heartbeat_health_fields() -> BTreeMap<String, DaemonLogFieldValue> {
    DAEMON_HEARTBEAT_FIELDS_PROVIDER
        .get()
        .map(|provider| provider())
        .unwrap_or_default()
}

/// Submit telemetry from within the daemon process.
/// Returns true if the handle was available and envelopes were submitted.
pub fn submit_daemon_internal_telemetry(envelopes: Vec<TelemetryEnvelope>) -> bool {
    if let Some(handle) = DAEMON_INTERNAL_TELEMETRY.get() {
        submit_daemon_internal_telemetry_with_handle(handle.clone(), envelopes);
        true
    } else {
        false
    }
}

fn submit_daemon_internal_telemetry_with_handle(
    handle: DaemonTelemetryWorkerHandle,
    envelopes: Vec<TelemetryEnvelope>,
) {
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime.spawn(async move {
            handle.submit_telemetry(envelopes).await;
        });
    } else {
        handle.submit_telemetry_sync(envelopes);
    }
}

/// Submit daemon diagnostic events from within the daemon process.
/// Returns true if the handle was available and events were submitted.
pub fn submit_daemon_internal_daemon_logs(events: Vec<DaemonLogEvent>) -> bool {
    if let Some(handle) = DAEMON_INTERNAL_TELEMETRY.get() {
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let handle = handle.clone();
            runtime.spawn(async move {
                handle.submit_daemon_logs(events).await;
            });
        } else {
            handle.submit_daemon_logs_sync(events);
        }
        true
    } else {
        false
    }
}

/// Uses only the dedicated telemetry runtime; no diagnostic upload can occupy
/// a trace-ingestion or repository sequencing worker.
pub fn spawn_telemetry_worker() -> DaemonTelemetryWorkerHandle {
    crate::otel::spawn_worker();
    let buffer = Arc::new(Mutex::new(TelemetryBuffer::new()));
    let (flush_tx, mut flush_rx) = tokio::sync::mpsc::unbounded_channel::<FlushRequest>();
    let handle = DaemonTelemetryWorkerHandle {
        buffer: buffer.clone(),
        flush_tx,
    };
    let daemon_id = daemon_run_id().to_string();
    crate::tokio_runtime::telemetry_runtime().spawn(async move {
        let started = std::time::Instant::now();
        let mut heartbeat_at = started + DAEMON_LOG_HEARTBEAT_INTERVAL;
        loop {
            let request = tokio::select! {
                _ = sleep_until(next_telemetry_flush_at(Instant::now())) => None,
                request = flush_rx.recv() => { let Some(request) = request else { break }; Some(request) }
            };
            let batch = {
                let mut buffer = buffer.lock().await;
                let now = std::time::Instant::now();
                if now >= heartbeat_at {
                    heartbeat_at = now + DAEMON_LOG_HEARTBEAT_INTERVAL;
                    if daemon_log_upload_enabled() {
                        buffer.ingest_daemon_logs(vec![daemon_heartbeat_event(started.elapsed(), daemon_heartbeat_health_fields())]);
                    }
                }
                if buffer.is_empty() { None } else { Some(buffer.take()) }
            };
            if let Some(batch) = batch {
                let daemon_id = daemon_id.clone();
                let result = tokio::task::spawn_blocking(move || {
                    #[cfg(feature = "test-support")]
                    maybe_stall_telemetry_upload_for_test();
                    flush_telemetry_batch(batch, &daemon_id)
                }).await;
                match result {
                    Ok(requeue) if !requeue.is_empty() => buffer.lock().await.requeue_failed_daemon_logs(requeue),
                    Err(error) => tracing::error!(%error, "diagnostics flush task panicked"),
                    _ => {},
                }
            }
            if let Some(request) = request { let _ = request.completion.send(FlushStatus); }
        }
    });
    handle
}

fn next_telemetry_flush_at(completed_at: Instant) -> Instant {
    completed_at + FLUSH_INTERVAL
}

/// Test hook: simulate a hung upload backend inside the flush cycle.
#[cfg(feature = "test-support")]
fn maybe_stall_telemetry_upload_for_test() {
    if let Ok(raw_stall_ms) = std::env::var("GIT_AI_TEST_TELEMETRY_UPLOAD_STALL_MS")
        && let Ok(stall_ms) = raw_stall_ms.parse::<u64>()
        && stall_ms > 0
    {
        tracing::warn!("test telemetry upload stall engaged");
        std::thread::sleep(std::time::Duration::from_millis(stall_ms));
    }
}

fn flush_telemetry_batch(batch: TelemetryBuffer, daemon_id: &str) -> Vec<DaemonLogEvent> {
    let config = Config::get();
    let distinct_id = get_or_create_distinct_id();
    if !batch.errors.is_empty() || !batch.performances.is_empty() || !batch.messages.is_empty() {
        flush_sentry_and_posthog(
            config,
            &distinct_id,
            &batch.errors,
            &batch.performances,
            &batch.messages,
        );
    }
    dispatch_daemon_log_upload(batch.daemon_logs, daemon_id, &distinct_id)
}

fn daemon_log_upload_enabled() -> bool {
    Config::fresh().get_feature_flags().daemon_log_upload
}

fn daemon_run_id() -> &'static str {
    DAEMON_RUN_ID.get_or_init(crate::uuid::generate_v4).as_str()
}

/// Builds the heartbeat from the daemon's health `fields`; the contract fields
/// (`uptime_seconds`, `os`, `arch`) are set last and always win.
fn daemon_heartbeat_event(
    uptime: std::time::Duration,
    mut fields: BTreeMap<String, DaemonLogFieldValue>,
) -> DaemonLogEvent {
    fields.insert(
        "uptime_seconds".to_string(),
        DaemonLogFieldValue::from(uptime.as_secs()),
    );
    fields.insert(
        "os".to_string(),
        DaemonLogFieldValue::from(std::env::consts::OS),
    );
    fields.insert(
        "arch".to_string(),
        DaemonLogFieldValue::from(std::env::consts::ARCH),
    );

    DaemonLogEvent {
        id: Some(crate::uuid::generate_v4()),
        kind: DaemonLogKind::Heartbeat,
        timestamp: chrono::Utc::now().to_rfc3339(),
        level: DaemonLogLevel::Info,
        target: Some("git_ai::daemon".to_string()),
        message: "alive".to_string(),
        fields,
        repo_url: None,
        git_ai_version: None,
    }
}

fn daemon_log_upload_in_flight_flag() -> Arc<AtomicBool> {
    DAEMON_LOG_UPLOAD_IN_FLIGHT
        .get_or_init(|| Arc::new(AtomicBool::new(false)))
        .clone()
}

struct DaemonLogUploadInFlightGuard {
    in_flight: Arc<AtomicBool>,
}

impl Drop for DaemonLogUploadInFlightGuard {
    fn drop(&mut self) {
        self.in_flight.store(false, Ordering::Release);
    }
}

fn dispatch_daemon_log_upload(
    events: Vec<DaemonLogEvent>,
    daemon_id: &str,
    install_id: &str,
) -> Vec<DaemonLogEvent> {
    let daemon_id = daemon_id.to_string();
    let install_id = install_id.to_string();

    dispatch_daemon_log_upload_with(events, daemon_log_upload_in_flight_flag(), move |events| {
        let failed_events = flush_daemon_logs(events, &daemon_id, &install_id);
        if failed_events > 0 {
            tracing::debug!(
                failed_events,
                "daemon log upload failed after fire-and-forget dispatch"
            );
        }
    })
}

fn dispatch_daemon_log_upload_with<Upload>(
    events: Vec<DaemonLogEvent>,
    in_flight: Arc<AtomicBool>,
    upload: Upload,
) -> Vec<DaemonLogEvent>
where
    Upload: FnOnce(Vec<DaemonLogEvent>) + Send + 'static,
{
    if events.is_empty() {
        return Vec::new();
    }

    if in_flight
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return events;
    }

    let in_flight_for_task = in_flight.clone();
    let spawn_result = std::thread::Builder::new()
        .name("git-ai-daemon-log-upload".to_string())
        .spawn(move || {
            let _guard = DaemonLogUploadInFlightGuard {
                in_flight: in_flight_for_task,
            };
            upload(events);
        });

    if let Err(error) = spawn_result {
        in_flight.store(false, Ordering::Release);
        tracing::debug!(%error, "failed to spawn daemon log upload task");
    }

    Vec::new()
}

fn flush_daemon_logs(events: Vec<DaemonLogEvent>, daemon_id: &str, install_id: &str) -> usize {
    if !daemon_log_upload_enabled() {
        return 0;
    }

    let context = ApiContext::new(None);
    let api_base_url = context.base_url.clone();
    let client = ApiClient::new(context);

    if !daemon_logs_upload_allowed(&api_base_url, &client) {
        // These diagnostics are intentionally best-effort and only live in memory.
        // If the current API/auth setup cannot upload, do not keep re-flushing the
        // same buffered events every few seconds.
        return 0;
    }

    upload_daemon_log_chunk(events, daemon_id, install_id, |request| {
        client.upload_daemon_logs(request).map(|_| ())
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EmergencyLogUploadStatus {
    Completed,
    TimedOut,
    ThreadUnavailable,
}

/// Give one emergency diagnostic a bounded chance to reach the log endpoint.
///
/// Normal daemon-log delivery is deliberately fire-and-forget. Memory-limit
/// shutdown cannot rely on that worker surviving process exit, so this path
/// waits briefly for its own upload without allowing a stuck network request to
/// keep an unhealthy daemon alive.
pub(super) fn upload_emergency_daemon_log(
    event: DaemonLogEvent,
    timeout: std::time::Duration,
) -> EmergencyLogUploadStatus {
    upload_emergency_daemon_log_with(event, timeout, |event| {
        let install_id = get_or_create_distinct_id();
        let _ = flush_daemon_logs(vec![event], daemon_run_id(), &install_id);
    })
}

fn upload_emergency_daemon_log_with<Upload>(
    event: DaemonLogEvent,
    timeout: std::time::Duration,
    upload: Upload,
) -> EmergencyLogUploadStatus
where
    Upload: FnOnce(DaemonLogEvent) + Send + 'static,
{
    let (completion_tx, completion_rx) = std::sync::mpsc::sync_channel(0);
    let spawn_result = std::thread::Builder::new()
        .name("git-ai-emergency-log-upload".to_string())
        .spawn(move || {
            upload(event);
            let _ = completion_tx.send(());
        });

    if spawn_result.is_err() {
        return EmergencyLogUploadStatus::ThreadUnavailable;
    }

    match completion_rx.recv_timeout(timeout) {
        Ok(()) => EmergencyLogUploadStatus::Completed,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => EmergencyLogUploadStatus::TimedOut,
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            EmergencyLogUploadStatus::ThreadUnavailable
        }
    }
}

fn upload_daemon_log_chunk<Upload>(
    events: Vec<DaemonLogEvent>,
    daemon_id: &str,
    install_id: &str,
    mut upload: Upload,
) -> usize
where
    Upload: FnMut(&DaemonLogsUploadRequest) -> Result<(), GitAiError>,
{
    let Some(chunk) = events.chunks(MAX_DAEMON_LOG_EVENTS_PER_UPLOAD).next() else {
        return 0;
    };

    let mut failed_events = events.len().saturating_sub(chunk.len());
    let request = DaemonLogsUploadRequest {
        version: DAEMON_LOGS_UPLOAD_VERSION,
        git_ai_version: Some(GIT_AI_VERSION.to_string()),
        daemon_id: Some(daemon_id.to_string()),
        install_id: Some(install_id.to_string()),
        repo_url: None,
        events: chunk.to_vec(),
    };

    if upload(&request).is_err() {
        failed_events += chunk.len();
    }

    failed_events
}

fn flush_sentry_and_posthog(
    config: &Config,
    distinct_id: &str,
    errors: &[ErrorEvent],
    performances: &[PerformanceEvent],
    messages: &[MessageEvent],
) {
    // Check for Enterprise DSN
    let enterprise_dsn = config
        .telemetry_enterprise_dsn()
        .map(|s| s.to_string())
        .or_else(|| {
            std::env::var("SENTRY_ENTERPRISE")
                .ok()
                .or_else(|| option_env!("SENTRY_ENTERPRISE").map(|s| s.to_string()))
                .filter(|s| !s.is_empty())
        });

    // Check for OSS DSN
    let oss_dsn = if config.is_telemetry_oss_disabled() {
        None
    } else {
        std::env::var("SENTRY_OSS")
            .ok()
            .or_else(|| option_env!("SENTRY_OSS").map(|s| s.to_string()))
            .filter(|s| !s.is_empty())
    };

    // Check for PostHog configuration
    let posthog_api_key = if config.is_telemetry_oss_disabled() {
        None
    } else {
        std::env::var("POSTHOG_API_KEY")
            .ok()
            .or_else(|| option_env!("POSTHOG_API_KEY").map(|s| s.to_string()))
            .filter(|s| !s.is_empty())
    };

    let posthog_host = std::env::var("POSTHOG_HOST")
        .ok()
        .or_else(|| option_env!("POSTHOG_HOST").map(|s| s.to_string()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "https://us.i.posthog.com".to_string());

    // Build Sentry clients
    let oss_client = oss_dsn.and_then(|dsn| SentryClient::from_dsn(&dsn));
    let enterprise_client = enterprise_dsn.and_then(|dsn| SentryClient::from_dsn(&dsn));

    // Build base tags
    let mut base_tags = BTreeMap::new();
    base_tags.insert("os".to_string(), json!(std::env::consts::OS));
    base_tags.insert("arch".to_string(), json!(std::env::consts::ARCH));
    base_tags.insert("distinct_id".to_string(), json!(distinct_id));

    // Send errors
    for error in errors {
        let mut extra = BTreeMap::new();
        if let Some(ctx) = &error.context
            && let Some(obj) = ctx.as_object()
        {
            for (key, value) in obj {
                extra.insert(key.clone(), value.clone());
            }
        }

        let event = json!({
            "message": error.message,
            "level": "error",
            "timestamp": error.timestamp,
            "platform": "other",
            "tags": base_tags,
            "extra": extra,
            "release": format!("git-ai@{}", env!("CARGO_PKG_VERSION")),
        });

        if let Some(client) = &oss_client {
            let _ = client.send_event(event.clone());
        }
        if let Some(client) = &enterprise_client {
            let _ = client.send_event(event);
        }
    }

    // Send performance events
    for perf in performances {
        let mut extra = BTreeMap::new();
        extra.insert("operation".to_string(), json!(perf.operation));
        extra.insert("duration_ms".to_string(), json!(perf.duration_ms));
        if let Some(ctx) = &perf.context
            && let Some(obj) = ctx.as_object()
        {
            for (key, value) in obj {
                extra.insert(key.clone(), value.clone());
            }
        }

        let mut perf_tags = base_tags.clone();
        if let Some(tags) = &perf.tags {
            for (key, value) in tags {
                perf_tags.insert(key.clone(), json!(value));
            }
        }

        let event = json!({
            "message": format!("Performance: {} ({}ms)", perf.operation, perf.duration_ms),
            "level": "info",
            "timestamp": perf.timestamp,
            "platform": "other",
            "tags": perf_tags,
            "extra": extra,
            "release": format!("git-ai@{}", env!("CARGO_PKG_VERSION")),
        });

        if let Some(client) = &oss_client {
            let _ = client.send_event(event.clone());
        }
        if let Some(client) = &enterprise_client {
            let _ = client.send_event(event);
        }
    }

    // Send messages (to Sentry + PostHog)
    for msg in messages {
        let mut extra = BTreeMap::new();
        if let Some(ctx) = &msg.context
            && let Some(obj) = ctx.as_object()
        {
            for (key, value) in obj {
                extra.insert(key.clone(), value.clone());
            }
        }

        let sentry_event = json!({
            "message": msg.message,
            "level": msg.level,
            "timestamp": msg.timestamp,
            "platform": "other",
            "tags": base_tags,
            "extra": extra,
            "release": format!("git-ai@{}", env!("CARGO_PKG_VERSION")),
        });

        if let Some(client) = &oss_client {
            let _ = client.send_event(sentry_event.clone());
        }
        if let Some(client) = &enterprise_client {
            let _ = client.send_event(sentry_event);
        }

        // PostHog only gets messages
        if let Some(api_key) = &posthog_api_key {
            let mut properties = BTreeMap::new();
            properties.insert("os".to_string(), json!(std::env::consts::OS));
            properties.insert("arch".to_string(), json!(std::env::consts::ARCH));
            properties.insert("version".to_string(), json!(env!("CARGO_PKG_VERSION")));
            properties.insert("message".to_string(), json!(msg.message));
            properties.insert("level".to_string(), json!(msg.level));
            if let Some(ctx) = &msg.context
                && let Some(obj) = ctx.as_object()
            {
                for (key, value) in obj {
                    properties.insert(key.clone(), value.clone());
                }
            }

            let endpoint = format!("{}/capture/", posthog_host.trim_end_matches('/'));
            let mut ph_event = json!({
                "api_key": api_key,
                "event": msg.message,
                "properties": properties,
                "distinct_id": distinct_id,
            });
            ph_event["timestamp"] = json!(msg.timestamp);

            let agent = crate::http::build_agent(Some(30));
            let request = agent
                .post(&endpoint)
                .header("Content-Type", "application/json");
            let _ = crate::http::send_with_body(
                request,
                &serde_json::to_string(&ph_event).unwrap_or_default(),
            );
        }
    }
}

struct SentryClient {
    endpoint: String,
    public_key: String,
}

impl SentryClient {
    fn from_dsn(dsn: &str) -> Option<Self> {
        let url = url::Url::parse(dsn).ok()?;
        let public_key = url.username().to_string();
        let host = url.host_str()?;
        let project_id = url.path().trim_start_matches('/');
        let scheme = url.scheme();
        let endpoint = format!("{}://{}/api/{}/store/", scheme, host, project_id);
        Some(SentryClient {
            endpoint,
            public_key,
        })
    }

    fn send_event(&self, event: Value) -> Result<(), Box<dyn std::error::Error>> {
        let auth_header = format!(
            "Sentry sentry_version=7, sentry_key={}, sentry_client=git-ai/{}",
            self.public_key,
            env!("CARGO_PKG_VERSION")
        );

        let body = serde_json::to_string(&event)?;
        let agent = crate::http::build_agent(Some(30));
        let request = agent
            .post(&self.endpoint)
            .header("X-Sentry-Auth", &auth_header)
            .header("Content-Type", "application/json");
        let response = crate::http::send_with_body(request, &body)?;

        let status = response.status_code;
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(format!("Sentry returned status {}", status).into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;
    fn test_message_envelope(message: &str) -> TelemetryEnvelope {
        TelemetryEnvelope::Message {
            timestamp: chrono::Utc::now().to_rfc3339(),
            message: message.to_string(),
            level: "info".to_string(),
            context: None,
        }
    }

    #[tokio::test]
    async fn submit_daemon_internal_telemetry_spawns_when_runtime_exists() {
        let handle = DaemonTelemetryWorkerHandle::new_noop();
        let guard = handle.buffer.lock().await;

        submit_daemon_internal_telemetry_with_handle(
            handle.clone(),
            vec![test_message_envelope("runtime")],
        );

        assert!(guard.messages.is_empty());
        drop(guard);

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if handle.buffer.lock().await.messages.len() == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[test]
    fn submit_daemon_internal_telemetry_waits_without_runtime() {
        let handle = DaemonTelemetryWorkerHandle::new_noop();

        submit_daemon_internal_telemetry_with_handle(
            handle.clone(),
            vec![test_message_envelope("sync")],
        );

        let guard = handle.buffer.try_lock().unwrap();
        assert_eq!(guard.messages.len(), 1);
    }

    fn sample_daemon_log_event(message: impl Into<String>) -> DaemonLogEvent {
        DaemonLogEvent {
            id: Some(crate::uuid::generate_v4()),
            kind: DaemonLogKind::Log,
            timestamp: chrono::Utc::now().to_rfc3339(),
            level: DaemonLogLevel::Info,
            target: Some("git_ai::test".to_string()),
            message: message.into(),
            fields: BTreeMap::new(),
            repo_url: None,
            git_ai_version: None,
        }
    }

    #[test]
    fn upload_daemon_log_chunk_counts_events_past_per_upload_cap() {
        let events = (0..MAX_DAEMON_LOG_EVENTS_PER_UPLOAD + 2)
            .map(|index| sample_daemon_log_event(index.to_string()))
            .collect::<Vec<_>>();
        let uploaded_batch_sizes = Rc::new(RefCell::new(Vec::new()));

        let failed_events = upload_daemon_log_chunk(events, "daemon-id", "install-id", {
            let uploaded_batch_sizes = Rc::clone(&uploaded_batch_sizes);
            move |request| {
                uploaded_batch_sizes.borrow_mut().push(request.events.len());
                Ok(())
            }
        });

        assert_eq!(
            *uploaded_batch_sizes.borrow(),
            vec![MAX_DAEMON_LOG_EVENTS_PER_UPLOAD]
        );
        assert_eq!(failed_events, 2);
    }

    #[test]
    fn daemon_log_dispatch_requeues_when_upload_is_already_in_flight() {
        let in_flight = Arc::new(AtomicBool::new(true));
        let events = vec![sample_daemon_log_event("queued")];

        let retry_events = dispatch_daemon_log_upload_with(events, in_flight, |_events| {
            panic!("upload task should not run while another upload is running")
        });

        assert_eq!(retry_events.len(), 1);
        assert_eq!(retry_events[0].message, "queued");
    }

    #[test]
    fn daemon_log_dispatch_does_not_wait_for_upload_task() {
        let (upload_started_tx, upload_started_rx) = std::sync::mpsc::channel();
        let (release_upload_tx, release_upload_rx) = std::sync::mpsc::channel();
        let in_flight = Arc::new(AtomicBool::new(false));

        let started_at = std::time::Instant::now();
        let retry_events = dispatch_daemon_log_upload_with(
            vec![sample_daemon_log_event("blocked")],
            Arc::clone(&in_flight),
            move |_events| {
                upload_started_tx.send(()).unwrap();
                let _ = release_upload_rx.recv_timeout(Duration::from_secs(2));
            },
        );

        assert!(retry_events.is_empty());
        assert!(
            started_at.elapsed() < Duration::from_millis(500),
            "dispatch should return promptly while daemon log upload is blocked"
        );

        upload_started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("upload task should start");
        assert!(in_flight.load(Ordering::Acquire));

        release_upload_tx.send(()).unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while in_flight.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(!in_flight.load(Ordering::Acquire));
    }

    #[test]
    fn emergency_daemon_log_upload_waits_for_completion() {
        let (uploaded_tx, uploaded_rx) = std::sync::mpsc::channel();

        let status = upload_emergency_daemon_log_with(
            sample_daemon_log_event("memory emergency"),
            Duration::from_secs(1),
            move |event| uploaded_tx.send(event.message).unwrap(),
        );

        assert_eq!(status, EmergencyLogUploadStatus::Completed);
        assert_eq!(uploaded_rx.recv().unwrap(), "memory emergency");
    }

    #[test]
    fn emergency_daemon_log_upload_reuses_the_daemon_run_id() {
        let run_id = daemon_run_id().to_string();

        assert_eq!(daemon_run_id(), run_id);
    }

    #[test]
    fn emergency_daemon_log_upload_has_a_hard_wait_bound() {
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let started_at = std::time::Instant::now();

        let status = upload_emergency_daemon_log_with(
            sample_daemon_log_event("memory emergency"),
            Duration::from_millis(10),
            move |_event| {
                let _ = release_rx.recv_timeout(Duration::from_secs(1));
            },
        );

        assert_eq!(status, EmergencyLogUploadStatus::TimedOut);
        assert!(started_at.elapsed() < Duration::from_millis(500));
        release_tx.send(()).unwrap();
    }

    #[test]
    fn telemetry_buffer_caps_daemon_logs_to_latest_events() {
        let mut buffer = TelemetryBuffer::new();
        let total = MAX_DAEMON_LOG_BUFFER_EVENTS + 2;
        let events = (0..total)
            .map(|index| sample_daemon_log_event(index.to_string()))
            .collect();

        buffer.ingest_daemon_logs(events);

        assert_eq!(buffer.daemon_logs.len(), MAX_DAEMON_LOG_BUFFER_EVENTS);
        assert_eq!(buffer.daemon_logs.first().unwrap().message, "2");
        assert_eq!(
            buffer.daemon_logs.last().unwrap().message,
            (total - 1).to_string()
        );
    }

    #[test]
    fn telemetry_buffer_requeues_failed_daemon_logs_without_dropping_newer_events() {
        let mut buffer = TelemetryBuffer::new();
        buffer.ingest_daemon_logs(vec![
            sample_daemon_log_event("new-1"),
            sample_daemon_log_event("new-2"),
        ]);

        let failed_events = (0..MAX_DAEMON_LOG_BUFFER_EVENTS)
            .map(|index| sample_daemon_log_event(format!("old-{index}")))
            .collect();

        buffer.requeue_failed_daemon_logs(failed_events);

        assert_eq!(buffer.daemon_logs.len(), MAX_DAEMON_LOG_BUFFER_EVENTS);
        assert_eq!(buffer.daemon_logs.first().unwrap().message, "old-2");
        assert_eq!(
            buffer.daemon_logs[MAX_DAEMON_LOG_BUFFER_EVENTS - 2].message,
            "new-1"
        );
        assert_eq!(
            buffer.daemon_logs[MAX_DAEMON_LOG_BUFFER_EVENTS - 1].message,
            "new-2"
        );
    }

    #[test]
    fn daemon_heartbeat_event_uses_upload_contract_shape() {
        let event = daemon_heartbeat_event(std::time::Duration::from_secs(900), BTreeMap::new());

        assert!(event.id.is_some());
        assert_eq!(event.kind, DaemonLogKind::Heartbeat);
        assert_eq!(event.level, DaemonLogLevel::Info);
        assert_eq!(event.target.as_deref(), Some("git_ai::daemon"));
        assert_eq!(event.message, "alive");
        assert_eq!(
            event.fields.get("uptime_seconds"),
            Some(&DaemonLogFieldValue::from(900_u64))
        );
        assert!(event.fields.contains_key("os"));
        assert!(event.fields.contains_key("arch"));
    }

    #[test]
    fn daemon_heartbeat_event_merges_health_fields_and_keeps_contract_fields() {
        let mut health = BTreeMap::new();
        health.insert(
            "sequencer_stalled".to_string(),
            DaemonLogFieldValue::from(false),
        );
        health.insert(
            "trace_roots_open_mutating".to_string(),
            DaemonLogFieldValue::from(2_u64),
        );
        // A provider must not be able to shadow the contract fields.
        health.insert(
            "uptime_seconds".to_string(),
            DaemonLogFieldValue::from(1_u64),
        );

        let event = daemon_heartbeat_event(std::time::Duration::from_secs(900), health);

        assert_eq!(event.kind, DaemonLogKind::Heartbeat);
        assert_eq!(event.message, "alive");
        assert_eq!(
            event.fields.get("uptime_seconds"),
            Some(&DaemonLogFieldValue::from(900_u64))
        );
        assert_eq!(
            event.fields.get("sequencer_stalled"),
            Some(&DaemonLogFieldValue::from(false))
        );
        assert_eq!(
            event.fields.get("trace_roots_open_mutating"),
            Some(&DaemonLogFieldValue::from(2_u64))
        );
        assert!(event.fields.contains_key("os"));
        assert!(event.fields.contains_key("arch"));
    }
}
