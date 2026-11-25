//! Thin wrappers over the `attricat:host@1.2.0` imports. Every call is checked
//! by the host against this release's granted capabilities.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Map, Value, json};

use attricat_fakturownia_sync_core::Config;

use crate::attricat::host::api;
use crate::attricat::host::attricat_data;

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

/// The configured blueprint's current published revision.
pub struct Blueprint {
    pub id: String,
    pub version: i64,
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
            }),
            _ => None,
        },
    )
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
    let values = read["resolved_values"]["values"]
        .as_object()
        .map(|values| {
            values
                .iter()
                .map(|(code, entry)| (code.clone(), entry["value"].clone()))
                .collect()
        })
        .unwrap_or_default();
    Ok(Record {
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

pub fn release(record_id: &str, lease_token: &str) {
    let _ = call(
        "mappings.release.v1",
        &json!({"record_id": record_id, "lease_token": lease_token}),
    );
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
