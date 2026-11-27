//! Host-independent sync logic between an Attricat blueprint and Fakturownia
//! clients: configuration, field ownership, the payloads in each direction,
//! the synced-state hash and webhook payload parsing.
//!
//! The sync keeps one **projection**: the mapped fields of a record as
//! Attricat holds them. Its hash, stored as the export mapping's content hash,
//! is the state both sides last agreed on.
//!
//! - Outbound, an unchanged projection hash means there is nothing to push
//!   (this also skips the events caused by the extension's own inbound writes).
//! - Inbound, Fakturownia's values for the fields it owns are merged into the
//!   projection. If nothing changes, the change was the echo of our own push or
//!   touched only unmapped fields, so nothing is written.

pub mod invoices;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// The secret that holds the Fakturownia API token when configuration names none.
pub const DEFAULT_TOKEN_SECRET: &str = "fakturownia-api-token";
/// The Fakturownia domain an account subdomain belongs to.
pub const API_DOMAIN: &str = "fakturownia.pl";
/// Root key of a Fakturownia client in request bodies.
pub const CLIENT_ROOT: &str = "client";

/// The Fakturownia client fields this extension synchronizes.
pub const CLIENT_FIELDS: [&str; 8] = [
    "name",
    "tax_no",
    "email",
    "phone",
    "street",
    "city",
    "post_code",
    "country",
];

/// Which side's edits of a field count.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Owner {
    /// Pushed to Fakturownia; Fakturownia edits are ignored.
    #[default]
    Attricat,
    /// Pulled from Fakturownia; never pushed.
    Fakturownia,
    /// Both directions; the most recent change wins.
    Both,
}

impl Owner {
    pub fn pushes(self) -> bool {
        matches!(self, Owner::Attricat | Owner::Both)
    }

    pub fn pulls(self) -> bool {
        matches!(self, Owner::Fakturownia | Owner::Both)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FieldMapping {
    /// The Fakturownia client field, one of [`CLIENT_FIELDS`].
    pub field: String,
    /// The Attricat attribute code.
    pub attribute: String,
    #[serde(default)]
    pub owner: Owner,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientSync {
    /// The code of the blueprint whose records are Fakturownia clients. New
    /// records are created on its current published revision.
    pub blueprint: String,
    /// The code of the context values are read and written in.
    #[serde(default = "default_context")]
    pub context: String,
    pub fields: Vec<FieldMapping>,
}

/// The context used when configuration names none.
pub const DEFAULT_CONTEXT: &str = "default";

/// Whether `text` is an Attricat code: a lowercase letter, then lowercase
/// letters, digits or underscores.
pub fn is_code(text: &str) -> bool {
    let mut bytes = text.bytes();
    bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        && text.len() <= 64
}

pub(crate) fn default_context() -> String {
    DEFAULT_CONTEXT.to_owned()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The account subdomain: `<account>.fakturownia.pl`.
    pub account: String,
    /// The extension secret holding the API token.
    #[serde(default)]
    pub token_secret: Option<String>,
    /// Ignored since 0.4.1 and kept so older configurations stay valid. The
    /// body's `api_token` is a value of the receiver's choosing, not the API
    /// token; the endpoint's unguessable URL authenticates deliveries.
    #[serde(default)]
    pub verify_webhook_token: Option<bool>,
    pub clients: ClientSync,
    /// Invoices issued from Attricat records; optional.
    #[serde(default)]
    pub invoices: Option<invoices::InvoiceSync>,
}

impl Config {
    pub fn parse(json: &str) -> Result<Self, String> {
        let config: Config = serde_json::from_str(json)
            .map_err(|error| format!("Invalid configuration: {error}"))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), String> {
        let account_ok = !self.account.is_empty()
            && self.account.len() <= 63
            && self
                .account
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
        if !account_ok {
            return Err("account must be a Fakturownia subdomain (a-z, 0-9, -)".into());
        }
        if self.clients.blueprint.is_empty() || self.clients.context.is_empty() {
            return Err("clients.blueprint and clients.context must be codes".into());
        }
        if self.clients.fields.is_empty() {
            return Err("clients.fields maps no field".into());
        }
        if let Some(invoices) = &self.invoices {
            invoices.validate(&self.clients.blueprint)?;
        }
        let mut seen = std::collections::BTreeSet::new();
        for mapping in &self.clients.fields {
            if !CLIENT_FIELDS.contains(&mapping.field.as_str()) {
                return Err(format!(
                    "unknown Fakturownia client field {}",
                    mapping.field
                ));
            }
            if !seen.insert(mapping.field.as_str()) {
                return Err(format!("client field {} is mapped twice", mapping.field));
            }
        }
        Ok(())
    }

    pub fn token_secret(&self) -> &str {
        self.token_secret.as_deref().unwrap_or(DEFAULT_TOKEN_SECRET)
    }

    pub fn base_url(&self) -> String {
        format!("https://{}.{API_DOMAIN}", self.account)
    }
}

/// Mapped field values by Fakturownia field name. An absent key is an empty value.
pub type Projection = BTreeMap<String, String>;

/// A scalar as text; empty strings, `null` and non-scalars are absent.
fn text(value: Option<&Value>) -> Option<String> {
    let text = match value? {
        Value::String(text) => text.trim().to_owned(),
        Value::Number(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        _ => return None,
    };
    (!text.is_empty()).then_some(text)
}

/// The projection of a record's values (by attribute code).
pub fn local_projection(sync: &ClientSync, values: &Map<String, Value>) -> Projection {
    sync.fields
        .iter()
        .filter_map(|mapping| {
            text(values.get(&mapping.attribute)).map(|value| (mapping.field.clone(), value))
        })
        .collect()
}

/// The hash stored as the mapping's content hash.
pub fn projection_hash(projection: &Projection) -> String {
    let canonical = serde_json::to_vec(projection).expect("a string map serializes");
    let digest = Sha256::digest(canonical);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The `client` body to send: every field Attricat pushes, with empty
/// values cleared on Fakturownia's side. Creating a client seeds every mapped
/// field; ownership decides who may change a field once both sides have it.
pub fn outbound_client(
    sync: &ClientSync,
    projection: &Projection,
    creating: bool,
) -> Map<String, Value> {
    sync.fields
        .iter()
        .filter(|mapping| creating || mapping.owner.pushes())
        .map(|mapping| {
            let value = projection.get(&mapping.field).cloned().unwrap_or_default();
            (mapping.field.clone(), Value::String(value))
        })
        .collect()
}

/// One attribute write an inbound change needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboundWrite {
    pub attribute: String,
    /// `None` clears the value.
    pub value: Option<String>,
}

/// What an inbound Fakturownia client changes locally.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Merge {
    pub projection: Projection,
    pub writes: Vec<InboundWrite>,
}

/// Merges a Fakturownia client into the local projection. Fields Attricat
/// alone owns keep their local value.
pub fn merge_inbound(sync: &ClientSync, local: &Projection, remote: &Map<String, Value>) -> Merge {
    let mut projection = local.clone();
    let mut writes = Vec::new();
    for mapping in sync.fields.iter().filter(|mapping| mapping.owner.pulls()) {
        let incoming = text(remote.get(&mapping.field));
        if incoming.as_ref() == local.get(&mapping.field) {
            continue;
        }
        match &incoming {
            Some(value) => projection.insert(mapping.field.clone(), value.clone()),
            None => projection.remove(&mapping.field),
        };
        writes.push(InboundWrite {
            attribute: mapping.attribute.clone(),
            value: incoming,
        });
    }
    Merge { projection, writes }
}

/// The resource kinds a webhook may announce.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resource {
    Client,
    Product,
    Invoice,
    Payment,
}

impl Resource {
    pub fn name(self) -> &'static str {
        match self {
            Resource::Client => "client",
            Resource::Product => "product",
            Resource::Invoice => "invoice",
            Resource::Payment => "payment",
        }
    }
}

/// What a webhook announces. The body is only a hint: the handler fetches the
/// object again before it changes anything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Announcement {
    pub resource: Resource,
    /// The action from the registered URL's `?kind=<resource>:<action>`, such
    /// as `destroy`, or `change` when the URL names none. Fakturownia's body
    /// does not say which event fired.
    pub action: String,
    pub id: i64,
}

/// The top-level keys Fakturownia puts the changed object under, and what
/// each announces. An invoice arrives as `deal`; its body also nests the
/// buyer under `deal.client`, so invoices are matched before clients.
const WEBHOOK_OBJECTS: [(&str, Resource); 5] = [
    ("deal", Resource::Invoice),
    ("invoice", Resource::Invoice),
    ("payment", Resource::Payment),
    ("product", Resource::Product),
    ("client", Resource::Client),
];

/// Parses a Fakturownia webhook delivery, as documented at
/// pomoc.fakturownia.pl ("Integracja danych z Fakturowni za pomocą
/// webhooków"): `{"client": {...}, "app_name": "fakturownia",
/// "api_token": "...", "locale": "pl"}`, with the object under `client`,
/// `deal` (invoices) or `product`. The body names no event, so the action
/// comes from `kind=<resource>:<action>` in the registered URL's `query`, if
/// there is one. `api_token` is ignored.
pub fn parse_webhook(body: &str, query: &str) -> Result<Announcement, String> {
    let body: Value = serde_json::from_str(body).map_err(|_| "body is not JSON".to_owned())?;
    let (object, resource) = WEBHOOK_OBJECTS
        .iter()
        .find_map(|(key, resource)| Some((body.get(*key)?.as_object()?, *resource)))
        .ok_or("no client, deal or product object")?;
    let id = object
        .get("id")
        .or_else(|| body.get("id"))
        .and_then(|id| id.as_i64().or_else(|| id.as_str()?.parse().ok()))
        .ok_or("missing object id")?;
    Ok(Announcement {
        resource,
        action: query_action(query).unwrap_or_else(|| "change".to_owned()),
        id,
    })
}

/// The action of a `kind=<resource>:<action>` query parameter, with the colon
/// either literal or percent-encoded.
fn query_action(query: &str) -> Option<String> {
    let kind = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("kind="))?
        .replace("%3A", ":")
        .replace("%3a", ":");
    let (_, action) = kind.split_once(':')?;
    let valid = !action.is_empty()
        && action.len() <= 32
        && action
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte == b'_');
    valid.then(|| action.to_owned())
}

/// Fakturownia's validation error body: `{"code":"error","message":{"field":["- text"]}}`.
pub fn validation_message(body: &str) -> String {
    let Ok(body) = serde_json::from_str::<Value>(body) else {
        return "Fakturownia rejected the request".into();
    };
    match body.get("message") {
        Some(Value::Object(fields)) => fields
            .iter()
            .map(|(field, messages)| {
                let messages = match messages {
                    Value::Array(items) => items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(|text| text.trim_start_matches("- "))
                        .collect::<Vec<_>>()
                        .join(", "),
                    other => other.to_string(),
                };
                format!("{field}: {messages}")
            })
            .collect::<Vec<_>>()
            .join("; "),
        Some(Value::String(text)) => text.clone(),
        _ => "Fakturownia rejected the request".into(),
    }
}

/// Milliseconds since the Unix epoch of an RFC 3339 timestamp such as
/// Fakturownia's `2026-11-25T10:00:00.000+01:00`, so timestamps with different
/// offsets compare correctly.
pub fn timestamp_millis(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    let number = |range: std::ops::Range<usize>| -> Option<i64> {
        std::str::from_utf8(bytes.get(range)?).ok()?.parse().ok()
    };
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[13] != b':' {
        return None;
    }
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    let mut index = 19;
    let mut millis = 0;
    if bytes.get(index) == Some(&b'.') {
        let start = index + 1;
        index = start;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        let fraction = std::str::from_utf8(&bytes[start..index]).ok()?;
        let padded = format!("{fraction:0<3}");
        millis = padded.get(..3)?.parse().ok()?;
    }
    let offset_minutes = match bytes.get(index)? {
        b'Z' | b'z' => 0,
        sign @ (b'+' | b'-') => {
            let minutes = number(index + 1..index + 3)? * 60 + number(index + 4..index + 6)?;
            if *sign == b'-' { -minutes } else { minutes }
        }
        _ => return None,
    };
    // Days from the civil date (Howard Hinnant's algorithm).
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    let seconds = days * 86_400 + hour * 3_600 + minute * 60 + second - offset_minutes * 60;
    Some(seconds * 1_000 + millis)
}

#[cfg(test)]
mod tests {
    #[test]
    fn timestamps_compare_across_offsets() {
        assert_eq!(timestamp_millis("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            timestamp_millis("2026-11-25T10:00:00.250+01:00"),
            timestamp_millis("2026-11-25T09:00:00.25Z")
        );
        assert_eq!(
            timestamp_millis("2000-03-01T00:00:00Z"),
            Some(951_868_800_000)
        );
        assert!(timestamp_millis("yesterday").is_none());
    }

    use super::*;
    use serde_json::json;

    fn sync() -> ClientSync {
        ClientSync {
            blueprint: "client".into(),
            context: DEFAULT_CONTEXT.into(),
            fields: vec![
                FieldMapping {
                    field: "name".into(),
                    attribute: "name".into(),
                    owner: Owner::Both,
                },
                FieldMapping {
                    field: "tax_no".into(),
                    attribute: "nip".into(),
                    owner: Owner::Attricat,
                },
                FieldMapping {
                    field: "email".into(),
                    attribute: "email".into(),
                    owner: Owner::Fakturownia,
                },
            ],
        }
    }

    fn object(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn outbound_sends_only_pushed_fields_and_clears_empty_ones() {
        let projection = local_projection(
            &sync(),
            &object(json!({"name": " Acme ", "email": "a@b.c", "other": 1})),
        );
        assert_eq!(projection.get("name").unwrap(), "Acme");
        let body = outbound_client(&sync(), &projection, false);
        assert_eq!(Value::Object(body), json!({"name": "Acme", "tax_no": ""}));
        let created = outbound_client(&sync(), &projection, true);
        assert_eq!(
            Value::Object(created),
            json!({"name": "Acme", "tax_no": "", "email": "a@b.c"})
        );
    }

    #[test]
    fn the_echo_of_a_push_changes_nothing() {
        let local = local_projection(
            &sync(),
            &object(json!({"name": "Acme", "nip": "123", "email": "a@b.c"})),
        );
        let remote = object(json!({"id": 7, "name": "Acme", "tax_no": "999", "email": "a@b.c"}));
        let merge = merge_inbound(&sync(), &local, &remote);
        assert!(merge.writes.is_empty());
        assert_eq!(projection_hash(&merge.projection), projection_hash(&local));
    }

    #[test]
    fn inbound_writes_pulled_fields_and_ignores_attricat_owned_ones() {
        let local = local_projection(&sync(), &object(json!({"name": "Acme", "nip": "123"})));
        let remote = object(json!({"name": "Acme SA", "tax_no": "999", "email": "new@b.c"}));
        let merge = merge_inbound(&sync(), &local, &remote);
        assert_eq!(
            merge.writes,
            vec![
                InboundWrite {
                    attribute: "name".into(),
                    value: Some("Acme SA".into())
                },
                InboundWrite {
                    attribute: "email".into(),
                    value: Some("new@b.c".into())
                },
            ]
        );
        assert_eq!(merge.projection.get("tax_no").unwrap(), "123");
    }

    #[test]
    fn an_emptied_remote_field_clears_the_local_value() {
        let local = local_projection(&sync(), &object(json!({"email": "a@b.c"})));
        let merge = merge_inbound(&sync(), &local, &object(json!({"email": ""})));
        assert_eq!(
            merge.writes,
            vec![InboundWrite {
                attribute: "email".into(),
                value: None
            }]
        );
    }

    #[test]
    fn webhooks_parse_the_documented_format() {
        // Real deliveries carry no updated_at and name no event.
        let client = json!({"client": {"id": 42, "name": "Acme"},
            "app_name": "fakturownia", "api_token": "", "locale": "pl"})
        .to_string();
        let announcement = parse_webhook(&client, "").unwrap();
        assert_eq!(
            (
                announcement.resource,
                announcement.id,
                announcement.action.as_str()
            ),
            (Resource::Client, 42, "change")
        );
        assert_eq!(
            parse_webhook(&client, "kind=client%3Adestroy")
                .unwrap()
                .action,
            "destroy"
        );
        assert_eq!(
            parse_webhook(&client, "a=1&kind=client:update")
                .unwrap()
                .action,
            "update"
        );
        assert_eq!(
            parse_webhook(&client, "kind=client:DROP%20TABLE")
                .unwrap()
                .action,
            "change"
        );

        // An invoice arrives as `deal`, with its buyer nested inside it.
        let invoice = parse_webhook(
            &json!({"id": 9, "deal": {"id": "9", "invoice_no": "FV 1/11/2026",
                "client": {"id": 42}}, "app_name": "fakturownia", "api_token": "",
                "locale": "pl"})
            .to_string(),
            "",
        )
        .unwrap();
        assert_eq!((invoice.resource, invoice.id), (Resource::Invoice, 9));
        let product = parse_webhook(r#"{"product":{"id":7},"app_name":"fakturownia"}"#, "");
        assert_eq!(product.unwrap().resource, Resource::Product);

        assert!(parse_webhook(r#"{"app_name":"fakturownia","api_token":""}"#, "").is_err());
        assert!(parse_webhook(r#"{"client":{"name":"no id"}}"#, "").is_err());
        assert!(parse_webhook("not json", "").is_err());
    }

    #[test]
    fn configuration_rejects_unknown_and_duplicate_fields() {
        let config = |fields: Value, account: &str| {
            json!({"account": account, "clients": {"blueprint": "client",
                "fields": fields}})
            .to_string()
        };
        assert!(
            Config::parse(&config(
                json!([{"field": "name", "attribute": "name"}]),
                "acme"
            ))
            .is_ok()
        );
        let parsed = Config::parse(&config(
            json!([{"field": "name", "attribute": "name"}]),
            "acme",
        ))
        .unwrap();
        assert_eq!(parsed.clients.context, "default");
        assert!(
            Config::parse(
                &json!({"account": "acme", "clients": {"blueprint_id": "b",
                    "fields": [{"field": "name", "attribute": "name"}]}})
                .to_string()
            )
            .is_err(),
            "the UUID form of earlier releases is refused"
        );
        assert!(
            Config::parse(&config(
                json!([{"field": "nope", "attribute": "x"}]),
                "acme"
            ))
            .is_err()
        );
        assert!(
            Config::parse(&config(
                json!([{"field": "name", "attribute": "a"}, {"field": "name", "attribute": "b"}]),
                "acme"
            ))
            .is_err()
        );
        assert!(
            Config::parse(&config(
                json!([{"field": "name", "attribute": "name"}]),
                "evil.com/x"
            ))
            .is_err()
        );
    }

    #[test]
    fn validation_errors_read_as_text() {
        assert_eq!(
            validation_message(r#"{"code":"error","message":{"name":["- nie może być puste"]}}"#),
            "name: nie może być puste"
        );
    }
}
