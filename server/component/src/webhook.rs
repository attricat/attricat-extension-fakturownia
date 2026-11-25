//! The `fakturownia` inbound webhook (attricat:host 1.2).
//!
//! Fakturownia documents only its webhook management API, not the payload or
//! any signature, so a webhook is treated as a **hint**: `receive` keeps the
//! announced resource and ID, and the handler fetches the object from the API
//! before it changes anything. A forged request costs one GET at most.
//!
//! When `verify_webhook_token` is set, the body's `api_token` must equal the
//! configured API token (an assumption to verify against a real account).

use serde_json::{Value, json};

use attricat_fakturownia_sync_core::{Resource, constant_time_eq, parse_webhook};

use crate::attricat::host::api::Event;
use crate::attricat::host::outcomes::{Failure, Rejection};
use crate::exports::attricat::host::webhook_receiver::{
    KeptPayload, WebhookHeader, WebhookOutcome, WebhookReply, WebhookRequest,
};
use crate::host::{self, decode};
use crate::sync::{self, Applied};

/// The manifest webhook this module answers.
pub const WEBHOOK: &str = "fakturownia";
pub const RECEIVED_EVENT: &str = "webhook.received.v1";

const STATUS_OK: u16 = 200;
const STATUS_BAD_REQUEST: u16 = 400;
const STATUS_UNAUTHORIZED: u16 = 401;
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
    let config = host::config()?;
    let announcement = match parse_webhook(&body) {
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
    if config.verify_webhook_token {
        let token = host::secret(config.token_secret())?;
        let genuine = announcement
            .api_token
            .as_deref()
            .is_some_and(|sent| constant_time_eq(sent.as_bytes(), token.as_bytes()));
        if !genuine {
            return Ok(reply(STATUS_UNAUTHORIZED, "invalid token"));
        }
    }
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
    if kept["resource"] != Resource::Client.name() {
        host::log(
            "info",
            &format!("webhook for {} not synced yet", kept["resource"]),
        );
        return Ok(());
    }
    let id = kept["id"]
        .as_i64()
        .ok_or_else(|| rejected("invalid_payload", "missing id".into()))?;
    let config = host::config()?;
    match sync::fetch_client(&config, id)? {
        Some(client) => {
            let applied = sync::apply_client(&config, &client, &sync::Mode::Handler)?;
            if applied != Applied::Unchanged {
                host::log("info", &format!("applied client {id}: {applied:?}"));
            }
        }
        None => host::log(
            "info",
            &format!("client {id} no longer exists in Fakturownia"),
        ),
    }
    Ok(())
}
