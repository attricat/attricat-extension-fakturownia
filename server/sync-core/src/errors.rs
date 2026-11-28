//! How an unusable Fakturownia answer becomes a failure a person can read.
//!
//! The component turns a [`Failed`] into the host's typed `failure`: a
//! [`Failed::Rejected`] is recorded and shown with its message, a
//! [`Failed::Retry`] is retried with backoff. Nothing here is a fault: a
//! Fakturownia error is never a bug of the extension and must not quarantine
//! it.

use serde_json::Value;

pub const STATUS_UNAUTHORIZED: u16 = 401;
pub const STATUS_FORBIDDEN: u16 = 403;
pub const STATUS_UNPROCESSABLE: u16 = 422;
pub const STATUS_TOO_MANY_REQUESTS: u16 = 429;
const STATUS_CLIENT_ERRORS: std::ops::Range<u16> = 400..500;
const STATUS_SERVER_ERRORS: u16 = 500;

/// The API token was refused (401 or 403).
pub const CODE_UNAUTHORIZED: &str = "fakturownia_unauthorized";
/// A validation error (422): a field Fakturownia refuses.
pub const CODE_INVALID: &str = "fakturownia_invalid";
/// Any other refused request (4xx).
pub const CODE_REJECTED: &str = "fakturownia_rejected";
/// An answer that is neither a success nor an error (1xx, 3xx).
pub const CODE_UNEXPECTED: &str = "fakturownia_unexpected";

/// The wait before retrying when Fakturownia names none.
pub const DEFAULT_RETRY_AFTER_MS: u64 = 5_000;
/// The longest `Retry-After` honoured; a longer one is capped.
pub const MAX_RETRY_AFTER_MS: u64 = 60 * 60 * 1_000;
/// The longest failure message, in bytes; longer text is cut with an ellipsis.
pub const MAX_MESSAGE_BYTES: usize = 500;
const ELLIPSIS: &str = "…";
/// Prefix of every message that carries Fakturownia's own text.
const PREFIX: &str = "Fakturownia: ";

/// What an unusable Fakturownia answer means for the work that sent it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failed {
    /// Retrying the same request cannot succeed.
    Rejected { code: &'static str, message: String },
    /// A temporary failure; try again after `after_ms`.
    Retry { message: String, after_ms: u64 },
}

/// Maps a Fakturownia answer other than the success the caller expected.
/// `retry_after` is the `Retry-After` header, when Fakturownia sent one.
pub fn failed_response(status: u16, body: &str, retry_after: Option<&str>) -> Failed {
    let text = error_text(body);
    let with_text = |fallback: String| {
        bounded(match &text {
            Some(text) => format!("{PREFIX}{text}"),
            None => fallback,
        })
    };
    match status {
        STATUS_UNAUTHORIZED | STATUS_FORBIDDEN => Failed::Rejected {
            code: CODE_UNAUTHORIZED,
            message: format!(
                "Fakturownia refused the API token (HTTP {status}). Check the Fakturownia API \
                 token secret in the extension's Secrets tab and the account in its configuration."
            ),
        },
        STATUS_TOO_MANY_REQUESTS => Failed::Retry {
            message: format!("Fakturownia throttled the request (HTTP {status}); retrying later."),
            after_ms: retry_after_ms(retry_after),
        },
        status if status >= STATUS_SERVER_ERRORS => Failed::Retry {
            message: with_text(format!(
                "Fakturownia failed to answer (HTTP {status}); retrying later."
            )),
            after_ms: retry_after_ms(retry_after),
        },
        STATUS_UNPROCESSABLE => Failed::Rejected {
            code: CODE_INVALID,
            message: with_text(format!(
                "Fakturownia refused the request as invalid (HTTP {status})."
            )),
        },
        status if STATUS_CLIENT_ERRORS.contains(&status) => Failed::Rejected {
            code: CODE_REJECTED,
            message: with_text(format!("Fakturownia refused the request (HTTP {status}).")),
        },
        status => Failed::Rejected {
            code: CODE_UNEXPECTED,
            message: format!("Fakturownia answered HTTP {status}."),
        },
    }
}

/// The `after-ms` of a `Retry-After` header in seconds, capped; the default
/// wait when it is missing or not a number of seconds (an HTTP date).
pub fn retry_after_ms(header: Option<&str>) -> u64 {
    header
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map_or(DEFAULT_RETRY_AFTER_MS, |seconds| {
            seconds.saturating_mul(1_000).min(MAX_RETRY_AFTER_MS)
        })
}

/// The human text of a Fakturownia error body, or `None` when it has none.
///
/// Fakturownia answers `{"code":"error","message":{"field":["- text"]}}` to a
/// validation error and `{"code":"error","message":"text"}` otherwise; this
/// also reads a bare string, a bare field map, `error`/`errors` keys and
/// plain text. HTML pages (a proxy's error page) carry no usable text.
pub fn error_text(body: &str) -> Option<String> {
    let text = match serde_json::from_str::<Value>(body) {
        Ok(value) => json_text(&value),
        Err(_) => {
            let body = body.trim();
            (!body.is_empty() && !body.starts_with('<')).then(|| body.to_owned())
        }
    }?;
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty()).then_some(text)
}

fn json_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(message_line(text)),
        Value::Array(items) => {
            let lines: Vec<String> = items.iter().filter_map(json_text).collect();
            (!lines.is_empty()).then(|| lines.join(", "))
        }
        Value::Object(object) => ["message", "error", "errors", "description"]
            .iter()
            .find_map(|key| object.get(*key).and_then(json_text))
            .or_else(|| field_errors(object)),
        _ => None,
    }
    .filter(|text| !text.is_empty())
}

/// A field map such as `{"nip": ["- is invalid"], "name": "is empty"}`.
fn field_errors(object: &serde_json::Map<String, Value>) -> Option<String> {
    let fields: Vec<String> = object
        .iter()
        .filter(|(field, _)| field.as_str() != "code")
        .filter_map(|(field, messages)| {
            let text = match messages {
                Value::Object(nested) => field_errors(nested),
                other => json_text(other),
            }?;
            Some(format!("{field}: {text}"))
        })
        .collect();
    (!fields.is_empty()).then(|| fields.join("; "))
}

/// One message as Fakturownia lists it, without its `- ` bullet.
fn message_line(text: &str) -> String {
    text.trim().trim_start_matches("- ").trim().to_owned()
}

/// `message` cut to [`MAX_MESSAGE_BYTES`] at a character boundary.
pub fn bounded(message: String) -> String {
    if message.len() <= MAX_MESSAGE_BYTES {
        return message;
    }
    let mut end = MAX_MESSAGE_BYTES - ELLIPSIS.len();
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{ELLIPSIS}", &message[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_field_errors() {
        assert_eq!(
            error_text(r#"{"code":"error","message":{"name":["- nie może być puste"]}}"#)
                .as_deref(),
            Some("name: nie może być puste")
        );
        assert_eq!(
            error_text(
                r#"{"message":{"nip":["- jest nieprawidłowy","- za krótki"],"name":"pusty"}}"#
            )
            .as_deref(),
            Some("name: pusty; nip: jest nieprawidłowy, za krótki")
        );
        // A bare field map, as the webhook registration answers.
        assert_eq!(
            error_text(r#"{"kind":["- nie znajduje się na liście"]}"#).as_deref(),
            Some("kind: nie znajduje się na liście")
        );
        assert_eq!(
            error_text(
                r#"{"message":{"positions":{"total_price_gross":["- pole nie może być puste"]}}}"#
            )
            .as_deref(),
            Some("positions: total_price_gross: pole nie może być puste")
        );
    }

    #[test]
    fn reads_messages_and_strings() {
        assert_eq!(
            error_text(r#"{"code":"error","message":"Brak dostępu"}"#).as_deref(),
            Some("Brak dostępu")
        );
        assert_eq!(
            error_text(r#""bad request""#).as_deref(),
            Some("bad request")
        );
        assert_eq!(
            error_text(r#"{"error":"not found"}"#).as_deref(),
            Some("not found")
        );
        assert_eq!(
            error_text(r#"{"errors":["a","b"]}"#).as_deref(),
            Some("a, b")
        );
        assert_eq!(
            error_text("  Service\n temporarily unavailable ").as_deref(),
            Some("Service temporarily unavailable")
        );
    }

    #[test]
    fn finds_no_text_in_empty_or_html_bodies() {
        assert_eq!(error_text(""), None);
        assert_eq!(error_text("<html><body>502</body></html>"), None);
        assert_eq!(error_text(r#"{"code":"error"}"#), None);
        assert_eq!(error_text(r#"{"message":""}"#), None);
        assert_eq!(error_text("null"), None);
        assert_eq!(error_text("{}"), None);
    }

    #[test]
    fn a_validation_error_is_rejected_with_its_text() {
        let body = r#"{"code":"error","message":{"buyer_tax_no":["- jest nieprawidłowy"]}}"#;
        assert_eq!(
            failed_response(422, body, None),
            Failed::Rejected {
                code: CODE_INVALID,
                message: "Fakturownia: buyer_tax_no: jest nieprawidłowy".into()
            }
        );
        assert_eq!(
            failed_response(422, "", None),
            Failed::Rejected {
                code: CODE_INVALID,
                message: "Fakturownia refused the request as invalid (HTTP 422).".into()
            }
        );
    }

    #[test]
    fn other_client_errors_are_rejected() {
        assert_eq!(
            failed_response(400, r#"{"code":"error","message":"zły format"}"#, None),
            Failed::Rejected {
                code: CODE_REJECTED,
                message: "Fakturownia: zły format".into()
            }
        );
        assert_eq!(
            failed_response(404, "", None),
            Failed::Rejected {
                code: CODE_REJECTED,
                message: "Fakturownia refused the request (HTTP 404).".into()
            }
        );
        assert!(matches!(
            failed_response(302, "", None),
            Failed::Rejected {
                code: CODE_UNEXPECTED,
                ..
            }
        ));
    }

    #[test]
    fn a_refused_token_is_rejected_and_names_the_secret() {
        for status in [401, 403] {
            let Failed::Rejected { code, message } = failed_response(
                status,
                r#"{"code":"error","message":"You must be logged in"}"#,
                None,
            ) else {
                panic!("a refused token is a rejection");
            };
            assert_eq!(code, CODE_UNAUTHORIZED);
            assert!(message.contains("API token secret"), "{message}");
            assert!(message.contains(&status.to_string()), "{message}");
        }
    }

    #[test]
    fn throttling_and_server_errors_are_retried() {
        assert_eq!(
            failed_response(429, "", Some("30")),
            Failed::Retry {
                message: "Fakturownia throttled the request (HTTP 429); retrying later.".into(),
                after_ms: 30_000
            }
        );
        assert_eq!(
            failed_response(503, "<html>down</html>", None),
            Failed::Retry {
                message: "Fakturownia failed to answer (HTTP 503); retrying later.".into(),
                after_ms: DEFAULT_RETRY_AFTER_MS
            }
        );
        assert_eq!(
            failed_response(500, r#"{"message":"Internal error"}"#, Some("2")),
            Failed::Retry {
                message: "Fakturownia: Internal error".into(),
                after_ms: 2_000
            }
        );
    }

    #[test]
    fn retry_after_is_seconds_capped() {
        assert_eq!(retry_after_ms(Some(" 7 ")), 7_000);
        assert_eq!(retry_after_ms(Some("0")), 0);
        assert_eq!(retry_after_ms(Some("86400")), MAX_RETRY_AFTER_MS);
        assert_eq!(
            retry_after_ms(Some(&u64::MAX.to_string())),
            MAX_RETRY_AFTER_MS
        );
        assert_eq!(
            retry_after_ms(Some("Wed, 21 Oct 2026 07:28:00 GMT")),
            DEFAULT_RETRY_AFTER_MS
        );
        assert_eq!(retry_after_ms(None), DEFAULT_RETRY_AFTER_MS);
    }

    #[test]
    fn long_messages_are_bounded_at_a_character() {
        let body = serde_json::json!({"message": "ż".repeat(400)}).to_string();
        let Failed::Rejected { message, .. } = failed_response(422, &body, None) else {
            panic!("a 422 is a rejection");
        };
        assert!(message.len() <= MAX_MESSAGE_BYTES, "{}", message.len());
        assert!(message.starts_with("Fakturownia: żż"));
        assert!(message.ends_with(ELLIPSIS));
        assert_eq!(bounded("short".into()), "short");
    }
}
