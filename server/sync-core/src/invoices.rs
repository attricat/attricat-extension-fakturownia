//! Host-independent rules for issuing Fakturownia invoices from Attricat
//! records: the `invoices` configuration and its validation against the
//! catalog, the positions and body of an invoice, and the projection of an
//! invoice's state written back to its source record.
//!
//! An invoice is issued once per source record. The source record's export
//! mapping holds the invoice under [`INVOICE_EXTERNAL_ID_PREFIX`], so invoice
//! and client mappings never share an external ID. Everything written back is
//! owned by Fakturownia: the mapping's content hash is the hash of the
//! write-back projection both sides last agreed on, as for clients.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::{InboundWrite, Projection, projection_hash};

/// Root key of a Fakturownia invoice in request bodies.
pub const INVOICE_ROOT: &str = "invoice";
/// The prefix of an invoice's external ID in the source record's mapping.
pub const INVOICE_EXTERNAL_ID_PREFIX: &str = "invoice:";
/// The most relationship hops a position path follows before its leaf.
pub const MAX_PATH_HOPS: usize = 3;
/// Fixed tax rates a configuration may default to.
pub const TAX_RATES: [&str; 6] = ["23", "8", "5", "0", "zw", "np"];
/// The invoice kind used when configuration names none.
pub const DEFAULT_KIND: &str = "vat";
/// Invoice kinds this extension issues: sales invoices that move no money
/// in advance and correct nothing.
pub const INVOICE_KINDS: [&str; 2] = ["vat", "bill"];
/// Fakturownia invoice statuses (documented).
pub const INVOICE_STATUSES: [&str; 5] = ["issued", "sent", "paid", "partial", "rejected"];
/// The status of a paid invoice.
pub const PAID: &str = "paid";

/// Attribute value types, as the catalog names them.
pub const STRING: &str = "string";
pub const RELATIONSHIP: &str = "relationship";
pub const FILE: &str = "file";
/// Value types a header date may come from.
const DATE_TYPES: [&str; 2] = ["date", STRING];
/// Value types a position number may come from.
const NUMBER_TYPES: [&str; 3] = ["integer", "number", STRING];

/// The source records invoices are issued from.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub blueprint: String,
    #[serde(default = "crate::default_context")]
    pub context: String,
}

/// The invoice header. Dates, `oid` and `description` are attribute codes
/// on the source (`None`: Fakturownia's default date, or nothing); the rest
/// are fixed values.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Header {
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(default)]
    pub issue_date: Option<String>,
    #[serde(default)]
    pub sell_date: Option<String>,
    /// The source's number, sent as Fakturownia's order number. Without it
    /// the source record's ID is sent.
    #[serde(default)]
    pub oid: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub payment_to_days: Option<u32>,
    #[serde(default)]
    pub payment_type: Option<String>,
    #[serde(default)]
    pub currency: Option<String>,
}

fn default_kind() -> String {
    DEFAULT_KIND.to_owned()
}

impl Default for Header {
    fn default() -> Self {
        Self {
            kind: default_kind(),
            issue_date: None,
            sell_date: None,
            oid: None,
            description: None,
            payment_to_days: None,
            payment_type: None,
            currency: None,
        }
    }
}

/// The line records of a source: records of `blueprint` whose `parent`
/// relationship points at it.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Lines {
    pub blueprint: String,
    pub parent: String,
}

/// A position's tax: an attribute path, a fixed rate, or a path with a
/// fixed rate for positions where it is empty.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Tax {
    #[serde(default)]
    pub attribute: Option<String>,
    #[serde(default)]
    pub default: Option<String>,
}

/// Where each position value comes from: attribute paths on the line (or
/// on the source, without `lines`).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Positions {
    #[serde(default)]
    pub lines: Option<Lines>,
    pub name: String,
    #[serde(default)]
    pub code: Option<String>,
    pub quantity: String,
    #[serde(default)]
    pub unit: Option<String>,
    pub price_net: String,
    pub tax: Tax,
}

impl Positions {
    /// Every configured path with its position field, in body order.
    pub fn paths(&self) -> Vec<(&'static str, &str)> {
        let mut paths = vec![("name", self.name.as_str())];
        paths.extend(self.code.as_deref().map(|path| ("code", path)));
        paths.push(("quantity", self.quantity.as_str()));
        paths.extend(self.unit.as_deref().map(|path| ("quantity_unit", path)));
        paths.push(("price_net", self.price_net.as_str()));
        paths.extend(self.tax.attribute.as_deref().map(|path| ("tax", path)));
        paths
    }
}

/// Source attributes the invoice's state is written to. All optional.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WriteBack {
    #[serde(default)]
    pub number: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub ksef_number: Option<String>,
    #[serde(default)]
    pub ksef_status: Option<String>,
    /// A file attribute receiving the invoice PDF.
    #[serde(default)]
    pub file: Option<String>,
}

impl WriteBack {
    /// The scalar write-back fields: `(invoice state key, attribute code)`.
    pub fn scalars(&self) -> Vec<(&'static str, &str)> {
        [
            ("number", &self.number),
            ("status", &self.status),
            ("url", &self.url),
            ("ksef_number", &self.ksef_number),
            ("ksef_status", &self.ksef_status),
        ]
        .into_iter()
        .filter_map(|(key, attribute)| attribute.as_deref().map(|attribute| (key, attribute)))
        .collect()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InvoiceSync {
    pub source: Source,
    /// A relationship on the source to a record of the `clients` blueprint.
    pub buyer: String,
    #[serde(default)]
    pub header: Header,
    pub positions: Positions,
    #[serde(default)]
    pub write_back: WriteBack,
    /// Link invoices made in Fakturownia to the source record whose
    /// `header.oid` attribute holds their order number.
    #[serde(default)]
    pub link_existing_by_oid: bool,
}

/// A path's segments: relationship codes, then the leaf.
pub fn path_segments(path: &str) -> Result<Vec<&str>, String> {
    let segments: Vec<&str> = path.split('.').collect();
    if segments.len() > MAX_PATH_HOPS + 1 {
        return Err(format!(
            "path {path} has more than {MAX_PATH_HOPS} relationship hops"
        ));
    }
    if segments.iter().any(|segment| !crate::is_code(segment)) {
        return Err(format!("path {path} is not a dotted list of codes"));
    }
    Ok(segments)
}

impl InvoiceSync {
    /// Checks what needs no catalog: codes, paths, fixed values.
    pub fn validate(&self, clients_blueprint: &str) -> Result<(), String> {
        if !crate::is_code(&self.source.blueprint) || self.source.context.is_empty() {
            return Err("invoices.source needs a blueprint code and a context code".into());
        }
        if self.source.blueprint == clients_blueprint {
            return Err("invoices.source.blueprint must not be the clients blueprint".into());
        }
        if !INVOICE_KINDS.contains(&self.header.kind.as_str()) {
            return Err(format!(
                "invoices.header.kind must be one of {}",
                INVOICE_KINDS.join(", ")
            ));
        }
        for path in self.positions.paths().into_iter().map(|(_, path)| path) {
            path_segments(path).map_err(|error| format!("invoices.positions: {error}"))?;
        }
        match (&self.positions.tax.attribute, &self.positions.tax.default) {
            (None, None) => {
                return Err("invoices.positions.tax needs an attribute or a default".into());
            }
            (_, Some(rate)) if !TAX_RATES.contains(&rate.as_str()) => {
                return Err(format!(
                    "invoices.positions.tax.default must be one of {}",
                    TAX_RATES.join(", ")
                ));
            }
            _ => {}
        }
        if self.link_existing_by_oid && self.header.oid.is_none() {
            return Err("invoices.link_existing_by_oid needs invoices.header.oid".into());
        }
        Ok(())
    }

    /// The fields the attribute `code` of the source holds for the header.
    fn header_attributes(&self) -> Vec<(&'static str, &str, &'static [&'static str])> {
        [
            ("issue_date", &self.header.issue_date, &DATE_TYPES[..]),
            ("sell_date", &self.header.sell_date, &DATE_TYPES[..]),
            ("oid", &self.header.oid, &[STRING, "integer"][..]),
            (
                "description",
                &self.header.description,
                &[STRING, "text"][..],
            ),
        ]
        .into_iter()
        .filter_map(|(field, attribute, types)| {
            attribute
                .as_deref()
                .map(|attribute| (field, attribute, types))
        })
        .collect()
    }
}

// ------------------------------------------------------------ catalog checks

/// An attribute of a published blueprint revision, as validation needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttributeShape {
    pub value_type: String,
    /// Target blueprint codes of a relationship.
    pub targets: Vec<String>,
}

/// A published blueprint revision's attributes by code.
pub type BlueprintShape = BTreeMap<String, AttributeShape>;

/// Reads the shape of a blueprint's current published revision.
pub trait Catalog {
    fn blueprint(&mut self, code: &str) -> Result<Option<BlueprintShape>, String>;
}

/// The shape of a `blueprint` read of `attricat.read`: its `attributes`
/// with `code`, `value_type` and `target_blueprint_code(s)`.
pub fn blueprint_shape(read: &Value) -> Option<BlueprintShape> {
    let attributes = read.get("attributes")?.as_array()?;
    Some(
        attributes
            .iter()
            .filter_map(|attribute| {
                let code = attribute.get("code")?.as_str()?.to_owned();
                let mut targets: Vec<String> = attribute
                    .get("target_blueprint_codes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|target| target.as_str().map(str::to_owned))
                    .collect();
                if let Some(target) = attribute
                    .get("target_blueprint_code")
                    .and_then(Value::as_str)
                    && !targets.iter().any(|known| known == target)
                {
                    targets.push(target.to_owned());
                }
                Some((
                    code,
                    AttributeShape {
                        value_type: attribute.get("value_type")?.as_str()?.to_owned(),
                        targets,
                    },
                ))
            })
            .collect(),
    )
}

fn shape(catalog: &mut dyn Catalog, code: &str) -> Result<BlueprintShape, String> {
    catalog
        .blueprint(code)?
        .ok_or_else(|| format!("no published blueprint has the code {code}"))
}

fn attribute<'a>(
    blueprint: &'a BlueprintShape,
    blueprint_code: &str,
    code: &str,
) -> Result<&'a AttributeShape, String> {
    blueprint
        .get(code)
        .ok_or_else(|| format!("blueprint {blueprint_code} has no attribute {code}"))
}

fn expect_type(
    found: &AttributeShape,
    blueprint: &str,
    code: &str,
    types: &[&str],
) -> Result<(), String> {
    if types.contains(&found.value_type.as_str()) {
        Ok(())
    } else {
        Err(format!(
            "{blueprint}.{code} is a {} attribute; expected {}",
            found.value_type,
            types.join(" or ")
        ))
    }
}

/// Checks that `path` leads, from a record of `start`, through relationships
/// to a leaf of one of `leaf_types` on at least one target blueprint. A
/// multi-target relationship may reach other targets without the leaf.
fn check_path(
    catalog: &mut dyn Catalog,
    start: &str,
    path: &str,
    leaf_types: &[&str],
) -> Result<(), String> {
    let segments = path_segments(path)?;
    let (leaf, hops) = segments.split_last().expect("split yields one segment");
    let mut blueprints = vec![start.to_owned()];
    for hop in hops {
        let mut next = Vec::new();
        for code in &blueprints {
            let blueprint = shape(catalog, code)?;
            let Some(found) = blueprint.get(*hop) else {
                continue;
            };
            if found.value_type != RELATIONSHIP {
                return Err(format!("{code}.{hop} in {path} is not a relationship"));
            }
            next.extend(found.targets.iter().cloned());
        }
        if next.is_empty() {
            return Err(format!("{path}: no blueprint along it has {hop}"));
        }
        next.sort();
        next.dedup();
        blueprints = next;
    }
    let mut seen = false;
    for code in &blueprints {
        if let Some(found) = shape(catalog, code)?.get(*leaf) {
            expect_type(found, code, leaf, leaf_types)?;
            seen = true;
        }
    }
    if seen {
        Ok(())
    } else {
        Err(format!("{path}: no target blueprint has {leaf}"))
    }
}

/// Checks the `invoices` section against the catalog's published
/// revisions: every code exists and has a fitting type, and every path
/// resolves.
pub fn validate_against(
    sync: &InvoiceSync,
    clients_blueprint: &str,
    catalog: &mut dyn Catalog,
) -> Result<(), String> {
    sync.validate(clients_blueprint)?;
    let source_code = sync.source.blueprint.as_str();
    let source = shape(catalog, source_code)?;
    let buyer = attribute(&source, source_code, &sync.buyer)?;
    expect_type(buyer, source_code, &sync.buyer, &[RELATIONSHIP])?;
    if !buyer
        .targets
        .iter()
        .any(|target| target == clients_blueprint)
    {
        return Err(format!(
            "{source_code}.{} does not point at the clients blueprint {clients_blueprint}",
            sync.buyer
        ));
    }
    for (_, code, types) in sync.header_attributes() {
        expect_type(
            attribute(&source, source_code, code)?,
            source_code,
            code,
            types,
        )?;
    }
    for (_, code) in sync.write_back.scalars() {
        expect_type(
            attribute(&source, source_code, code)?,
            source_code,
            code,
            &[STRING],
        )?;
    }
    if let Some(code) = &sync.write_back.file {
        expect_type(
            attribute(&source, source_code, code)?,
            source_code,
            code,
            &[FILE],
        )?;
    }
    let position_blueprint = match &sync.positions.lines {
        Some(lines) => {
            let line = shape(catalog, &lines.blueprint)?;
            let parent = attribute(&line, &lines.blueprint, &lines.parent)?;
            expect_type(parent, &lines.blueprint, &lines.parent, &[RELATIONSHIP])?;
            if !parent.targets.iter().any(|target| target == source_code) {
                return Err(format!(
                    "{}.{} does not point at {source_code}",
                    lines.blueprint, lines.parent
                ));
            }
            lines.blueprint.clone()
        }
        None => source_code.to_owned(),
    };
    for (field, path) in sync.positions.paths() {
        let types: &[&str] = match field {
            "quantity" | "price_net" => &NUMBER_TYPES,
            "tax" => &NUMBER_TYPES,
            _ => &[STRING],
        };
        check_path(catalog, &position_blueprint, path, types)
            .map_err(|error| format!("invoices.positions.{field}: {error}"))?;
    }
    Ok(())
}

// ------------------------------------------------------------ positions

/// The text of each position field, as read along its path; `None` is empty.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PositionValues {
    pub name: Option<String>,
    pub code: Option<String>,
    pub quantity: Option<String>,
    pub unit: Option<String>,
    pub price_net: Option<String>,
    pub tax: Option<String>,
}

/// A decimal as Fakturownia expects it, with a decimal point.
fn decimal(text: &str) -> Option<String> {
    let normalized = text.trim().replace(',', ".");
    normalized
        .parse::<f64>()
        .ok()
        .filter(|number| number.is_finite())
        .map(|_| normalized)
}

/// A position's tax as Fakturownia expects it: a rate without a percent
/// sign, `zw` or `np`.
fn tax_rate(text: &str) -> Option<String> {
    let text = text.trim().trim_end_matches('%').trim().to_lowercase();
    match text.as_str() {
        "zw" | "np" => Some(text),
        _ => decimal(&text).map(|rate| rate.trim_end_matches(".0").to_owned()),
    }
}

/// A non-negative decimal as an exact mantissa and scale: `19.11` is
/// `(1911, 2)`. At most 12 digits after the point and 18 in all.
fn fixed(text: &str) -> Option<(i128, u32)> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    let digits = format!("{whole}{fraction}");
    let valid = !digits.is_empty()
        && digits.len() <= 18
        && fraction.len() <= 12
        && digits.bytes().all(|byte| byte.is_ascii_digit());
    valid.then(|| Some((digits.parse().ok()?, fraction.len() as u32)))?
}

/// `mantissa / 10^scale` rounded half up to a whole number.
fn round_whole(mantissa: i128, scale: u32) -> Option<i128> {
    let divisor = 10_i128.checked_pow(scale)?;
    Some(mantissa.checked_add(divisor / 2)? / divisor)
}

/// `mantissa / 10^scale` rounded half up to grosze (hundredths).
fn round_to_grosze(mantissa: i128, scale: u32) -> Option<i128> {
    match scale.checked_sub(2) {
        Some(excess) => round_whole(mantissa, excess),
        None => mantissa.checked_mul(10_i128.checked_pow(2 - scale)?),
    }
}

/// A position's gross value in grosze, as an invoice computes it: the net
/// value (price × quantity) rounded to the grosz, plus the VAT on that net
/// value rounded to the grosz. `zw` and `np` carry no VAT.
fn gross_grosze(price_net: &str, quantity: &str, rate: &str) -> Option<i128> {
    let (price, price_scale) = fixed(price_net)?;
    let (amount, amount_scale) = fixed(quantity)?;
    let net = round_to_grosze(price.checked_mul(amount)?, price_scale + amount_scale)?;
    let vat = match rate {
        "zw" | "np" => 0,
        _ => {
            let (rate, rate_scale) = fixed(rate)?;
            // net (grosze) × rate / 100, in grosze.
            round_whole(net.checked_mul(rate)?, rate_scale + 2)?
        }
    };
    net.checked_add(vat)
}

/// Grosze as a Fakturownia amount: `23505` is `"235.05"`.
fn amount(grosze: i128) -> String {
    format!("{}.{:02}", grosze / 100, grosze % 100)
}

/// Builds one invoice position, or every reason it cannot be sent, each
/// naming `label` (the record it comes from) and the field.
pub fn position(
    positions: &Positions,
    label: &str,
    values: &PositionValues,
) -> Result<Map<String, Value>, Vec<String>> {
    let mut errors = Vec::new();
    let mut body = Map::new();
    let missing = |field: &str| format!("{label}: {field} is empty");
    match &values.name {
        Some(name) => {
            body.insert("name".into(), json!(name));
        }
        None => errors.push(missing("name")),
    }
    if let Some(code) = &values.code {
        body.insert("code".into(), json!(code));
    }
    match values.quantity.as_deref().map(|text| (text, decimal(text))) {
        Some((_, Some(quantity))) if quantity.parse::<f64>().is_ok_and(|q| q > 0.0) => {
            body.insert("quantity".into(), json!(quantity));
        }
        Some((text, _)) => {
            errors.push(format!("{label}: quantity {text} is not a positive number"))
        }
        None => errors.push(missing("quantity")),
    }
    if let Some(unit) = &values.unit {
        body.insert("quantity_unit".into(), json!(unit));
    }
    match values
        .price_net
        .as_deref()
        .map(|text| (text, decimal(text)))
    {
        Some((_, Some(price))) if price.parse::<f64>().is_ok_and(|p| p >= 0.0) => {
            body.insert("price_net".into(), json!(price));
        }
        Some((text, _)) => errors.push(format!("{label}: price_net {text} is not a price")),
        None => errors.push(missing("price_net")),
    }
    match values
        .tax
        .as_deref()
        .or(positions.tax.default.as_deref())
        .map(|text| (text, tax_rate(text)))
    {
        Some((_, Some(rate))) => {
            body.insert("tax".into(), json!(rate));
        }
        Some((text, None)) => errors.push(format!("{label}: tax {text} is not a rate")),
        None => errors.push(missing("tax")),
    }
    // Fakturownia derives the other amounts but not the gross total, which a
    // position without a product must carry.
    if let (Some(Value::String(price)), Some(Value::String(quantity)), Some(Value::String(rate))) =
        (body.get("price_net"), body.get("quantity"), body.get("tax"))
    {
        match gross_grosze(price, quantity, rate) {
            Some(gross) => {
                body.insert("total_price_gross".into(), json!(amount(gross)));
            }
            None => errors.push(format!(
                "{label}: price_net {price}, quantity {quantity} and tax {rate} give no gross amount"
            )),
        }
    }
    if errors.is_empty() {
        Ok(body)
    } else {
        Err(errors)
    }
}

// ------------------------------------------------------------ the invoice

/// The header text read from the source record, by header field.
pub type HeaderValues = BTreeMap<&'static str, String>;

impl InvoiceSync {
    /// The source attributes to read for the header, by field.
    pub fn header_reads(&self) -> Vec<(&'static str, &str)> {
        self.header_attributes()
            .into_iter()
            .map(|(field, code, _)| (field, code))
            .collect()
    }

    /// The order number sent as `oid`: the configured attribute's value, or
    /// the source record's ID.
    pub fn oid(&self, header: &HeaderValues, record_id: &str) -> String {
        header
            .get("oid")
            .cloned()
            .unwrap_or_else(|| record_id.to_owned())
    }

    /// The `invoice` body to create. Positions carry no product ID, so the
    /// invoice moves no Fakturownia stock; `exclude_from_stock_level` says
    /// so explicitly. `oid_unique` makes Fakturownia refuse a second invoice
    /// with the same order number.
    pub fn invoice_body(
        &self,
        client_id: &str,
        oid: &str,
        header: &HeaderValues,
        positions: Vec<Map<String, Value>>,
    ) -> Value {
        let mut invoice = Map::new();
        invoice.insert("kind".into(), json!(self.header.kind));
        invoice.insert(
            "client_id".into(),
            client_id
                .parse::<i64>()
                .map(Value::from)
                .unwrap_or_else(|_| json!(client_id)),
        );
        invoice.insert("oid".into(), json!(oid));
        invoice.insert("oid_unique".into(), json!("yes"));
        for field in ["issue_date", "sell_date", "description"] {
            if let Some(value) = header.get(field) {
                invoice.insert(field.into(), json!(value));
            }
        }
        if let Some(days) = self.header.payment_to_days {
            invoice.insert("payment_to_kind".into(), json!(days.to_string()));
        }
        if let Some(payment_type) = &self.header.payment_type {
            invoice.insert("payment_type".into(), json!(payment_type));
        }
        if let Some(currency) = &self.header.currency {
            invoice.insert("currency".into(), json!(currency));
        }
        invoice.insert("exclude_from_stock_level".into(), json!(true));
        invoice.insert(
            "positions".into(),
            Value::Array(positions.into_iter().map(Value::Object).collect()),
        );
        json!({ INVOICE_ROOT: invoice })
    }
}

/// The invoice's ID in the source record's mapping.
pub fn invoice_external_id(id: &str) -> String {
    format!("{INVOICE_EXTERNAL_ID_PREFIX}{id}")
}

/// The Fakturownia invoice ID of a mapping's external ID, if it is one.
pub fn invoice_id(external_id: &str) -> Option<&str> {
    external_id.strip_prefix(INVOICE_EXTERNAL_ID_PREFIX)
}

fn text(value: Option<&Value>) -> Option<String> {
    let text = match value? {
        Value::String(text) => text.trim().to_owned(),
        Value::Number(number) => number.to_string(),
        _ => return None,
    };
    (!text.is_empty()).then_some(text)
}

/// The invoice's view URL: Fakturownia serves `/invoice/{token}` without a
/// login (documented).
pub fn view_url(base_url: &str, invoice: &Map<String, Value>) -> Option<String> {
    text(invoice.get("token")).map(|token| format!("{base_url}/invoice/{token}"))
}

/// The invoice state Attricat keeps, by state key: number, status, view
/// URL, KSeF number (`gov_id`) and KSeF status (`gov_status`).
pub fn invoice_state(
    base_url: &str,
    invoice: &Map<String, Value>,
) -> BTreeMap<&'static str, String> {
    [
        ("number", text(invoice.get("number"))),
        ("status", text(invoice.get("status"))),
        ("url", view_url(base_url, invoice)),
        ("ksef_number", text(invoice.get("gov_id"))),
        ("ksef_status", text(invoice.get("gov_status"))),
    ]
    .into_iter()
    .filter_map(|(key, value)| value.map(|value| (key, value)))
    .collect()
}

/// The write-back projection of an invoice: its state by attribute code,
/// for the configured write-back fields only.
pub fn write_back_projection(
    write_back: &WriteBack,
    state: &BTreeMap<&'static str, String>,
) -> Projection {
    write_back
        .scalars()
        .into_iter()
        .filter_map(|(key, attribute)| Some((attribute.to_owned(), state.get(key)?.clone())))
        .collect()
}

/// The write-back projection of a source record's current values.
pub fn local_write_back(write_back: &WriteBack, values: &Map<String, Value>) -> Projection {
    write_back
        .scalars()
        .into_iter()
        .filter_map(|(_, attribute)| Some((attribute.to_owned(), text(values.get(attribute))?)))
        .collect()
}

/// The writes that bring `local` to `remote`: changed values, and clears for
/// values the invoice no longer has. Unchanged fields are left alone.
pub fn write_back_writes(
    write_back: &WriteBack,
    local: &Projection,
    remote: &Projection,
) -> Vec<InboundWrite> {
    write_back
        .scalars()
        .into_iter()
        .filter(|(_, attribute)| local.get(*attribute) != remote.get(*attribute))
        .map(|(_, attribute)| InboundWrite {
            attribute: attribute.to_owned(),
            value: remote.get(attribute).cloned(),
        })
        .collect()
}

/// Clears every scalar write-back field the record holds, for a deleted
/// invoice.
pub fn clear_writes(write_back: &WriteBack, local: &Projection) -> Vec<InboundWrite> {
    write_back_writes(write_back, local, &Projection::new())
}

/// The hash stored as an invoice mapping's content hash.
pub fn write_back_hash(remote: &Projection) -> String {
    projection_hash(remote)
}

/// A file name for an invoice PDF: its number with path separators replaced.
pub fn pdf_filename(number: &str) -> String {
    let safe: String = number
        .chars()
        .map(|character| match character {
            '/' | '\\' => '-',
            character if character.is_control() => '_',
            character => character,
        })
        .collect();
    format!("{}.pdf", safe.trim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct Fixed(HashMap<String, BlueprintShape>);

    impl Catalog for Fixed {
        fn blueprint(&mut self, code: &str) -> Result<Option<BlueprintShape>, String> {
            Ok(self.0.get(code).cloned())
        }
    }

    fn attrs(list: &[(&str, &str, &[&str])]) -> BlueprintShape {
        list.iter()
            .map(|(code, value_type, targets)| {
                (
                    (*code).to_owned(),
                    AttributeShape {
                        value_type: (*value_type).to_owned(),
                        targets: targets.iter().map(|t| (*t).to_owned()).collect(),
                    },
                )
            })
            .collect()
    }

    fn catalog() -> Fixed {
        Fixed(HashMap::from([
            (
                "sales_order".to_owned(),
                attrs(&[
                    ("number", "string", &[]),
                    ("customer", "relationship", &["client"]),
                    ("document_date", "date", &[]),
                    ("note", "string", &[]),
                    ("invoice_number", "string", &[]),
                    ("invoice_status", "string", &[]),
                    ("invoice_url", "string", &[]),
                    ("ksef_number", "string", &[]),
                    ("invoice_pdf", "file", &[]),
                    ("total", "number", &[]),
                ]),
            ),
            (
                "sales_order_line".to_owned(),
                attrs(&[
                    ("sales_order", "relationship", &["sales_order"]),
                    ("item", "relationship", &["product", "service"]),
                    ("quantity", "number", &[]),
                    ("unit_price_net", "number", &[]),
                ]),
            ),
            (
                "product".to_owned(),
                attrs(&[
                    ("name", "string", &[]),
                    ("code", "string", &[]),
                    ("base_unit", "relationship", &["unit"]),
                ]),
            ),
            ("service".to_owned(), attrs(&[("name", "string", &[])])),
            ("unit".to_owned(), attrs(&[("code", "string", &[])])),
            ("client".to_owned(), attrs(&[("name", "string", &[])])),
        ]))
    }

    fn sync() -> InvoiceSync {
        serde_json::from_value(json!({
            "source": {"blueprint": "sales_order"},
            "buyer": "customer",
            "header": {"sell_date": "document_date", "oid": "number", "description": "note",
                "payment_to_days": 14, "payment_type": "transfer", "currency": "PLN"},
            "positions": {
                "lines": {"blueprint": "sales_order_line", "parent": "sales_order"},
                "name": "item.name", "code": "item.code", "quantity": "quantity",
                "unit": "item.base_unit.code", "price_net": "unit_price_net",
                "tax": {"default": "23"}
            },
            "write_back": {"number": "invoice_number", "status": "invoice_status",
                "url": "invoice_url", "ksef_number": "ksef_number", "file": "invoice_pdf"}
        }))
        .unwrap()
    }

    #[test]
    fn a_complete_configuration_validates_against_the_catalog() {
        let sync = sync();
        assert_eq!(sync.source.context, "default");
        assert_eq!(sync.header.kind, "vat");
        validate_against(&sync, "client", &mut catalog()).unwrap();
    }

    #[test]
    fn a_single_line_document_reads_positions_from_the_source() {
        let mut sync = sync();
        sync.positions = serde_json::from_value(json!({
            "name": "note", "quantity": "total", "price_net": "total", "tax": {"default": "zw"}
        }))
        .unwrap();
        validate_against(&sync, "client", &mut catalog()).unwrap();
    }

    #[test]
    fn configuration_errors_name_the_code_and_the_expected_type() {
        let check = |change: &dyn Fn(&mut InvoiceSync)| {
            let mut sync = sync();
            change(&mut sync);
            validate_against(&sync, "client", &mut catalog()).unwrap_err()
        };
        assert_eq!(
            check(&|sync| sync.source.blueprint = "nope".into()),
            "no published blueprint has the code nope"
        );
        assert_eq!(
            check(&|sync| sync.buyer = "number".into()),
            "sales_order.number is a string attribute; expected relationship"
        );
        assert_eq!(
            check(&|sync| sync.write_back.status = Some("total".into())),
            "sales_order.total is a number attribute; expected string"
        );
        assert_eq!(
            check(&|sync| sync.write_back.file = Some("invoice_url".into())),
            "sales_order.invoice_url is a string attribute; expected file"
        );
        assert_eq!(
            check(&|sync| sync.positions.unit = Some("item.base_unit.symbol".into())),
            "invoices.positions.quantity_unit: item.base_unit.symbol: no target blueprint has symbol"
        );
        assert_eq!(
            check(&|sync| sync.positions.name = "quantity.name".into()),
            "invoices.positions.name: sales_order_line.quantity in quantity.name is not a relationship"
        );
        assert_eq!(
            check(&|sync| sync.positions.name = "a.b.c.d.e".into()),
            "invoices.positions: path a.b.c.d.e has more than 3 relationship hops"
        );
        assert_eq!(
            check(&|sync| sync.positions.tax = Tax {
                attribute: None,
                default: Some("22".into())
            }),
            "invoices.positions.tax.default must be one of 23, 8, 5, 0, zw, np"
        );
        assert_eq!(
            check(&|sync| sync.source.blueprint = "client".into()),
            "invoices.source.blueprint must not be the clients blueprint"
        );
        assert_eq!(
            check(&|sync| {
                sync.header.oid = None;
                sync.link_existing_by_oid = true;
            }),
            "invoices.link_existing_by_oid needs invoices.header.oid"
        );
    }

    #[test]
    fn a_multi_target_relationship_needs_the_leaf_on_one_target() {
        // `item` targets products and services; only products have a code.
        let sync = sync();
        assert!(validate_against(&sync, "client", &mut catalog()).is_ok());
    }

    fn values(name: Option<&str>, quantity: Option<&str>, price: Option<&str>) -> PositionValues {
        PositionValues {
            name: name.map(str::to_owned),
            quantity: quantity.map(str::to_owned),
            price_net: price.map(str::to_owned),
            ..PositionValues::default()
        }
    }

    #[test]
    fn positions_use_the_default_tax_and_decimal_points() {
        let built = position(
            &sync().positions,
            "line 1",
            &PositionValues {
                code: Some("SKU-1".into()),
                unit: Some("szt".into()),
                ..values(Some("Widget"), Some("2,5"), Some("10.00"))
            },
        )
        .unwrap();
        assert_eq!(
            Value::Object(built),
            json!({"name": "Widget", "code": "SKU-1", "quantity": "2.5",
                "quantity_unit": "szt", "price_net": "10.00", "tax": "23",
                "total_price_gross": "30.75"})
        );
        let mut positions = sync().positions;
        positions.tax = Tax {
            attribute: Some("vat".into()),
            default: Some("23".into()),
        };
        let taxed = position(
            &positions,
            "line 1",
            &PositionValues {
                tax: Some("8%".into()),
                ..values(Some("Widget"), Some("1"), Some("1"))
            },
        )
        .unwrap();
        assert_eq!(taxed["tax"], "8");
        let exempt = position(
            &positions,
            "line 1",
            &PositionValues {
                tax: Some("ZW".into()),
                ..values(Some("Widget"), Some("1"), Some("1"))
            },
        )
        .unwrap();
        assert_eq!(exempt["tax"], "zw");
        assert_eq!(exempt["total_price_gross"], "1.00");
    }

    #[test]
    fn the_gross_total_rounds_net_and_vat_to_the_grosz() {
        let gross = |price: &str, quantity: &str, tax: &str| {
            position(
                &sync().positions,
                "l",
                &PositionValues {
                    tax: Some(tax.into()),
                    ..values(Some("Widget"), Some(quantity), Some(price))
                },
            )
            .map(|body| body["total_price_gross"].clone())
        };
        // 10 × 19.11 = 191.10 net, 43.953 → 43.95 VAT.
        assert_eq!(gross("19.11", "10", "23").unwrap(), "235.05");
        // 3 × 0.333 = 0.999 → 1.00 net before the VAT, not after.
        assert_eq!(gross("0.333", "3", "23").unwrap(), "1.23");
        // 0.005 VAT rounds half up.
        assert_eq!(gross("0.10", "1", "5").unwrap(), "0.11");
        // 12.5 × 0.125 = 1.5625 → 1.56 net, 0.1248 → 0.12 VAT.
        assert_eq!(gross("12,5", "0,125", "8").unwrap(), "1.68");
        assert_eq!(gross("100", "1", "np").unwrap(), "100.00");
        assert_eq!(gross("0", "4", "23").unwrap(), "0.00");
        assert!(gross("1e3", "1", "23").is_err());
        assert!(gross("1", "1234567890123456789", "23").is_err());
    }

    #[test]
    fn missing_values_are_named_with_their_record() {
        assert_eq!(
            position(&sync().positions, "line 2", &values(None, Some("0"), None)).unwrap_err(),
            vec![
                "line 2: name is empty".to_owned(),
                "line 2: quantity 0 is not a positive number".to_owned(),
                "line 2: price_net is empty".to_owned(),
            ]
        );
    }

    #[test]
    fn the_invoice_body_carries_no_product_ids() {
        let sync = sync();
        let header = HeaderValues::from([
            ("oid", "ZS/1".to_owned()),
            ("sell_date", "2026-11-26".to_owned()),
        ]);
        let line = position(
            &sync.positions,
            "l",
            &values(Some("Widget"), Some("1"), Some("5")),
        )
        .unwrap();
        let body = sync.invoice_body("42", &sync.oid(&header, "rec"), &header, vec![line]);
        assert_eq!(
            body,
            json!({"invoice": {"kind": "vat", "client_id": 42, "oid": "ZS/1", "oid_unique": "yes",
                "sell_date": "2026-11-26", "payment_to_kind": "14", "payment_type": "transfer",
                "currency": "PLN", "exclude_from_stock_level": true,
                "positions": [{"name": "Widget", "quantity": "1", "price_net": "5", "tax": "23",
                    "total_price_gross": "6.15"}]}})
        );
        assert_eq!(sync.oid(&HeaderValues::new(), "rec-1"), "rec-1");
    }

    #[test]
    fn write_back_writes_only_what_changed_and_hashes_the_state() {
        let sync = sync();
        let invoice = json!({"id": 7, "number": "FV 1/11/2026", "status": "paid", "token": "abc",
            "gov_id": "5250001009-20261126-ABC", "gov_status": "ok"});
        let state = invoice_state("https://acme.fakturownia.pl", invoice.as_object().unwrap());
        assert_eq!(state["url"], "https://acme.fakturownia.pl/invoice/abc");
        let remote = write_back_projection(&sync.write_back, &state);
        assert!(!remote.contains_key("ksef_status"), "not configured");
        let local = local_write_back(
            &sync.write_back,
            json!({"invoice_number": "FV 1/11/2026", "invoice_status": "issued", "other": 1})
                .as_object()
                .unwrap(),
        );
        let writes = write_back_writes(&sync.write_back, &local, &remote);
        assert_eq!(
            writes
                .iter()
                .map(|write| write.attribute.as_str())
                .collect::<Vec<_>>(),
            vec!["invoice_status", "invoice_url", "ksef_number"]
        );
        assert!(write_back_writes(&sync.write_back, &remote, &remote).is_empty());
        assert_eq!(write_back_hash(&remote), write_back_hash(&remote.clone()));
        assert_ne!(write_back_hash(&remote), write_back_hash(&local));
        let cleared = clear_writes(&sync.write_back, &local);
        assert!(cleared.iter().all(|write| write.value.is_none()));
        assert_eq!(cleared.len(), 2);
    }

    #[test]
    fn invoice_ids_are_prefixed_and_pdf_names_are_safe() {
        assert_eq!(invoice_external_id("7"), "invoice:7");
        assert_eq!(invoice_id("invoice:7"), Some("7"));
        assert_eq!(invoice_id("7"), None);
        assert_eq!(pdf_filename("FV 1/11/2026"), "FV 1-11-2026.pdf");
    }

    #[test]
    fn blueprint_reads_become_shapes() {
        let shape = blueprint_shape(&json!({"blueprint": {"id": "b"}, "attributes": [
            {"code": "item", "value_type": "relationship", "target_blueprint_code": "product",
                "target_blueprint_codes": ["product", "service"]},
            {"code": "name", "value_type": "string", "target_blueprint_codes": []}
        ]}))
        .unwrap();
        assert_eq!(shape["item"].targets, vec!["product", "service"]);
        assert_eq!(shape["name"].value_type, "string");
    }
}
