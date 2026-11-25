//! Attricat ↔ Fakturownia client sync.
//!
//! One component on the `attricat:host@1.2.0` ABI (`webhook-extension` world):
//!
//! - `event-handler`: pushes changed client records to Fakturownia, and applies
//!   the clients Fakturownia webhooks announce ([`sync`], [`webhook`]).
//! - `webhook-receiver`: checks and keeps Fakturownia webhook requests
//!   ([`webhook`]).
//! - `deferrable-operations`: the scheduled `pull` safety net ([`pull`]).

wit_bindgen::generate!({
    path: "wit",
    world: "webhook-extension",
});

mod host;
mod pull;
mod sync;
mod webhook;

use attricat::host::api::Event;
use attricat::host::outcomes::{Failure, Retry};
use exports::attricat::host::{
    command_handler::{CommandRequest, CommandResponse},
    deferrable_operations::{BatchOutcome, OperationRequest},
    event_handler::Delivery,
    webhook_receiver::{WebhookOutcome, WebhookRequest},
};

/// Host call failures are temporary: the host retries the delivery or batch
/// with backoff instead of quarantining the installation.
impl From<String> for Failure {
    fn from(message: String) -> Self {
        Failure::Retry(Retry {
            message,
            after_ms: None,
        })
    }
}

const RECORD_EVENTS: [&str; 2] = ["record.created.v1", "record.updated.v1"];

struct Extension;

impl exports::attricat::host::event_handler::Guest for Extension {
    fn handle_event(event: Event, delivery: Delivery) -> Result<(), Failure> {
        if event.event_type == webhook::RECEIVED_EVENT {
            return webhook::handle_received(&event);
        }
        if !RECORD_EVENTS.contains(&event.event_type.as_str())
            || delivery.source_name == host::OWN_SOURCE
        {
            return Ok(());
        }
        let config = host::config()?;
        sync::push_record(&config, &event.aggregate_id, delivery.record_revision)
    }
}

impl exports::attricat::host::webhook_receiver::Guest for Extension {
    fn receive(request: WebhookRequest) -> Result<WebhookOutcome, String> {
        webhook::receive(request)
    }
}

impl exports::attricat::host::command_handler::Guest for Extension {
    fn handle_command(request: CommandRequest) -> Result<CommandResponse, String> {
        Err(format!("unknown command {}", request.handler))
    }
}

impl exports::attricat::host::deferrable_operations::Guest for Extension {
    fn prepare(request: OperationRequest) -> Result<String, String> {
        pull::prepare(&request)
    }

    fn start(_request: OperationRequest) -> Result<String, String> {
        Ok("started".into())
    }

    fn process_batch(request: OperationRequest) -> Result<BatchOutcome, Failure> {
        match request.operation_id.as_str() {
            pull::OPERATION => pull::process_batch(&request),
            other => Err(Failure::Fault(format!("unknown operation {other}"))),
        }
    }

    fn checkpoint(_request: OperationRequest) -> Result<(), String> {
        Ok(())
    }

    fn finish(_request: OperationRequest) -> Result<(), String> {
        Ok(())
    }

    fn cancel(_request: OperationRequest) -> Result<(), String> {
        Ok(())
    }
}

export!(Extension);
