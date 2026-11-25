//! Both directions of the client sync. Each side's changes reach the other
//! through the export mapping of the record, whose content hash is the
//! projection both sides last agreed on (see `attricat-fakturownia-sync-core`).

use serde_json::{Map, Value, json};

use attricat_fakturownia_sync_core::{
    CLIENT_ROOT, Config, local_projection, merge_inbound, outbound_client, projection_hash,
    validation_message,
};

use crate::attricat::host::outcomes::{Failure, Rejection, Retry};
use crate::host::{self, Begin, Http};

const STATUS_OK: u16 = 200;
const STATUS_CREATED: u16 = 201;
const STATUS_UNAUTHORIZED: u16 = 401;
const STATUS_FORBIDDEN: u16 = 403;
const STATUS_NOT_FOUND: u16 = 404;
const STATUS_UNPROCESSABLE: u16 = 422;
const STATUS_TOO_MANY_REQUESTS: u16 = 429;
/// Wait before retrying a throttled or failed Fakturownia request.
const RETRY_AFTER_MS: u64 = 5_000;
/// Mapping statuses (see the host's export mapping store).
const SYNCED: &str = "synced";
const UNCERTAIN: &str = "uncertain";
/// Bound of an intent key.
const MAX_INTENT_KEY_BYTES: usize = 128;

fn rejected(code: &str, message: String, record_id: Option<&str>) -> Failure {
    Failure::Rejected(Rejection {
        code: code.into(),
        message,
        record_id: record_id.map(str::to_owned),
        connection: None,
    })
}

fn retry(message: &str, after_ms: u64) -> Failure {
    Failure::Retry(Retry {
        message: message.into(),
        after_ms: Some(after_ms),
    })
}

/// Maps an unusable Fakturownia answer to the delivery's outcome.
fn failed_response(status: u16, body: &str, record_id: Option<&str>) -> Failure {
    match status {
        STATUS_UNAUTHORIZED | STATUS_FORBIDDEN => rejected(
            "fakturownia_unauthorized",
            "Fakturownia refused the API token".into(),
            record_id,
        ),
        STATUS_UNPROCESSABLE => {
            rejected("fakturownia_invalid", validation_message(body), record_id)
        }
        STATUS_TOO_MANY_REQUESTS => retry("Fakturownia throttled the request", RETRY_AFTER_MS),
        status if status >= 500 => retry("Fakturownia failed to answer", RETRY_AFTER_MS),
        status => rejected(
            "fakturownia_unexpected",
            format!("Fakturownia answered HTTP {status}"),
            record_id,
        ),
    }
}

fn client_id(body: &str) -> Option<String> {
    let client: Value = serde_json::from_str(body).ok()?;
    let id = &client["id"];
    id.as_i64()
        .map(|id| id.to_string())
        .or_else(|| id.as_str().map(str::to_owned))
}

/// Pushes a changed client record to Fakturownia.
pub fn push_record(config: &Config, record_id: &str, revision: Option<i64>) -> Result<(), Failure> {
    let record = host::read_record(record_id)?;
    if record.blueprint_id != config.clients.blueprint_id || record.deleted {
        return Ok(());
    }
    let values = host::resolved_values(record_id, &config.clients.context_id)?;
    let projection = local_projection(&config.clients, &values);
    let hash = projection_hash(&projection);
    let revision = revision.or(record.content_revision).unwrap_or_default();
    let (lease_token, mapping) = match host::begin(record_id, revision)? {
        Begin::Proceed {
            lease_token,
            mapping,
        } => (lease_token, mapping),
        Begin::Stale => return Ok(()),
        Begin::Busy => return Err(retry("another push holds the record", RETRY_AFTER_MS)),
    };
    let mut external_id = mapping["external_id"].as_str().map(str::to_owned);
    if external_id.is_some() && mapping["content_hash"].as_str() == Some(hash.as_str()) {
        host::commit(
            record_id,
            json!({"lease_token": lease_token, "status": SYNCED}),
        )?;
        return Ok(());
    }
    // A create whose answer was lost may have happened: find it by our ID.
    if external_id.is_none() && mapping["status"].as_str() == Some(UNCERTAIN) {
        external_id = find_by_external_id(config, record_id).inspect_err(|_| {
            host::release(record_id, &lease_token);
        })?;
    }
    let mut client = outbound_client(&config.clients, &projection, external_id.is_none());
    let (method, path) = match &external_id {
        Some(id) => ("PUT", format!("/clients/{id}.json")),
        None => {
            client.insert("external_id".into(), Value::String(record_id.into()));
            ("POST", "/clients.json".to_owned())
        }
    };
    let body = json!({ CLIENT_ROOT: client });
    let answer = host::fakturownia(config, method, &path, Some(&body)).inspect_err(|_| {
        host::release(record_id, &lease_token);
    })?;
    match answer {
        Http::Response { status, body } if status == STATUS_OK || status == STATUS_CREATED => {
            let id = client_id(&body).or(external_id);
            host::commit(
                record_id,
                json!({"lease_token": lease_token, "status": SYNCED, "external_id": id,
                    "content_hash": hash}),
            )?;
            host::log("info", &format!("pushed client record {record_id}"));
            Ok(())
        }
        Http::Response { status, .. } if status == STATUS_NOT_FOUND && external_id.is_some() => {
            Err(rejected(
                "fakturownia_client_missing",
                "the client was deleted in Fakturownia".into(),
                Some(record_id),
            ))
        }
        Http::Response { status, body } => {
            let failure = failed_response(status, &body, Some(record_id));
            if matches!(failure, Failure::Retry(_)) {
                host::release(record_id, &lease_token);
            }
            Err(failure)
        }
        Http::RateLimited(after_ms) => {
            host::release(record_id, &lease_token);
            Err(retry("request budget spent", after_ms))
        }
        Http::Unreachable => {
            host::release(record_id, &lease_token);
            Err(retry("Fakturownia is unreachable", RETRY_AFTER_MS))
        }
        Http::Uncertain => {
            host::commit(
                record_id,
                json!({"lease_token": lease_token, "status": UNCERTAIN}),
            )?;
            Err(retry("the push may have been applied", RETRY_AFTER_MS))
        }
    }
}

/// The Fakturownia client created for `record_id`, if any.
fn find_by_external_id(config: &Config, record_id: &str) -> Result<Option<String>, Failure> {
    match host::fakturownia(
        config,
        "GET",
        &format!("/clients.json?external_id={record_id}"),
        None,
    )? {
        Http::Response { status, body } if status == STATUS_OK => {
            let clients: Value = host::decode(&body, "client list")?;
            Ok(clients
                .as_array()
                .and_then(|clients| clients.first())
                .and_then(|client| client_id(&client.to_string())))
        }
        Http::Response { status, body } => Err(failed_response(status, &body, Some(record_id))),
        _ => Err(retry("could not look the client up", RETRY_AFTER_MS)),
    }
}

/// Fetches a client from Fakturownia; `None` when it no longer exists.
pub fn fetch_client(config: &Config, id: i64) -> Result<Option<Map<String, Value>>, Failure> {
    match host::fakturownia(config, "GET", &format!("/clients/{id}.json"), None)? {
        Http::Response { status, body } if status == STATUS_OK => {
            let client: Value = host::decode(&body, "client")?;
            Ok(client.as_object().cloned())
        }
        Http::Response { status, .. } if status == STATUS_NOT_FOUND => Ok(None),
        Http::Response { status, body } => Err(failed_response(status, &body, None)),
        Http::RateLimited(after_ms) => Err(retry("request budget spent", after_ms)),
        _ => Err(retry("Fakturownia did not answer", RETRY_AFTER_MS)),
    }
}

/// What applying a Fakturownia client did locally.
#[derive(Debug, PartialEq, Eq)]
pub enum Applied {
    Created(String),
    Updated(String),
    Unchanged,
}

/// Whether `text` looks like an Attricat record ID (Fakturownia's
/// `external_id` is free text and empty when no integration set it).
fn is_uuid(text: &str) -> bool {
    text.len() == 36
        && text.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

/// A stable intent key for one remote state of a client.
fn intent_key(action: &str, id: &str, updated_at: &str) -> String {
    let mut key = format!("{action}:{id}:{updated_at}");
    key.truncate(MAX_INTENT_KEY_BYTES);
    key
}

/// Where an inbound change is applied from.
pub enum Mode<'a> {
    /// An event handler: reads the record and writes only what changed.
    Handler,
    /// An operation run, which has no single-record read: it writes every
    /// field Fakturownia may change, under the run's batch key.
    Run { batch_key: &'a str },
}

impl Mode<'_> {
    fn command(&self, intent: Value) -> Result<Result<String, String>, String> {
        match self {
            Mode::Handler => host::command(intent, None),
            Mode::Run { batch_key } => host::command(intent, Some(batch_key)),
        }
    }
}

/// Applies a fetched Fakturownia client to its record, creating the record
/// when the client is new to Attricat.
pub fn apply_client(
    config: &Config,
    client: &Map<String, Value>,
    mode: &Mode,
) -> Result<Applied, Failure> {
    let id = client_id(&Value::Object(client.clone()).to_string())
        .ok_or_else(|| rejected("invalid_client", "client without id".into(), None))?;
    let updated_at = client
        .get("updated_at")
        .and_then(Value::as_str)
        .unwrap_or("-");
    let sync = &config.clients;
    let mapping = match host::mapping_by_external_id(&id)? {
        Some(mapping) => Some(mapping),
        // A client this extension created carries the record ID.
        None => match client.get("external_id").and_then(Value::as_str) {
            Some(record_id) if is_uuid(record_id) => host::mapping_by_record(record_id)?,
            _ => None,
        },
    };
    let Some(mapping) = mapping else {
        let remote = local_projection(sync, &remote_values(sync, client));
        let values: Vec<Value> = sync
            .fields
            .iter()
            .filter_map(|mapping| {
                let value = remote.get(&mapping.field)?;
                Some(host::scalar(
                    &mapping.attribute,
                    &sync.context_id,
                    Some(value),
                ))
            })
            .collect();
        let record_id = mode
            .command(json!({
                "kind": "create",
                "intent_key": intent_key("create", &id, updated_at),
                "blueprint_id": sync.blueprint_id,
                "blueprint_version": sync.blueprint_version,
                "values": values,
            }))?
            .map_err(|error| rejected("create_refused", error, None))?;
        host::commit(
            &record_id,
            json!({"status": SYNCED, "external_id": id, "content_hash": projection_hash(&remote)}),
        )?;
        return Ok(Applied::Created(record_id));
    };
    let record_id = mapping["record_id"]
        .as_str()
        .ok_or_else(|| rejected("invalid_mapping", "mapping without record".into(), None))?
        .to_owned();
    if let Mode::Run { .. } = mode {
        let remote = local_projection(sync, &remote_values(sync, client));
        let values: Vec<Value> = sync
            .fields
            .iter()
            .filter(|mapping| mapping.owner.pulls())
            .map(|mapping| {
                let value = remote.get(&mapping.field).map(String::as_str);
                host::scalar(&mapping.attribute, &sync.context_id, value)
            })
            .collect();
        mode.command(json!({
            "kind": "update",
            "intent_key": intent_key("update", &id, updated_at),
            "record_id": record_id,
            "values": values,
        }))?
        .map_err(|error| rejected("update_refused", error, Some(&record_id)))?;
        // The record's other fields are unknown here, so the agreed hash is
        // left as it was; the next local edit compares against it and pushes.
        host::commit(&record_id, json!({"status": SYNCED, "external_id": id}))?;
        return Ok(Applied::Updated(record_id));
    }
    let local = local_projection(sync, &host::resolved_values(&record_id, &sync.context_id)?);
    let merge = merge_inbound(sync, &local, client);
    let hash = projection_hash(&merge.projection);
    if merge.writes.is_empty() {
        if mapping["content_hash"].as_str() != Some(hash.as_str()) {
            host::commit(&record_id, json!({"status": SYNCED, "content_hash": hash}))?;
        }
        return Ok(Applied::Unchanged);
    }
    let values: Vec<Value> = merge
        .writes
        .iter()
        .map(|write| host::scalar(&write.attribute, &sync.context_id, write.value.as_deref()))
        .collect();
    mode.command(json!({
        "kind": "update",
        "intent_key": intent_key("update", &id, updated_at),
        "record_id": record_id,
        "values": values,
    }))?
    .map_err(|error| rejected("update_refused", error, Some(&record_id)))?;
    let revision = host::read_record(&record_id)?.content_revision;
    host::commit(
        &record_id,
        json!({"status": SYNCED, "external_id": id, "content_hash": hash,
            "record_revision": revision}),
    )?;
    Ok(Applied::Updated(record_id))
}

/// A remote client's mapped fields keyed by attribute code, so the local
/// projection function reads it like a record.
fn remote_values(
    sync: &attricat_fakturownia_sync_core::ClientSync,
    client: &Map<String, Value>,
) -> Map<String, Value> {
    sync.fields
        .iter()
        .filter_map(|mapping| {
            Some((
                mapping.attribute.clone(),
                client.get(&mapping.field)?.clone(),
            ))
        })
        .collect()
}
