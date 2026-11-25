//! The `pull` operation: the safety net for webhooks that never arrived.
//!
//! Fakturownia has no "changed since" filter, so the operation pages clients
//! by `updated_at` descending and stops at the watermark the previous run
//! stored. The first run only sets the watermark; linking existing clients is
//! a separate step.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use attricat_fakturownia_sync_core::timestamp_millis;

use crate::attricat::host::clock;
use crate::attricat::host::outcomes::{Failure, Retry};
use crate::exports::attricat::host::deferrable_operations::{BatchOutcome, OperationRequest};
use crate::host::{self, Http, decode, encode};
use crate::sync;

pub const OPERATION: &str = "pull";
const WATERMARK_KEY: &str = "pull.clients.watermark";
/// Clients per page; Fakturownia allows at most 100.
const PAGE_SIZE: usize = 25;
const STATUS_OK: u16 = 200;
const RETRY_AFTER_MS: u64 = 5_000;

#[derive(Default, Deserialize, Serialize)]
struct Checkpoint {
    page: u32,
    /// The newest `updated_at` seen in this run, in epoch milliseconds.
    newest: Option<i64>,
    applied: u32,
}

fn retry(message: &str, after_ms: u64) -> Failure {
    Failure::Retry(Retry {
        message: message.into(),
        after_ms: Some(after_ms),
    })
}

pub fn prepare(_request: &OperationRequest) -> Result<String, String> {
    host::config()?;
    Ok("prepared".into())
}

pub fn process_batch(request: &OperationRequest) -> Result<BatchOutcome, Failure> {
    let config = host::config()?;
    let mut checkpoint: Checkpoint = if request.checkpoint.is_empty() {
        Checkpoint::default()
    } else {
        decode(&request.checkpoint, "checkpoint").unwrap_or_default()
    };
    let watermark = host::storage_get(WATERMARK_KEY)?.and_then(|value| value.as_i64());
    let page = checkpoint.page + 1;
    let path = format!("/clients.json?order=updated_at.desc&page={page}&per_page={PAGE_SIZE}");
    let clients = match host::fakturownia(&config, "GET", &path, None)? {
        Http::Response { status, body } if status == STATUS_OK => {
            let clients: Value = decode(&body, "client list")?;
            clients.as_array().cloned().unwrap_or_default()
        }
        Http::Response { status, .. } => {
            return Err(retry(
                &format!("Fakturownia answered HTTP {status}"),
                RETRY_AFTER_MS,
            ));
        }
        Http::RateLimited(after_ms) => return Err(retry("request budget spent", after_ms)),
        _ => return Err(retry("Fakturownia did not answer", RETRY_AFTER_MS)),
    };
    let mut reached_watermark = clients.len() < PAGE_SIZE;
    for client in &clients {
        let Some(object) = client.as_object() else {
            continue;
        };
        let updated = object
            .get("updated_at")
            .and_then(Value::as_str)
            .and_then(timestamp_millis);
        checkpoint.newest = checkpoint.newest.max(updated);
        match (watermark, updated) {
            (None, _) => reached_watermark = true,
            (Some(watermark), Some(updated)) if updated <= watermark => {
                reached_watermark = true;
                break;
            }
            _ => {
                let applied = sync::apply_client(
                    &config,
                    object,
                    &sync::Mode::Run {
                        batch_key: &request.batch_key,
                    },
                )
                .inspect_err(|failure| {
                    host::log(
                        "warn",
                        &format!("pull could not apply a client: {failure:?}"),
                    );
                })?;
                if applied != sync::Applied::Unchanged {
                    checkpoint.applied += 1;
                }
            }
        }
    }
    checkpoint.page = page;
    if reached_watermark {
        // A first run over an empty account still starts the watermark.
        let now = clock::now().seconds as i64 * 1_000;
        let newest = checkpoint.newest.max(watermark).unwrap_or(now);
        host::storage_set(WATERMARK_KEY, &json!(newest))?;
    }
    let progress = json!({"pages": checkpoint.page, "applied": checkpoint.applied,
        "baseline": watermark.is_none()});
    Ok(BatchOutcome {
        checkpoint: encode(&checkpoint)?,
        progress: progress.to_string(),
        done: reached_watermark,
        resume_after_ms: None,
    })
}
