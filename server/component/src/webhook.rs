//! The `fakturownia` inbound webhook (attricat:host 1.2).
//!
//! Fakturownia signs nothing: its body carries the changed object and an
//! `api_token` of the receiver's choosing, which is ignored. The endpoint's
//! unguessable URL is the credential, and a delivery is only a **hint**:
//! `receive` keeps the announced resource and ID, and the handler fetches the
//! object from the API before it changes anything. A forged request with a
//! leaked URL costs one GET at most.

use serde_json::{Value, json};

use attricat_fakturownia_sync_core::{Resource, parse_webhook};

use crate::attricat::host::api::Event;
use crate::attricat::host::outcomes::{Failure, Rejection};
use crate::exports::attricat::host::webhook_receiver::{
    KeptPayload, WebhookHeader, WebhookOutcome, WebhookReply, WebhookRequest,
};
use crate::host::{self, decode};
use crate::invoices;
use crate::sync::{self, Applied};

/// The manifest webhook this module answers.
pub const WEBHOOK: &str = "fakturownia";
pub const RECEIVED_EVENT: &str = "webhook.received.v1";

const STATUS_OK: u16 = 200;
const STATUS_BAD_REQUEST: u16 = 400;
const STATUS_NOT_FOUND: u16 = 404;
const STATUS_METHOD_NOT_ALLOWED: u16 = 405;

fn reply(status: u16, message: &str) -> WebhookOutcome {
    WebhookOutcome {
        reply: WebhookReply {
            status,
            headers: vec![WebhookHeader {
                name: "content-type".into(),
                value: "application/json".into(),
            }],
            body: json!({"status": message}).to_string(),
        },
        keep: None,
    }
}

pub fn receive(request: WebhookRequest) -> Result<WebhookOutcome, String> {
    if request.webhook != WEBHOOK {
        return Ok(reply(STATUS_NOT_FOUND, "unknown webhook"));
    }
    if request.method != "POST" {
        return Ok(reply(STATUS_METHOD_NOT_ALLOWED, "POST only"));
    }
    let Ok(body) = String::from_utf8(request.body) else {
        return Ok(reply(STATUS_BAD_REQUEST, "body is not UTF-8"));
    };
    let announcement = match parse_webhook(&body, &request.query) {
        Ok(announcement) => announcement,
        // Acknowledged so the sender does not retry a body we will never read.
        Err(reason) => {
            host::log(
                "warn",
                &format!("ignored webhook: {reason} ({} bytes)", body.len()),
            );
            return Ok(reply(STATUS_OK, "ignored"));
        }
    };
    let kept = json!({
        "resource": announcement.resource.name(),
        "action": announcement.action,
        "id": announcement.id,
    });
    let mut outcome = reply(STATUS_OK, "accepted");
    outcome.keep = Some(KeptPayload {
        payload: kept.to_string(),
        dedupe_key: Some(announcement.dedupe_key()),
    });
    Ok(outcome)
}

/// Fetches the announced object and applies it.
pub fn handle_received(event: &Event) -> Result<(), Failure> {
    let rejected = |code: &str, message: String| {
        Failure::Rejected(Rejection {
            code: code.into(),
            message,
            record_id: None,
            connection: None,
        })
    };
    let announced: Value = decode(&event.payload, "webhook event")
        .map_err(|error| rejected("invalid_payload", error))?;
    if announced["webhook"] != WEBHOOK {
        return Ok(());
    }
    let receipt = host::call(
        "webhooks.receipt.get.v1",
        &json!({"receipt_id": announced["receipt_id"]}),
    )?;
    let Some(payload) = receipt["payload"].as_str() else {
        return Err(rejected(
            "receipt_expired",
            "the webhook payload is no longer kept".into(),
        ));
    };
    let kept: Value = decode(payload, "kept webhook")?;
    let id = kept["id"]
        .as_i64()
        .ok_or_else(|| rejected("invalid_payload", "missing id".into()))?;
    let config = host::config()?;
    let mode = host::Mode::Handler;
    match kept["resource"].as_str() {
        Some(resource) if resource == Resource::Client.name() => {
            match sync::fetch_client(&config, id)? {
                Some(client) => {
                    let applied = sync::apply_client(&config, &client, &mode)?;
                    if applied != Applied::Unchanged {
                        host::log("info", &format!("applied client {id}: {applied:?}"));
                    }
                }
                None => host::log(
                    "info",
                    &format!("client {id} no longer exists in Fakturownia"),
                ),
            }
        }
        // Every invoice webhook, a delete too, is checked against the API.
        Some(resource) if resource == Resource::Invoice.name() => {
            match invoices::fetch_invoice(&config, &id.to_string())? {
                Some(invoice) => {
                    let applied = invoices::apply_invoice(&config, &invoice, &mode)?;
                    host::log("info", &format!("applied invoice {id}: {applied:?}"));
                }
                None => {
                    if invoices::deleted(&config, &id.to_string(), &mode)? {
                        host::log("info", &format!("invoice {id} was deleted in Fakturownia"));
                    }
                }
            }
        }
        other => host::log("info", &format!("webhook for {other:?} not synced")),
    }
    Ok(())
}
