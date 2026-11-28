//! Both directions of the client sync. Each side's changes reach the other
//! through the export mapping of the record, whose content hash is the
//! projection both sides last agreed on (see `attricat-fakturownia-sync-core`).

use serde_json::{Map, Value, json};

use attricat_fakturownia_sync_core::{
    CLIENT_ROOT, Config, local_projection, merge_inbound, outbound_client, projection_hash,
};

use crate::attricat::host::outcomes::Failure;
use crate::host::{self, Begin, Http, Mode, budget_spent, failed_response, rejected, retry};

const STATUS_OK: u16 = 200;
const STATUS_CREATED: u16 = 201;
const STATUS_NOT_FOUND: u16 = 404;
/// Mapping statuses (see the host's export mapping store).
const SYNCED: &str = "synced";
const UNCERTAIN: &str = "uncertain";
/// Bound of an intent key.
const MAX_INTENT_KEY_BYTES: usize = 128;

fn client_id(body: &str) -> Option<String> {
    let client: Value = serde_json::from_str(body).ok()?;
    let id = &client["id"];
    id.as_i64()
        .map(|id| id.to_string())
        .or_else(|| id.as_str().map(str::to_owned))
}

/// The configured context's ID.
pub fn context_id(config: &Config, mode: &Mode) -> Result<String, Failure> {
    let code = &config.clients.context;
    host::context(mode, code)?.ok_or_else(|| {
        rejected(
            "configuration_invalid",
            format!("no context has the code {code}"),
            None,
        )
    })
}

/// The configured blueprint's current revision, which new records use.
pub fn blueprint(config: &Config, mode: &Mode) -> Result<host::Blueprint, Failure> {
    let code = &config.clients.blueprint;
    host::blueprint(mode, code)?.ok_or_else(|| {
        rejected(
            "configuration_invalid",
            format!("no published blueprint has the code {code}"),
            None,
        )
    })
}

/// Pushes a changed client record to Fakturownia.
pub fn push_record(config: &Config, record_id: &str, revision: Option<i64>) -> Result<(), Failure> {
    let mode = Mode::Handler;
    // Every record event arrives here; one read tells whether it is a client.
    let record = host::read_record(&mode, record_id, None)?;
    if record.blueprint != config.clients.blueprint || record.deleted {
        return Ok(());
    }
    let context_id = context_id(config, &mode)?;
    let values = host::read_record(&mode, record_id, Some(&context_id))?.values;
    let projection = local_projection(&config.clients, &values);
    let hash = projection_hash(&projection);
    let revision = revision.or(record.content_revision).unwrap_or_default();
    let (lease_token, mapping) = match host::begin(record_id, revision)? {
        Begin::Proceed {
            lease_token,
            mapping,
        } => (lease_token, mapping),
        Begin::Stale => return Ok(()),
        Begin::Busy => {
            return Err(retry(
                "Another push holds the record; retrying later.",
                None,
            ));
        }
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
        Http::Response { status, body, .. } if status == STATUS_OK || status == STATUS_CREATED => {
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
                "The client was deleted in Fakturownia, so the record cannot be pushed.".into(),
                Some(record_id),
            ))
        }
        Http::Response {
            status,
            body,
            retry_after,
        } => {
            let failure = failed_response(status, &body, retry_after.as_deref(), Some(record_id));
            if matches!(failure, Failure::Retry(_)) {
                host::release(record_id, &lease_token);
            }
            Err(failure)
        }
        Http::RateLimited(after_ms) => {
            host::release(record_id, &lease_token);
            Err(budget_spent(after_ms))
        }
        Http::Unreachable => {
            host::release(record_id, &lease_token);
            Err(host::unreachable())
        }
        Http::Uncertain => {
            host::commit(
                record_id,
                json!({"lease_token": lease_token, "status": UNCERTAIN}),
            )?;
            Err(retry(
                "Fakturownia did not confirm the push, which may have been applied; retrying.",
                None,
            ))
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
        Http::Response { status, body, .. } if status == STATUS_OK => {
            let clients: Value = host::decode(&body, "client list")?;
            Ok(clients
                .as_array()
                .and_then(|clients| clients.first())
                .and_then(|client| client_id(&client.to_string())))
        }
        Http::Response {
            status,
            body,
            retry_after,
        } => Err(failed_response(
            status,
            &body,
            retry_after.as_deref(),
            Some(record_id),
        )),
        Http::RateLimited(after_ms) => Err(budget_spent(after_ms)),
        _ => Err(retry(
            "Could not look the client up in Fakturownia; retrying later.",
            None,
        )),
    }
}

/// Fetches a client from Fakturownia; `None` when it no longer exists.
pub fn fetch_client(config: &Config, id: i64) -> Result<Option<Map<String, Value>>, Failure> {
    match host::fakturownia(config, "GET", &format!("/clients/{id}.json"), None)? {
        Http::Response { status, body, .. } if status == STATUS_OK => {
            let client: Value = host::decode(&body, "client")?;
            Ok(client.as_object().cloned())
        }
        Http::Response { status, .. } if status == STATUS_NOT_FOUND => Ok(None),
        Http::Response {
            status,
            body,
            retry_after,
        } => Err(failed_response(status, &body, retry_after.as_deref(), None)),
        Http::RateLimited(after_ms) => Err(budget_spent(after_ms)),
        _ => Err(retry("Fakturownia did not answer; retrying later.", None)),
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

/// Applies a fetched Fakturownia client to its record, creating the record
/// when the client is new to Attricat. Only the fields that differ are
/// written, and the merged projection becomes the agreed state.
pub fn apply_client(
    config: &Config,
    client: &Map<String, Value>,
    mode: &Mode,
) -> Result<Applied, Failure> {
    let id = client_id(&Value::Object(client.clone()).to_string()).ok_or_else(|| {
        rejected(
            "invalid_client",
            "Fakturownia sent a client without an ID.".into(),
            None,
        )
    })?;
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
    let context_id = context_id(config, mode)?;
    let Some(mapping) = mapping else {
        let blueprint = blueprint(config, mode)?;
        let remote = local_projection(sync, &remote_values(sync, client));
        let values: Vec<Value> = sync
            .fields
            .iter()
            .filter_map(|mapping| {
                let value = remote.get(&mapping.field)?;
                Some(host::scalar(&mapping.attribute, &context_id, value))
            })
            .collect();
        let record_id = mode
            .command(json!({
                "kind": "create",
                "intent_key": intent_key("create", &id, updated_at),
                "blueprint_id": blueprint.id,
                "blueprint_version": blueprint.version,
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
    let local = local_projection(
        sync,
        &host::read_record(mode, &record_id, Some(&context_id))?.values,
    );
    let merge = merge_inbound(sync, &local, client);
    let hash = projection_hash(&merge.projection);
    if merge.writes.is_empty() {
        if mapping["content_hash"].as_str() != Some(hash.as_str()) {
            host::commit(&record_id, json!({"status": SYNCED, "content_hash": hash}))?;
        }
        return Ok(Applied::Unchanged);
    }
    let mut values = Vec::new();
    let mut remove_values = Vec::new();
    for write in &merge.writes {
        match &write.value {
            Some(value) => values.push(host::scalar(&write.attribute, &context_id, value)),
            None => remove_values.push(host::cleared(&write.attribute, &context_id)),
        }
    }
    mode.command(json!({
        "kind": "update",
        "intent_key": intent_key("update", &id, updated_at),
        "record_id": record_id,
        "values": values,
        "remove_values": remove_values,
    }))?
    .map_err(|error| rejected("update_refused", error, Some(&record_id)))?;
    let revision = host::read_record(mode, &record_id, None)?.content_revision;
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
