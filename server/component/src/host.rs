//! Thin wrappers over the `attricat:host@1.2.0` imports. Every call is checked
//! by the host against this release's granted capabilities.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Map, Value, json};

use attricat_fakturownia_sync_core::Config;

use crate::attricat::host::api;
use crate::attricat::host::{attricat_data, selection, transfer};

/// The `source-name` of events caused by this extension's own writes.
pub const OWN_SOURCE: &str = "extension:attricat-fakturownia";
/// The manifest host permission for the Fakturownia API.
pub const API_PERMISSION: &str = "fakturownia-api";

pub fn decode<T: DeserializeOwned>(value: &str, label: &str) -> Result<T, String> {
    serde_json::from_str(value).map_err(|error| format!("Invalid {label}: {error}"))
}

pub fn encode(value: &impl Serialize) -> Result<String, String> {
    serde_json::to_string(value).map_err(|_| "Serialization failed.".to_owned())
}

pub fn call(operation: &str, request: &Value) -> Result<Value, String> {
    let response = api::call(operation, &request.to_string())?;
    decode(&response, operation)
}

pub fn config() -> Result<Config, String> {
    Config::parse(&api::call("configuration.get.v1", "{}")?)
}

pub fn secret(name: &str) -> Result<String, String> {
    let secret = call("secrets.get.v1", &json!({"name": name}))?;
    Ok(secret["value"].as_str().unwrap_or_default().to_owned())
}

/// Best-effort structured logging; a log failure never fails the work.
pub fn log(level: &str, message: &str) {
    let _ = api::log(level, message);
}

// ---------------------------------------------------------------- catalog

/// Where catalog reads and writes go. Event handlers use the JSON
/// `attricat.read.v1` and `attricat.command.v1` calls; operation runs may not,
/// and reach the same JSON through the run-scoped `attricat-data` interface,
/// writing under the run's current batch key.
pub enum Mode<'a> {
    Handler,
    Run { batch_key: &'a str },
}

impl Mode<'_> {
    /// One `attricat.read` request.
    pub fn read(&self, request: &Value) -> Result<Value, String> {
        match self {
            Mode::Handler => call("attricat.read.v1", request),
            Mode::Run { .. } => decode(&attricat_data::read(&request.to_string())?, "read"),
        }
    }

    /// One applied intent: the record it touched, or why it was refused.
    pub fn command(&self, intent: Value) -> Result<Result<String, String>, String> {
        let intent_key = intent["intent_key"].as_str().unwrap_or_default().to_owned();
        let batch_key = match self {
            Mode::Handler => intent_key.as_str(),
            Mode::Run { batch_key } => batch_key,
        };
        let request = json!({"operation": "batch", "batch": {
            "batch_key": batch_key,
            "dry_run": false,
            "intents": [intent],
        }});
        let outcomes: Value = match self {
            Mode::Handler => call("attricat.command.v1", &request)?,
            Mode::Run { .. } => decode(
                &attricat_data::batch(&request.to_string())?,
                "batch outcome",
            )?,
        };
        let outcome = &outcomes[0];
        Ok(match outcome["status"].as_str() {
            Some("applied" | "already_applied") => {
                Ok(outcome["record_id"].as_str().unwrap_or_default().to_owned())
            }
            _ => Err(outcome["error"]
                .as_str()
                .unwrap_or("intent rejected")
                .to_owned()),
        })
    }
}

/// A blueprint's current published revision.
pub struct Blueprint {
    pub id: String,
    pub version: i64,
    /// The `blueprint` read as the host answered it, with its attributes.
    pub read: Value,
}

impl Blueprint {
    /// The ID of the attribute with `code` in this revision.
    pub fn attribute_id(&self, code: &str) -> Option<&str> {
        self.read["attributes"]
            .as_array()?
            .iter()
            .find(|attribute| attribute["code"] == code)?["id"]
            .as_str()
    }
}

/// The current revision of the blueprint with `code`, or `None`.
pub fn blueprint(mode: &Mode, code: &str) -> Result<Option<Blueprint>, String> {
    let found = mode.read(&json!({"operation": "blueprint", "code": code}))?;
    let blueprint = &found["blueprint"];
    Ok(
        match (blueprint["id"].as_str(), blueprint["version"].as_i64()) {
            (Some(id), Some(version)) => Some(Blueprint {
                id: id.to_owned(),
                version,
                read: found.clone(),
            }),
            _ => None,
        },
    )
}

/// The record of `blueprint` whose `attribute` is `value`: `Ok(None)` when
/// none is, an error when several are.
pub fn lookup(
    mode: &Mode,
    blueprint: &Blueprint,
    attribute_id: &str,
    value: &str,
) -> Result<Option<String>, String> {
    let found = mode.read(&json!({"operation": "lookup", "blueprint_id": blueprint.id,
        "blueprint_version": blueprint.version, "attribute_id": attribute_id, "value": value}))?;
    Ok(found["id"].as_str().map(str::to_owned))
}

/// Records per `incoming` page, the host's maximum.
const INCOMING_PAGE: u32 = 100;

/// One page of the records of `source_blueprint` whose `field` points at
/// `record_id`: their IDs and the next cursor.
pub fn incoming(
    mode: &Mode,
    record_id: &str,
    source_blueprint: &str,
    field: &str,
    cursor: Option<&str>,
) -> Result<(Vec<String>, Option<String>), String> {
    let page = mode.read(&json!({"operation": "incoming", "record_id": record_id,
        "relationships": [{"source_blueprint": source_blueprint, "field": field}],
        "cursor": cursor, "limit": INCOMING_PAGE}))?;
    let ids = page["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item["id"].as_str().map(str::to_owned))
        .collect();
    Ok((ids, page["next_cursor"].as_str().map(str::to_owned)))
}

/// The ID of the context with `code`, or `None`.
pub fn context(mode: &Mode, code: &str) -> Result<Option<String>, String> {
    let found = mode.read(&json!({"operation": "context", "code": code}))?;
    Ok(found["id"].as_str().map(str::to_owned))
}

pub struct Record {
    /// The code of the record's blueprint.
    pub blueprint: String,
    pub content_revision: Option<i64>,
    pub deleted: bool,
    /// Values resolved in the requested context, keyed by attribute code;
    /// empty when no context was requested.
    pub values: Map<String, Value>,
    /// Relationship targets by attribute code, in order.
    pub relationships: Map<String, Value>,
}

impl Record {
    /// The first record a relationship points at.
    pub fn target(&self, code: &str) -> Option<&str> {
        self.relationships.get(code)?.as_array()?.first()?.as_str()
    }
}

/// One record, with its values resolved in `context_id` when one is given.
pub fn read_record(
    mode: &Mode,
    record_id: &str,
    context_id: Option<&str>,
) -> Result<Record, String> {
    let read = mode.read(&json!({"operation": "record", "record_id": record_id,
        "context_id": context_id}))?;
    let record = &read["record"];
    // `values` maps each code to `{value, source_context}`.
    let values: Map<String, Value> = read["resolved_values"]["values"]
        .as_object()
        .map(|values| {
            values
                .iter()
                .map(|(code, entry)| (code.clone(), entry["value"].clone()))
                .collect()
        })
        .unwrap_or_default();
    // Relationship targets are direct values, in the requested context or
    // in any context when it has none; the read resolves no relationships.
    let codes: Map<String, Value> = read["blueprint"]["attributes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|attribute| {
            Some((
                attribute["id"].as_str()?.to_owned(),
                attribute["code"].clone(),
            ))
        })
        .collect();
    let mut relationships: Map<String, Value> = Map::new();
    let direct = read["direct_values"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut in_requested = Vec::new();
    for in_context in [true, false] {
        if !in_context {
            in_requested = relationships.keys().cloned().collect();
        }
        for value in &direct {
            let (Some(target), Some(code)) = (
                value["relationship_target_record_id"].as_str(),
                value["attribute_id"]
                    .as_str()
                    .and_then(|id| codes.get(id))
                    .and_then(Value::as_str),
            ) else {
                continue;
            };
            if value["active"] == false
                || (in_context && value["context_id"].as_str() != context_id)
                || (!in_context && in_requested.iter().any(|known| known == code))
            {
                continue;
            }
            if let Value::Array(targets) = relationships
                .entry(code.to_owned())
                .or_insert_with(|| Value::Array(Vec::new()))
            {
                targets.push(json!(target));
            }
        }
    }
    Ok(Record {
        relationships,
        blueprint: read["blueprint"]["blueprint"]["code"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        content_revision: record["content_revision"].as_i64(),
        deleted: !record["deleted_at"].is_null(),
        values,
    })
}

/// A string value write.
pub fn scalar(attribute: &str, context_id: &str, value: &str) -> Value {
    json!({"kind": "scalar", "attribute_code": attribute, "context_id": context_id,
        "value": value})
}

/// A value to clear, for an intent's `remove_values`. Clearing works for any
/// attribute type, unlike writing `null` (refused) or `""` (strings only).
pub fn cleared(attribute: &str, context_id: &str) -> Value {
    json!({"attribute_code": attribute, "context_id": context_id})
}

// ---------------------------------------------------------------- mappings

pub fn mapping_by_record(record_id: &str) -> Result<Option<Value>, String> {
    let response = call("mappings.get.v1", &json!({"record_id": record_id}))?;
    Ok(Some(response["mapping"].clone()).filter(|mapping| !mapping.is_null()))
}

pub fn mapping_by_external_id(external_id: &str) -> Result<Option<Value>, String> {
    let response = call("mappings.get.v1", &json!({"external_id": external_id}))?;
    Ok(Some(response["mapping"].clone()).filter(|mapping| !mapping.is_null()))
}

pub enum Begin {
    Proceed { lease_token: String, mapping: Value },
    Stale,
    Busy,
}

pub fn begin(record_id: &str, record_revision: i64) -> Result<Begin, String> {
    let response = call(
        "mappings.begin.v1",
        &json!({"record_id": record_id, "record_revision": record_revision}),
    )?;
    Ok(match response["outcome"].as_str() {
        Some("proceed") => Begin::Proceed {
            lease_token: response["lease_token"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            mapping: response["mapping"].clone(),
        },
        Some("stale") => Begin::Stale,
        _ => Begin::Busy,
    })
}

pub fn commit(record_id: &str, fields: Value) -> Result<(), String> {
    let mut request = json!({"record_id": record_id});
    if let (Some(request), Some(fields)) = (request.as_object_mut(), fields.as_object()) {
        request.extend(fields.clone());
    }
    call("mappings.commit.v1", &request)?;
    Ok(())
}

pub fn delete_mapping(record_id: &str) -> Result<(), String> {
    call("mappings.delete.v1", &json!({"record_id": record_id}))?;
    Ok(())
}

pub fn release(record_id: &str, lease_token: &str) {
    let _ = call(
        "mappings.release.v1",
        &json!({"record_id": record_id, "lease_token": lease_token}),
    );
}

// ---------------------------------------------------------------- runs

/// One member of an interactive run's selection and the cursor after it.
pub fn selection_member(cursor: &str) -> Result<(Option<Value>, Option<String>), String> {
    let page: Value = decode(&selection::page(cursor, 1)?, "selection page")?;
    Ok((
        page["records"]
            .as_array()
            .and_then(|records| records.first())
            .cloned(),
        page["next_cursor"].as_str().map(str::to_owned),
    ))
}

/// The number of records in an interactive run's selection.
pub fn selection_count() -> Result<u64, String> {
    let described: Value = decode(&selection::describe()?, "selection")?;
    Ok(described["count"].as_u64().unwrap_or_default())
}

/// The largest document fetched into a run, the host's range maximum.
const MAX_FETCH_BYTES: u32 = 16 * 1024 * 1024;

/// Downloads `path_and_query` from the account into an input artifact of the
/// run, with the API token as a host-substituted Bearer header, and returns
/// the artifact's ID. A retried batch gets the same artifact back.
pub fn fetch_document(
    config: &Config,
    path_and_query: &str,
    transfer_key: &str,
) -> Result<String, String> {
    let request = json!({
        "host_permission_id": API_PERMISSION,
        "url": format!("{}{path_and_query}", config.base_url()),
        "transfer_key": transfer_key,
        "offset": 0,
        "max_bytes": MAX_FETCH_BYTES,
        "secret_headers": [{"secret": config.token_secret(), "header": "authorization",
            "prefix": "Bearer "}],
    });
    let fetched: Value = decode(
        &transfer::fetch_input_v2(&request.to_string())?,
        "fetched input",
    )?;
    fetched["artifact_id"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("the document was not fetched: {fetched}"))
}

/// Attaches an input artifact of the run to a record's file attribute.
pub fn attach_file(
    record_id: &str,
    attribute: &str,
    context_id: &str,
    artifact_id: &str,
    filename: &str,
) -> Result<Value, String> {
    call(
        "files.attach.v1",
        &json!({"record_id": record_id, "attribute_code": attribute, "context_id": context_id,
            "artifact_id": artifact_id, "filename": filename}),
    )
}

// ---------------------------------------------------------------- storage

pub fn storage_get(key: &str) -> Result<Option<Value>, String> {
    let entry = call("storage.get.v1", &json!({"key": key}))?;
    Ok(Some(entry["value"].clone()).filter(|value| !value.is_null()))
}

pub fn storage_set(key: &str, value: &Value) -> Result<(), String> {
    call(
        "storage.set.v1",
        &json!({"key": key, "value": value, "expected_revision": null}),
    )?;
    Ok(())
}

// ---------------------------------------------------------------- Fakturownia

/// The outcome of one brokered Fakturownia request.
pub enum Http {
    Response {
        status: u16,
        body: String,
    },
    /// The rule's budget is spent; try again after the wait.
    RateLimited(u64),
    /// Nothing was sent; safe to send again.
    Unreachable,
    /// The request may have been applied.
    Uncertain,
}

/// Sends one request to `<account>.fakturownia.pl`, authorized with the API
/// token as a host-substituted `Authorization: Bearer` header, so the token
/// never enters component memory or the URL.
pub fn fakturownia(
    config: &Config,
    method: &str,
    path_and_query: &str,
    body: Option<&Value>,
) -> Result<Http, String> {
    let mut request = json!({
        "host_permission_id": API_PERMISSION,
        "method": method,
        "url": format!("{}{path_and_query}", config.base_url()),
        "headers": {"accept": "application/json"},
        "secret_headers": [{"secret": config.token_secret(), "header": "authorization",
            "prefix": "Bearer "}],
    });
    if let Some(body) = body {
        request["body"] = Value::String(body.to_string());
        request["headers"]["content-type"] = Value::String("application/json".into());
    }
    let response = call("network.request.v2", &request)?;
    Ok(match response["outcome"].as_str() {
        Some("response") => {
            let bytes = BASE64
                .decode(response["body_base64"].as_str().unwrap_or_default())
                .map_err(|_| "Invalid response body encoding".to_owned())?;
            Http::Response {
                status: response["status"].as_u64().unwrap_or_default() as u16,
                body: String::from_utf8_lossy(&bytes).into_owned(),
            }
        }
        Some("rate_limited") => {
            Http::RateLimited(response["retry_after_ms"].as_u64().unwrap_or(1000))
        }
        Some("unreachable") => Http::Unreachable,
        _ => Http::Uncertain,
    })
}
