use super::db::{Outbox, Row, Status};
use super::{Source, endpoint, envelope, prepare};
use crate::config::Config;
use crate::error::GitAiError;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{Notify, mpsc, oneshot};

// Leave headroom under the collector's default 5 MiB body limit.
const MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;
const INTERVAL: Duration = Duration::from_secs(3);
static WAKE: Notify = Notify::const_new();
type FlushReply = oneshot::Sender<Result<(Status, bool), String>>;
static FLUSH: OnceLock<mpsc::UnboundedSender<FlushReply>> = OnceLock::new();

pub(super) fn wake() {
    WAKE.notify_one();
}

pub(crate) fn spawn_worker() {
    let (tx, mut rx) = mpsc::unbounded_channel::<FlushReply>();
    if FLUSH.set(tx).is_err() {
        return;
    }
    crate::tokio_runtime::telemetry_runtime().spawn(async move {
        loop {
            let reply = tokio::select! {
                request = rx.recv() => {
                    let Some(request) = request else { break; };
                    Some(request)
                }
                _ = WAKE.notified() => None,
                _ = tokio::time::sleep(INTERVAL) => None,
            };
            // Keep the dedicated pool lazy when no API key is configured.
            let result = if super::enabled() {
                crate::tokio_runtime::otel_runtime()
                    .spawn_blocking(flush_cycle)
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|r| r.map_err(|e| e.to_string()))
            } else {
                Ok((Status::default(), false))
            };
            if let Err(error) = &result {
                tracing::warn!(%error, "otel: flush failed");
            }
            if let Some(reply) = reply {
                let _ = reply.send(result);
            }
        }
    });
}

pub(crate) async fn flush_and_wait() -> Result<Status, GitAiError> {
    let Some(tx) = FLUSH.get() else {
        return Ok(Status::default());
    };
    loop {
        let (reply, rx) = oneshot::channel();
        tx.send(reply)
            .map_err(|_| GitAiError::Generic("OTLP worker stopped".into()))?;
        let (status, more_due) = rx
            .await
            .map_err(|e| GitAiError::Generic(e.to_string()))?
            .map_err(GitAiError::Generic)?;
        if !more_due {
            return Ok(status);
        }
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn credential(key: &str) -> String {
    format!("{:x}", Sha256::digest(key.as_bytes()))
}

fn flush_cycle() -> Result<(Status, bool), GitAiError> {
    let config = Config::fresh();
    let Some(endpoint) = endpoint(&config) else {
        return Ok((Status::default(), false));
    };
    let key = config.api_key().expect("endpoint requires API key");
    let mut db = Outbox::open()?;
    let fingerprint = credential(key);
    db.recover(&endpoint, &fingerprint, now())?;
    // Each source is an entire operation. Do not split its Git work per commit
    // or into fixed-size chunks: that would make process counts scale with N.
    for _ in 0..8 {
        let sources = db.claim(&endpoint, "source", 1, now())?;
        let Some(source) = sources.first() else {
            break;
        };
        let result = serde_json::from_str::<Source>(&source.payload)
            .map_err(GitAiError::from)
            .and_then(|s| prepare(s, &endpoint));
        match result {
            Ok(records) => db.prepared(source.id, &endpoint, &records)?,
            Err(error) => {
                tracing::warn!(%error, "otel: cannot prepare commit links");
                db.finish(&[source.id], "failed", &error.to_string(), "", now())?;
            }
        }
    }
    for _ in 0..16 {
        let rows = db.claim(&endpoint, "record", 100, now())?;
        if rows.is_empty() {
            break;
        }
        deliver(&mut db, &endpoint, key, &rows)?;
    }
    Ok((db.status(&endpoint)?, db.has_due(&endpoint, now())?))
}

fn deliver(db: &mut Outbox, endpoint: &str, key: &str, rows: &[Row]) -> Result<(), GitAiError> {
    let records: Vec<Value> = rows
        .iter()
        .map(|r| serde_json::from_str(&r.payload))
        .collect::<Result<_, _>>()?;
    let body = serde_json::to_string(&envelope(&records))?;
    let fingerprint = credential(key);
    let ids: Vec<_> = rows.iter().map(|r| r.id).collect();
    if body.len() > MAX_REQUEST_BYTES {
        return split_or_park(db, endpoint, key, rows);
    }
    // Reuse TLS/proxy settings and common request headers, but do not construct
    // an ApiContext: its OAuth and Git-identity discovery are irrelevant here.
    let (_agent, request) = crate::api::ApiContext::http_post(endpoint, Some(30));
    let response = crate::http::send_with_body(
        request
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {key}")),
        &body,
    );
    let (state, error) = match response {
        Ok(response) => match response.status_code {
            200 => ("delivered", String::new()),
            413 => return split_or_park(db, endpoint, key, rows),
            401 => ("unauthorized", "HTTP 401".into()),
            429 | 500..=599 => ("pending", format!("HTTP {}", response.status_code)),
            status => ("failed", format!("HTTP {status}")),
        },
        Err(error) => ("pending", error),
    };
    if state != "delivered" {
        tracing::warn!(state, %error, count=rows.len(), "otel: upload not delivered");
    }
    db.finish(&ids, state, &error, &fingerprint, now())
}

fn split_or_park(
    db: &mut Outbox,
    endpoint: &str,
    key: &str,
    rows: &[Row],
) -> Result<(), GitAiError> {
    if rows.len() <= 1 {
        return db.finish(
            &rows.iter().map(|r| r.id).collect::<Vec<_>>(),
            "failed",
            "OTLP record exceeds collector size limit",
            "",
            now(),
        );
    }
    let (left, right) = rows.split_at(rows.len() / 2);
    deliver(db, endpoint, key, left)?;
    deliver(db, endpoint, key, right)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn otel_transport_classifies_responses_and_retains_retry_payloads() {
        for (status, failed, due) in [
            (200, 0, false),
            (400, 1, false),
            (401, 1, false),
            (403, 1, false),
            (404, 1, false),
            (415, 1, false),
            (429, 0, true),
            (503, 0, true),
        ] {
            let mut server = mockito::Server::new();
            let mock = server
                .mock("POST", "/logs")
                .match_header("authorization", "Bearer secret")
                .match_header("content-type", "application/json")
                .with_status(status)
                .with_body("{}")
                .expect(1)
                .create();
            let dir = tempfile::tempdir().unwrap();
            let endpoint = format!("{}/logs", server.url());
            let mut db = Outbox::open_at(&dir.path().join("outbox")).unwrap();
            let record = r#"{"timeUnixNano":"1234567890123456789","body":{"stringValue":"git_ai.session.linked_commit"}}"#;
            db.prepared(-1, &endpoint, &[("identity".into(), record.into())])
                .unwrap();
            let rows = db.claim(&endpoint, "record", 1, now()).unwrap();
            deliver(&mut db, &endpoint, "secret", &rows).unwrap();
            mock.assert();
            let result = db.status(&endpoint).unwrap();
            assert_eq!(
                result.remaining,
                usize::from(status != 200),
                "status {status}"
            );
            assert_eq!(result.failed, failed, "status {status}");
            assert_eq!(
                db.has_due(&endpoint, now() + 10000).unwrap(),
                due,
                "status {status}"
            );
            if due {
                let retry = db.claim(&endpoint, "record", 1, now() + 10000).unwrap();
                assert_eq!(retry[0].payload, record);
            }
        }
    }

    #[test]
    fn otel_transport_splits_413_and_parks_an_oversized_single_record() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("POST", "/logs")
            .with_status(413)
            .with_body("{}")
            .expect(3)
            .create();
        let dir = tempfile::tempdir().unwrap();
        let endpoint = format!("{}/logs", server.url());
        let mut db = Outbox::open_at(&dir.path().join("outbox")).unwrap();
        db.prepared(
            -1,
            &endpoint,
            &[("one".into(), "{}".into()), ("two".into(), "{}".into())],
        )
        .unwrap();
        let rows = db.claim(&endpoint, "record", 100, now()).unwrap();
        deliver(&mut db, &endpoint, "secret", &rows).unwrap();
        mock.assert();
        assert_eq!(db.status(&endpoint).unwrap().failed, 2);
    }
}
