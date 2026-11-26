//! The `pull` operation: the safety net for webhooks that never arrived.
//!
//! Fakturownia has no "changed since" filter, so the operation pages clients,
//! then invoices when they are configured, by `updated_at` descending and
//! stops at the watermark each kind's previous run stored. The first run only
//! sets the watermarks; linking existing clients is a separate step. Deleted
//! invoices are not listed, so only their webhook clears them.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use attricat_fakturownia_sync_core::timestamp_millis;

use crate::attricat::host::clock;
use crate::attricat::host::outcomes::{Failure, Retry};
use crate::exports::attricat::host::deferrable_operations::{BatchOutcome, OperationRequest};
use crate::host::{self, Http, Mode, decode, encode};
use crate::{invoices, sync};

pub const OPERATION: &str = "pull";
/// Objects per page; Fakturownia allows at most 100.
const PAGE_SIZE: usize = 25;
const STATUS_OK: u16 = 200;
const RETRY_AFTER_MS: u64 = 5_000;

/// What a pull pages, in order.
#[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Phase {
    #[default]
    Clients,
    Invoices,
}

impl Phase {
    fn watermark_key(self) -> &'static str {
        match self {
            Phase::Clients => "pull.clients.watermark",
            Phase::Invoices => "pull.invoices.watermark",
        }
    }

    fn list_path(self, page: u32) -> String {
        match self {
            Phase::Clients => {
                format!("/clients.json?order=updated_at.desc&page={page}&per_page={PAGE_SIZE}")
            }
            Phase::Invoices => format!(
                "/invoices.json?period=all&order=updated_at.desc&page={page}&per_page={PAGE_SIZE}"
            ),
        }
    }
}

#[derive(Default, Deserialize, Serialize)]
struct Checkpoint {
    #[serde(default)]
    phase: Phase,
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

/// Refuses to start, with a JSON error, when the configured blueprint or context does not exist.
pub fn prepare(request: &OperationRequest) -> Result<String, String> {
    let config = host::config()?;
    let mode = Mode::Run {
        batch_key: &request.batch_key,
    };
    // The host keeps a run's error only when it is JSON; plain text is
    // redacted, which would hide why the run failed.
    let missing = |failure: Failure| {
        let (code, message) = match failure {
            Failure::Rejected(rejection) => (rejection.code, rejection.message),
            Failure::Retry(retry) => ("unavailable".to_owned(), retry.message),
            Failure::Fault(message) => ("fault".to_owned(), message),
        };
        json!({"code": code, "message": message}).to_string()
    };
    sync::blueprint(&config, &mode).map_err(missing)?;
    sync::context_id(&config, &mode).map_err(missing)?;
    if invoices::section(&config).is_some() {
        invoices::validated(&config, &mode).map_err(missing)?;
    }
    Ok("prepared".into())
}

pub fn process_batch(request: &OperationRequest) -> Result<BatchOutcome, Failure> {
    let config = host::config()?;
    let mut checkpoint: Checkpoint = if request.checkpoint.is_empty() {
        Checkpoint::default()
    } else {
        decode(&request.checkpoint, "checkpoint").unwrap_or_default()
    };
    let phase = checkpoint.phase;
    let mode = Mode::Run {
        batch_key: &request.batch_key,
    };
    let watermark = host::storage_get(phase.watermark_key())?.and_then(|value| value.as_i64());
    let page = checkpoint.page + 1;
    let objects = match host::fakturownia(&config, "GET", &phase.list_path(page), None)? {
        Http::Response { status, body } if status == STATUS_OK => {
            let objects: Value = decode(&body, "list")?;
            objects.as_array().cloned().unwrap_or_default()
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
    let mut reached_watermark = objects.len() < PAGE_SIZE;
    for object in &objects {
        let Some(object) = object.as_object() else {
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
                let changed = match phase {
                    Phase::Clients => {
                        sync::apply_client(&config, object, &mode)? != sync::Applied::Unchanged
                    }
                    Phase::Invoices => matches!(
                        invoices::apply_invoice(&config, object, &mode)?,
                        invoices::Applied::Updated | invoices::Applied::Linked
                    ),
                };
                if changed {
                    checkpoint.applied += 1;
                }
            }
        }
    }
    checkpoint.page = page;
    let mut done = false;
    if reached_watermark {
        // A first run over an empty account still starts the watermark.
        let now = clock::now().seconds as i64 * 1_000;
        let newest = checkpoint.newest.max(watermark).unwrap_or(now);
        host::storage_set(phase.watermark_key(), &json!(newest))?;
        if phase == Phase::Clients && invoices::section(&config).is_some() {
            checkpoint.phase = Phase::Invoices;
            checkpoint.page = 0;
            checkpoint.newest = None;
        } else {
            done = true;
        }
    }
    let progress = json!({"phase": checkpoint.phase, "pages": checkpoint.page,
        "applied": checkpoint.applied, "baseline": watermark.is_none()});
    Ok(BatchOutcome {
        checkpoint: encode(&checkpoint)?,
        progress: progress.to_string(),
        done,
        resume_after_ms: None,
    })
}
