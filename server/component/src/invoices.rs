//! Fakturownia invoices issued from Attricat records.
//!
//! - The `issue-invoices` interactive operation issues one invoice per
//!   selected source record ([`process_batch`]), one record per batch.
//! - Webhooks and `pull` apply an invoice's state to its source record
//!   ([`apply_invoice`]), and a deleted invoice clears it ([`deleted`]).
//!
//! The source record's export mapping holds the invoice as
//! `invoice:<id>`. Before an invoice is created, the mapping is committed
//! `uncertain` with a `pending` marker (run, order number, client), so a
//! retry after a lost answer looks the invoice up by order number and buyer
//! instead of issuing a second one; Fakturownia's `oid_unique` refuses a
//! duplicate order number as a second guard.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use attricat_fakturownia_sync_core::invoices::{
    BlueprintShape, Catalog, HeaderValues, InvoiceSync, PositionValues, blueprint_shape,
    clear_writes, invoice_external_id, invoice_id, invoice_state, local_write_back, path_segments,
    pdf_filename, position, validate_against, write_back_hash, write_back_projection,
    write_back_writes,
};
use attricat_fakturownia_sync_core::{Config, InboundWrite, validation_message};

use crate::attricat::host::clock;
use crate::attricat::host::outcomes::{Failure, Rejection, Retry};
use crate::exports::attricat::host::deferrable_operations::{BatchOutcome, OperationRequest};
use crate::host::{self, Http, Mode, Record, decode, encode};

pub const OPERATION: &str = "issue-invoices";

const STATUS_OK: u16 = 200;
const STATUS_CREATED: u16 = 201;
const STATUS_UNAUTHORIZED: u16 = 401;
const STATUS_FORBIDDEN: u16 = 403;
const STATUS_NOT_FOUND: u16 = 404;
const STATUS_UNPROCESSABLE: u16 = 422;
const RETRY_AFTER_MS: u64 = 5_000;
/// How long another run's pending create is left alone before this run
/// looks for its invoice and, finding none, creates one.
const PENDING_GRACE_MS: i64 = 120_000;
/// Mapping statuses.
const SYNCED: &str = "synced";
const UNCERTAIN: &str = "uncertain";
const REJECTED: &str = "rejected";
/// Bound of an intent key.
const MAX_INTENT_KEY_BYTES: usize = 128;

fn retry(message: &str) -> Failure {
    Failure::Retry(Retry {
        message: message.into(),
        after_ms: Some(RETRY_AFTER_MS),
    })
}

fn configuration_invalid(message: String) -> Failure {
    Failure::Rejected(Rejection {
        code: "configuration_invalid".into(),
        message,
        record_id: None,
        connection: None,
    })
}

fn now_ms() -> i64 {
    let now = clock::now();
    now.seconds as i64 * 1_000 + i64::from(now.nanoseconds / 1_000_000)
}

fn text(value: Option<&Value>) -> Option<String> {
    let text = match value? {
        Value::String(text) => text.trim().to_owned(),
        Value::Number(number) => number.to_string(),
        _ => return None,
    };
    (!text.is_empty()).then_some(text)
}

fn intent_key(parts: &[&str]) -> String {
    let mut key = parts.join(":");
    key.truncate(MAX_INTENT_KEY_BYTES);
    key
}

/// The `invoices` section, when configured.
pub fn section(config: &Config) -> Option<&InvoiceSync> {
    config.invoices.as_ref()
}

// ------------------------------------------------------------ the catalog

/// Blueprint shapes read through `mode`, each once.
struct HostCatalog<'a> {
    mode: &'a Mode<'a>,
    cache: BTreeMap<String, Option<BlueprintShape>>,
}

impl Catalog for HostCatalog<'_> {
    fn blueprint(&mut self, code: &str) -> Result<Option<BlueprintShape>, String> {
        if let Some(shape) = self.cache.get(code) {
            return Ok(shape.clone());
        }
        let found = self
            .mode
            .read(&json!({"operation": "blueprint", "code": code}))?;
        let shape = if found.is_null() {
            None
        } else {
            blueprint_shape(&found)
        };
        self.cache.insert(code.to_owned(), shape.clone());
        Ok(shape)
    }
}

/// The `invoices` section, checked against the catalog. A bad
/// configuration fails here, before any Fakturownia call.
pub fn validated<'a>(config: &'a Config, mode: &Mode) -> Result<&'a InvoiceSync, Failure> {
    let sync = section(config)
        .ok_or_else(|| configuration_invalid("invoices are not configured".into()))?;
    let mut catalog = HostCatalog {
        mode,
        cache: BTreeMap::new(),
    };
    validate_against(sync, &config.clients.blueprint, &mut catalog)
        .map_err(configuration_invalid)?;
    Ok(sync)
}

/// The configured source context's ID.
fn context_id(sync: &InvoiceSync, mode: &Mode) -> Result<String, Failure> {
    host::context(mode, &sync.source.context)?.ok_or_else(|| {
        configuration_invalid(format!("no context has the code {}", sync.source.context))
    })
}

/// Records read in one context, each once per batch.
struct Reader<'a> {
    mode: &'a Mode<'a>,
    context_id: &'a str,
    records: BTreeMap<String, Record>,
}

impl<'a> Reader<'a> {
    fn new(mode: &'a Mode<'a>, context_id: &'a str) -> Self {
        Self {
            mode,
            context_id,
            records: BTreeMap::new(),
        }
    }

    fn read(&mut self, record_id: &str) -> Result<&Record, String> {
        if !self.records.contains_key(record_id) {
            let record = host::read_record(self.mode, record_id, Some(self.context_id))?;
            self.records.insert(record_id.to_owned(), record);
        }
        Ok(&self.records[record_id])
    }

    /// The text at `path` from `record_id`: relationships are followed to
    /// their first target; a target without the leaf yields nothing.
    fn path_text(&mut self, record_id: &str, path: &str) -> Result<Option<String>, String> {
        let segments = path_segments(path)?;
        let (leaf, hops) = segments.split_last().expect("one segment at least");
        let mut current = record_id.to_owned();
        for hop in hops {
            let Some(target) = self.read(&current)?.target(hop).map(str::to_owned) else {
                return Ok(None);
            };
            current = target;
        }
        Ok(text(self.read(&current)?.values.get(*leaf)))
    }
}

// ------------------------------------------------------------ issuing

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct Outcome {
    record_id: String,
    /// `succeeded`, `failed` or `skipped`.
    outcome: String,
    message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    number: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    url: Option<String>,
}

#[derive(Default, Deserialize, Serialize)]
struct Checkpoint {
    #[serde(default)]
    cursor: String,
    #[serde(default)]
    total: u64,
    #[serde(default)]
    started: bool,
    #[serde(default)]
    results: Vec<Outcome>,
}

impl Checkpoint {
    fn count(&self, outcome: &str) -> usize {
        self.results
            .iter()
            .filter(|result| result.outcome == outcome)
            .count()
    }

    fn progress(&self) -> Value {
        json!({"completed": self.results.len(), "total": self.total,
            "outcome": {"succeeded": self.count("succeeded"), "failed": self.count("failed"),
                "skipped": self.count("skipped")},
            "results": self.results})
    }
}

/// Refuses to start a run whose configuration is incomplete or does not
/// fit the catalog. The error is JSON, which the host keeps.
pub fn prepare(request: &OperationRequest) -> Result<String, String> {
    let config = host::config()?;
    let mode = Mode::Run {
        batch_key: &request.batch_key,
    };
    validated(&config, &mode).map_err(|failure| match failure {
        Failure::Rejected(rejection) => {
            json!({"code": rejection.code, "message": rejection.message}).to_string()
        }
        Failure::Retry(retry) => {
            json!({"code": "unavailable", "message": retry.message}).to_string()
        }
        Failure::Fault(message) => json!({"code": "fault", "message": message}).to_string(),
    })?;
    Ok("prepared".into())
}

/// Issues the invoice of the next selected record.
pub fn process_batch(request: &OperationRequest) -> Result<BatchOutcome, Failure> {
    let config = host::config()?;
    let mode = Mode::Run {
        batch_key: &request.batch_key,
    };
    let sync = validated(&config, &mode)?;
    let mut checkpoint: Checkpoint = if request.checkpoint.is_empty() {
        Checkpoint::default()
    } else {
        decode(&request.checkpoint, "checkpoint").unwrap_or_default()
    };
    if !checkpoint.started {
        checkpoint.total = host::selection_count()?;
        checkpoint.started = true;
    }
    let (member, next) = host::selection_member(&checkpoint.cursor)?;
    if let Some(member) = member {
        let record_id = member["record_id"].as_str().unwrap_or_default().to_owned();
        let outcome = match member["status"].as_str() {
            Some("available") => issue(&config, sync, &mode, &record_id, &request.run_id)?,
            status => Outcome {
                record_id,
                outcome: "skipped".into(),
                message: format!("the record is {}", status.unwrap_or("unavailable")),
                ..Outcome::default()
            },
        };
        host::log(
            "info",
            &format!(
                "issue invoice for {}: {} {}",
                outcome.record_id, outcome.outcome, outcome.message
            ),
        );
        checkpoint.results.push(outcome);
    }
    let done = next.is_none();
    checkpoint.cursor = next.unwrap_or_default();
    Ok(BatchOutcome {
        checkpoint: encode(&checkpoint)?,
        progress: checkpoint.progress().to_string(),
        done,
        resume_after_ms: None,
    })
}

/// A create that started and may have reached Fakturownia.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct Pending {
    run: String,
    oid: String,
    client_id: String,
    at_ms: i64,
}

/// Commits the source record's mapping as rejected and reports it.
fn reject(record_id: &str, code: &str, message: String) -> Result<Outcome, Failure> {
    host::commit(
        record_id,
        json!({"status": REJECTED, "external_ids": {},
            "error": {"code": code, "message": message}}),
    )?;
    Ok(Outcome {
        record_id: record_id.to_owned(),
        outcome: "failed".into(),
        message,
        ..Outcome::default()
    })
}

/// What a source record would be invoiced with.
struct Draft {
    client_id: String,
    oid: String,
    body: Value,
}

/// Reads the source, its buyer and its positions, and builds the invoice;
/// `Err` holds the reason nothing may be sent.
fn draft(
    sync: &InvoiceSync,
    reader: &mut Reader,
    record_id: &str,
) -> Result<Result<Draft, (String, String)>, Failure> {
    let source = reader.read(record_id)?;
    let Some(buyer) = source.target(&sync.buyer).map(str::to_owned) else {
        return Ok(Err((
            "buyer_missing".into(),
            format!("record {record_id}: {} is empty", sync.buyer),
        )));
    };
    let header: HeaderValues = sync
        .header_reads()
        .into_iter()
        .filter_map(|(field, code)| Some((field, text(source.values.get(code))?)))
        .collect();
    let client = host::mapping_by_record(&buyer)?;
    let Some(client_id) = client
        .as_ref()
        .and_then(|mapping| mapping["external_id"].as_str())
        .filter(|id| invoice_id(id).is_none())
        .map(str::to_owned)
    else {
        return Ok(Err((
            "buyer_not_in_fakturownia".into(),
            format!(
                "record {record_id}: the buyer {buyer} is not a Fakturownia client yet; save the client record so it is pushed, then issue again"
            ),
        )));
    };
    let mut lines = Vec::new();
    match &sync.positions.lines {
        Some(config) => {
            let mut cursor = None;
            loop {
                let (ids, next) = host::incoming(
                    reader.mode,
                    record_id,
                    &config.blueprint,
                    &config.parent,
                    cursor.as_deref(),
                )?;
                lines.extend(ids);
                match next {
                    Some(next) => cursor = Some(next),
                    None => break,
                }
            }
        }
        None => lines.push(record_id.to_owned()),
    }
    if lines.is_empty() {
        return Ok(Err((
            "no_positions".into(),
            format!("record {record_id}: it has no lines to invoice"),
        )));
    }
    let mut positions = Vec::new();
    let mut errors = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let label = if sync.positions.lines.is_some() {
            format!("line {} ({line})", index + 1)
        } else {
            format!("record {line}")
        };
        let read = |path: Option<&str>, reader: &mut Reader| -> Result<Option<String>, String> {
            match path {
                Some(path) => reader.path_text(line, path),
                None => Ok(None),
            }
        };
        let values = PositionValues {
            name: read(Some(&sync.positions.name), reader)?,
            code: read(sync.positions.code.as_deref(), reader)?,
            quantity: read(Some(&sync.positions.quantity), reader)?,
            unit: read(sync.positions.unit.as_deref(), reader)?,
            price_net: read(Some(&sync.positions.price_net), reader)?,
            tax: read(sync.positions.tax.attribute.as_deref(), reader)?,
        };
        match position(&sync.positions, &label, &values) {
            Ok(body) => positions.push(body),
            Err(reasons) => errors.extend(reasons),
        }
    }
    if !errors.is_empty() {
        return Ok(Err(("position_incomplete".into(), errors.join("; "))));
    }
    let oid = sync.oid(&header, record_id);
    let body = sync.invoice_body(&client_id, &oid, &header, positions);
    Ok(Ok(Draft {
        client_id,
        oid,
        body,
    }))
}

/// Issues the invoice of one source record, or reports why not.
fn issue(
    config: &Config,
    sync: &InvoiceSync,
    mode: &Mode,
    record_id: &str,
    run_id: &str,
) -> Result<Outcome, Failure> {
    let context_id = context_id(sync, mode)?;
    let mut reader = Reader::new(mode, &context_id);
    let source = reader.read(record_id)?;
    if source.blueprint != sync.source.blueprint {
        return Ok(Outcome {
            record_id: record_id.to_owned(),
            outcome: "skipped".into(),
            message: format!("not a {} record", sync.source.blueprint),
            ..Outcome::default()
        });
    }
    let mapping = host::mapping_by_record(record_id)?.unwrap_or(Value::Null);
    if let Some(invoice) = mapping["external_id"].as_str().and_then(invoice_id) {
        // A retried batch of the run that issued it finishes the write-back.
        if mapping["external_ids"]["run"] == run_id {
            let invoice = fetch_invoice(config, invoice)?
                .ok_or_else(|| retry("the invoice just issued is not found"))?;
            return finish(config, sync, mode, &context_id, record_id, run_id, &invoice);
        }
        let number = mapping["external_ids"]["number"]
            .as_str()
            .unwrap_or(invoice);
        let url = text(
            reader
                .read(record_id)?
                .values
                .get(sync.write_back.url.as_deref().unwrap_or_default()),
        );
        return Ok(Outcome {
            record_id: record_id.to_owned(),
            outcome: "failed".into(),
            message: format!("already invoiced as {number}"),
            number: Some(number.to_owned()),
            url,
        });
    }
    let draft = match draft(sync, &mut reader, record_id)? {
        Ok(draft) => draft,
        Err((code, message)) => return reject(record_id, &code, message),
    };
    let pending: Option<Pending> = (mapping["status"] == UNCERTAIN)
        .then(|| serde_json::from_value(mapping["external_ids"]["pending"].clone()).ok())
        .flatten();
    if let Some(pending) = &pending {
        if pending.run != run_id && now_ms() - pending.at_ms < PENDING_GRACE_MS {
            return Err(retry("another run is issuing this record's invoice"));
        }
        // ASSUMED: the list filters by `oid` and `client_id` together.
        if let Some(invoice) = find_invoice(config, &pending.oid, &pending.client_id)? {
            return finish(config, sync, mode, &context_id, record_id, run_id, &invoice);
        }
    }
    let marker = Pending {
        run: run_id.to_owned(),
        oid: draft.oid.clone(),
        client_id: draft.client_id.clone(),
        at_ms: pending
            .as_ref()
            .filter(|pending| pending.run == run_id)
            .map_or_else(now_ms, |pending| pending.at_ms),
    };
    host::commit(
        record_id,
        json!({"status": UNCERTAIN, "external_ids": {"pending": marker}}),
    )?;
    match host::fakturownia(config, "POST", "/invoices.json", Some(&draft.body))? {
        Http::Response { status, body } if status == STATUS_CREATED || status == STATUS_OK => {
            let invoice: Value = decode(&body, "invoice")?;
            let invoice = invoice
                .as_object()
                .cloned()
                .ok_or_else(|| retry("Fakturownia answered without an invoice"))?;
            finish(config, sync, mode, &context_id, record_id, run_id, &invoice)
        }
        Http::Response { status, body } if status == STATUS_UNPROCESSABLE => {
            // `oid_unique` refuses a second invoice for the order: find the
            // one that exists, which a lost answer may have created.
            if let Some(invoice) = find_invoice(config, &draft.oid, &draft.client_id)? {
                return finish(config, sync, mode, &context_id, record_id, run_id, &invoice);
            }
            reject(record_id, "fakturownia_invalid", validation_message(&body))
        }
        Http::Response { status, .. }
            if status == STATUS_UNAUTHORIZED || status == STATUS_FORBIDDEN =>
        {
            reject(
                record_id,
                "fakturownia_unauthorized",
                "Fakturownia refused the API token".into(),
            )
        }
        Http::Response { status, .. } if status >= 500 => {
            Err(retry("Fakturownia failed to answer"))
        }
        Http::Response { status, .. } => reject(
            record_id,
            "fakturownia_unexpected",
            format!("Fakturownia answered HTTP {status}"),
        ),
        Http::RateLimited(after_ms) => Err(Failure::Retry(Retry {
            message: "request budget spent".into(),
            after_ms: Some(after_ms),
        })),
        Http::Unreachable => Err(retry("Fakturownia is unreachable")),
        // The invoice may exist: the retried batch looks it up first.
        Http::Uncertain => Err(retry("the invoice may have been issued")),
    }
}

/// Records an issued invoice on its source: the mapping, the write-back
/// fields and the PDF.
fn finish(
    config: &Config,
    sync: &InvoiceSync,
    mode: &Mode,
    context_id: &str,
    record_id: &str,
    run_id: &str,
    invoice: &Map<String, Value>,
) -> Result<Outcome, Failure> {
    let id = text(invoice.get("id")).ok_or_else(|| retry("the invoice has no id"))?;
    let state = invoice_state(&config.base_url(), invoice);
    host::commit(
        record_id,
        json!({"status": SYNCED, "external_id": invoice_external_id(&id),
            "external_ids": {"run": run_id, "number": state.get("number"), "token": invoice.get("token")}}),
    )?;
    let mut message = format!(
        "issued {}",
        state.get("number").map_or(id.as_str(), String::as_str)
    );
    write_back(sync, mode, context_id, record_id, &id, &state)?;
    if let Some(attribute) = &sync.write_back.file {
        let filename = pdf_filename(state.get("number").map_or(id.as_str(), String::as_str));
        let attached = host::fetch_document(
            config,
            &format!("/invoices/{id}.pdf"),
            &format!("invoice-pdf-{id}"),
        )
        .and_then(|artifact| {
            host::attach_file(record_id, attribute, context_id, &artifact, &filename)
        });
        if let Err(error) = attached {
            host::log("warn", &format!("invoice {id} PDF not attached: {error}"));
            message.push_str(&format!("; the PDF was not attached: {error}"));
        }
    }
    Ok(Outcome {
        record_id: record_id.to_owned(),
        outcome: "succeeded".into(),
        message,
        number: state.get("number").cloned(),
        url: state.get("url").cloned(),
    })
}

/// Writes an invoice's state to its source where it differs, and stores
/// the agreed hash and revision. Returns whether anything was written.
fn write_back(
    sync: &InvoiceSync,
    mode: &Mode,
    context_id: &str,
    record_id: &str,
    invoice: &str,
    state: &BTreeMap<&'static str, String>,
) -> Result<bool, Failure> {
    let record = host::read_record(mode, record_id, Some(context_id))?;
    let local = local_write_back(&sync.write_back, &record.values);
    let remote = write_back_projection(&sync.write_back, state);
    let hash = write_back_hash(&remote);
    let writes = write_back_writes(&sync.write_back, &local, &remote);
    // The record's revision makes the key unique per change: an invoice that
    // returns to an earlier state (paid, partial, paid again) repeats its
    // hash, and the host would skip a write whose key it has already seen.
    // It goes before the hash, which intent_key may cut short.
    let revision = record
        .content_revision
        .map_or_else(|| "-".to_owned(), |revision| revision.to_string());
    let written = apply_writes(
        mode,
        context_id,
        record_id,
        &writes,
        &["invoice", invoice, &revision, &hash],
    )?;
    let revision = if written {
        host::read_record(mode, record_id, None)?.content_revision
    } else {
        record.content_revision
    };
    host::commit(
        record_id,
        json!({"status": SYNCED, "content_hash": hash, "record_revision": revision}),
    )?;
    Ok(written)
}

/// Applies write-back writes as one update; `false` when there are none.
fn apply_writes(
    mode: &Mode,
    context_id: &str,
    record_id: &str,
    writes: &[InboundWrite],
    key: &[&str],
) -> Result<bool, Failure> {
    if writes.is_empty() {
        return Ok(false);
    }
    let mut values = Vec::new();
    let mut remove_values = Vec::new();
    for write in writes {
        match &write.value {
            Some(value) => values.push(host::scalar(&write.attribute, context_id, value)),
            None => remove_values.push(host::cleared(&write.attribute, context_id)),
        }
    }
    let mut parts = vec!["writeback", record_id];
    parts.extend_from_slice(key);
    mode.command(json!({
        "kind": "update",
        "intent_key": intent_key(&parts),
        "record_id": record_id,
        "values": values,
        "remove_values": remove_values,
    }))?
    .map_err(|error| {
        Failure::Rejected(Rejection {
            code: "write_back_refused".into(),
            message: error,
            record_id: Some(record_id.to_owned()),
            connection: None,
        })
    })?;
    Ok(true)
}

// ------------------------------------------------------------ Fakturownia

fn failed_read(status: u16) -> Failure {
    if status == STATUS_UNAUTHORIZED || status == STATUS_FORBIDDEN {
        Failure::Rejected(Rejection {
            code: "fakturownia_unauthorized".into(),
            message: "Fakturownia refused the API token".into(),
            record_id: None,
            connection: None,
        })
    } else {
        retry(&format!("Fakturownia answered HTTP {status}"))
    }
}

/// Fetches an invoice; `None` when it no longer exists.
pub fn fetch_invoice(config: &Config, id: &str) -> Result<Option<Map<String, Value>>, Failure> {
    match host::fakturownia(config, "GET", &format!("/invoices/{id}.json"), None)? {
        Http::Response { status, body } if status == STATUS_OK => {
            let invoice: Value = decode(&body, "invoice")?;
            Ok(invoice.as_object().cloned())
        }
        Http::Response { status, .. } if status == STATUS_NOT_FOUND => Ok(None),
        Http::Response { status, .. } => Err(failed_read(status)),
        Http::RateLimited(after_ms) => Err(Failure::Retry(Retry {
            message: "request budget spent".into(),
            after_ms: Some(after_ms),
        })),
        _ => Err(retry("Fakturownia did not answer")),
    }
}

/// The invoice with order number `oid` for client `client_id`, if any.
fn find_invoice(
    config: &Config,
    oid: &str,
    client_id: &str,
) -> Result<Option<Map<String, Value>>, Failure> {
    let path = format!(
        "/invoices.json?period=all&oid={}&client_id={}",
        query_escape(oid),
        query_escape(client_id)
    );
    match host::fakturownia(config, "GET", &path, None)? {
        Http::Response { status, body } if status == STATUS_OK => {
            let invoices: Value = decode(&body, "invoice list")?;
            Ok(invoices
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_object)
                .find(|invoice| {
                    text(invoice.get("oid")).as_deref() == Some(oid)
                        && text(invoice.get("client_id")).as_deref() == Some(client_id)
                })
                .cloned())
        }
        Http::Response { status, .. } => Err(failed_read(status)),
        _ => Err(retry("could not look the invoice up")),
    }
}

/// Percent-encodes a query value.
pub fn query_escape(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            byte => format!("%{byte:02X}"),
        })
        .collect()
}

// ------------------------------------------------------------ inbound

/// What applying an invoice did.
#[derive(Debug, PartialEq, Eq)]
pub enum Applied {
    Updated,
    Linked,
    Unchanged,
    Ignored,
}

/// Applies an invoice's state to its source record, or links an invoice
/// made in Fakturownia by its order number.
pub fn apply_invoice(
    config: &Config,
    invoice: &Map<String, Value>,
    mode: &Mode,
) -> Result<Applied, Failure> {
    let Some(sync) = section(config) else {
        return Ok(Applied::Ignored);
    };
    let id = text(invoice.get("id")).ok_or_else(|| retry("invoice without id"))?;
    let context_id = context_id(sync, mode)?;
    let state = invoice_state(&config.base_url(), invoice);
    match host::mapping_by_external_id(&invoice_external_id(&id))? {
        Some(mapping) => {
            let record_id = mapping["record_id"].as_str().unwrap_or_default().to_owned();
            let remote = write_back_projection(&sync.write_back, &state);
            if mapping["content_hash"].as_str() == Some(write_back_hash(&remote).as_str()) {
                return Ok(Applied::Unchanged);
            }
            write_back(sync, mode, &context_id, &record_id, &id, &state)?;
            Ok(Applied::Updated)
        }
        None if sync.link_existing_by_oid => link(sync, mode, &context_id, &id, invoice, &state),
        None => Ok(Applied::Ignored),
    }
}

/// Links an invoice made in Fakturownia to the one source record whose
/// order-number attribute holds its `oid`.
fn link(
    sync: &InvoiceSync,
    mode: &Mode,
    context_id: &str,
    id: &str,
    invoice: &Map<String, Value>,
    state: &BTreeMap<&'static str, String>,
) -> Result<Applied, Failure> {
    let (Some(oid), Some(attribute)) = (text(invoice.get("oid")), sync.header.oid.as_deref())
    else {
        return Ok(Applied::Ignored);
    };
    let blueprint = host::blueprint(mode, &sync.source.blueprint)?.ok_or_else(|| {
        configuration_invalid(format!(
            "no published blueprint has the code {}",
            sync.source.blueprint
        ))
    })?;
    let attribute_id = blueprint
        .attribute_id(attribute)
        .ok_or_else(|| {
            configuration_invalid(format!(
                "{} has no attribute {attribute}",
                sync.source.blueprint
            ))
        })?
        .to_owned();
    let record_id = match host::lookup(mode, &blueprint, &attribute_id, &oid) {
        Ok(Some(record_id)) => record_id,
        Ok(None) => return Ok(Applied::Ignored),
        Err(error) => {
            host::log(
                "warn",
                &format!("invoice {id} with order number {oid} not linked: {error}"),
            );
            return Ok(Applied::Ignored);
        }
    };
    if let Some(existing) = host::mapping_by_record(&record_id)?
        && existing["external_id"]
            .as_str()
            .and_then(invoice_id)
            .is_some()
    {
        host::log(
            "warn",
            &format!("invoice {id} not linked: record {record_id} already has an invoice"),
        );
        return Ok(Applied::Ignored);
    }
    host::commit(
        &record_id,
        json!({"status": SYNCED, "external_id": invoice_external_id(id),
            "external_ids": {"number": state.get("number"), "token": invoice.get("token"), "linked": true}}),
    )?;
    write_back(sync, mode, context_id, &record_id, id, state)?;
    Ok(Applied::Linked)
}

/// A deleted invoice: clears its write-back fields and removes the mapping,
/// so the record can be invoiced again. The extension itself never deletes
/// an invoice.
pub fn deleted(config: &Config, id: &str, mode: &Mode) -> Result<bool, Failure> {
    let Some(sync) = section(config) else {
        return Ok(false);
    };
    let Some(mapping) = host::mapping_by_external_id(&invoice_external_id(id))? else {
        return Ok(false);
    };
    let record_id = mapping["record_id"].as_str().unwrap_or_default().to_owned();
    let context_id = context_id(sync, mode)?;
    let record = host::read_record(mode, &record_id, Some(&context_id))?;
    let writes = clear_writes(
        &sync.write_back,
        &local_write_back(&sync.write_back, &record.values),
    );
    apply_writes(mode, &context_id, &record_id, &writes, &["deleted", id])?;
    host::delete_mapping(&record_id)?;
    Ok(true)
}
