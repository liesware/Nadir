//! HTTP request building from resolved operations and case plans.
//!
//! # Design notes
//!
//! **Chosen:** `build_step_request()` takes a `ResolvedOperation`, bindings,
//! and a concrete set of replacements, and produces a `ResolvedStepInput`.
//! This is the bridge between the experiment model and the transport layer.

use serde_json::Value;
use url::Url;

use super::contract::{request_body_with_example, ResolvedOperation};
use crate::formats::{json, PayloadFormat};
use crate::experiment::types::{Binding, BindingValue, CompositePart, JsonExample};

// ── Public types ───────────────────────────────────────────────────

/// A fully resolved HTTP request ready for the transport layer.
#[derive(Debug, Clone)]
pub struct ResolvedStepInput {
    /// Full URL with path parameters substituted.
    pub url: Url,
    /// HTTP method.
    pub method: String,
    /// Request headers (including auth, content-type).
    pub headers: Vec<(String, String)>,
    /// Serialized JSON body, if any.
    pub body: Option<Vec<u8>>,
    /// Content-Type header value.
    pub content_type: Option<String>,
}

pub(crate) fn request_diagnostic(
    operation: &ResolvedOperation,
    request: Option<&ResolvedStepInput>,
) -> crate::execution::diagnostic::RequestDiagnostic {
    use crate::execution::diagnostic::{BodyShape, RequestDiagnostic};

    let content_type = request.and_then(|request| {
        let media_type = request.headers.iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .map(|(_, value)| value.as_str())
            .or(request.content_type.as_deref())?;
        media_type.split(';').next()
            .filter(|value| value.trim().eq_ignore_ascii_case("application/json"))
            .map(|_| "application/json")
    });
    let body = request.and_then(|request| request.body.as_deref());
    let body_shape = body
        .filter(|bytes| content_type.is_some() && bytes.len() <= 16 * 1024)
        .and_then(|bytes| json::decode(bytes).ok())
        .map(|value| match value {
            Value::Object(_) => BodyShape::Object,
            Value::Array(_) => BodyShape::Array,
            Value::String(_) => BodyShape::String,
            Value::Number(_) => BodyShape::Number,
            Value::Bool(_) => BodyShape::Boolean,
            Value::Null => BodyShape::Null,
        });
    RequestDiagnostic {
        method: operation.method.as_str(),
        path_template: operation.path_template.split(['?', '#']).next().unwrap_or_default().into(),
        content_type,
        body_size_bytes: request.map(|_| body.map_or(0, <[u8]>::len)),
        body_shape,
    }
}

/// Errors during input building.
#[derive(Debug, thiserror::Error)]
pub enum InputBuildError {
    #[error("unsupported content type: {0}")]
    UnsupportedContentType(String),
    #[error("required field '{field}' has no example, default, or binding")]
    UnresolvedRequiredField { field: String },
    #[error("binding targets unknown field: {0}")]
    UnknownBindingTarget(String),
    #[error("URL construction failed: {0}")]
    UrlError(String),
    #[error("JSON serialization failed: {0}")]
    JsonError(#[from] serde_json::Error),
    #[error("JSON key rename failed: {0}")]
    JsonRename(&'static str),
}

// Re-export from experiment types
pub use crate::experiment::types::ReplacementValue;

// ── Builder ────────────────────────────────────────────────────────

/// Build a fully resolved HTTP request for a step.
///
/// Bindings are applied first (overriding example values), then
/// replacements (from mutation) are applied on top.
pub fn build_step_request(
    operation: &ResolvedOperation,
    bindings: &[Binding],
    replacements: &[(String, ReplacementValue)],
    base_url: &Url,
) -> Result<ResolvedStepInput, InputBuildError> {
    build_step_request_with_example(operation, bindings, replacements, base_url, None, PayloadFormat::Json, None)
}

pub(crate) fn build_step_request_with_example(
    operation: &ResolvedOperation,
    bindings: &[Binding],
    replacements: &[(String, ReplacementValue)],
    base_url: &Url,
    example: Option<&Value>,
    format: PayloadFormat,
    json_example: Option<&JsonExample>,
) -> Result<ResolvedStepInput, InputBuildError> {
    let url = build_url(operation, bindings, replacements, base_url)?;
    let mut headers = build_headers(operation, bindings, replacements, format);
    let (body_bytes, content_type) = match json_example {
        Some(JsonExample::Syntax { fixture }) => {
            // Named syntax fixtures must reach the JSON parser as JSON.
            headers.retain(|(name, _)| !name.eq_ignore_ascii_case("content-type"));
            headers.push(("Content-Type".into(), format.media_type().into()));
            (Some(fixture.bytes().to_vec()), Some(format.media_type().into()))
        }
        _ => {
            let baseline = request_body_with_example(operation, bindings, example);
            let mut body = apply_replacements(baseline, replacements);
            if let Some(JsonExample::Rename { key, destination }) = json_example {
                json::rename(&mut body, key, destination).map_err(InputBuildError::JsonRename)?;
            }
            if body.is_null() || body.as_object().is_some_and(|object| object.is_empty()) {
                (None, None)
            } else {
                let mut bytes = format.encode(&body)?;
                // `use: body`: break the serialized text; Content-Type stays.
                if let Some(JsonExample::Edits { edits }) = json_example {
                    bytes = crate::mutation::body::apply(&String::from_utf8_lossy(&bytes), edits).into_bytes();
                }
                (Some(bytes), Some(format.media_type().into()))
            }
        }
    };

    Ok(ResolvedStepInput {
        url,
        method: operation.method.as_str().to_string(),
        headers,
        body: body_bytes,
        content_type,
    })
}

// ── Replacements ───────────────────────────────────────────────────

fn apply_replacements(mut body: Value, replacements: &[(String, ReplacementValue)]) -> Value {
    for (selector, value) in replacements {
        if selector.starts_with("header.") || selector.starts_with("path.") {
            continue;
        }
        let path: Vec<String> = selector
            .strip_prefix("body.")
            .unwrap_or(selector)
            .split('.')
            .map(String::from)
            .collect();

        let new_val = match value {
            ReplacementValue::JsonString(s) => Value::String(s.clone()),
            ReplacementValue::HeaderAscii(s) => Value::String(s.clone()),
            ReplacementValue::JsonValue(v) => v.clone(),
            ReplacementValue::Absent => {
                json::remove(&mut body, &path);
                continue;
            }
        };
        json::set(&mut body, &path, new_val);
    }
    body
}

// ── URL construction ───────────────────────────────────────────────

/// Characters a mutated path value keeps as they are: RFC 3986 unreserved.
const PATH_SEGMENT: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

fn build_url(
    operation: &ResolvedOperation,
    bindings: &[Binding],
    replacements: &[(String, ReplacementValue)],
    base_url: &Url,
) -> Result<Url, InputBuildError> {
    let mut path = operation.path_template.clone();

    // A mutated path value replaces the bound one, encoded as one segment so
    // `../` or `?` stay inside it. `Absent` leaves the segment empty.
    for (selector, value) in replacements {
        let Some(name) = selector.strip_prefix("path.") else {
            continue;
        };
        let text = match value {
            ReplacementValue::JsonString(text) | ReplacementValue::HeaderAscii(text) => text.clone(),
            ReplacementValue::JsonValue(Value::String(text)) => text.clone(),
            ReplacementValue::JsonValue(other) => other.to_string(),
            ReplacementValue::Absent => String::new(),
        };
        let encoded = percent_encoding::utf8_percent_encode(&text, PATH_SEGMENT).to_string();
        path = path.replace(&format!("{{{name}}}"), &encoded);
    }

    // Substitute path parameters from bindings
    for binding in bindings {
        if let crate::experiment::selectors::InputSelector::PathParam(ref name) = binding.selector {
            let value_str = binding_value_to_string(&binding.value);
            let placeholder = format!("{{{}}}", name);
            path = path.replace(&placeholder, &value_str);
        }
    }

    base_url
        .join(&path)
        .map_err(|e| InputBuildError::UrlError(e.to_string()))
}

fn binding_value_to_string(value: &BindingValue) -> String {
    match value {
        BindingValue::Literal(v) => match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        },
        BindingValue::Reference(r) => r.to_string(),
        BindingValue::Composite(parts) => parts
            .iter()
            .map(|p| match p {
                CompositePart::Literal(s) => s.clone(),
                CompositePart::Reference(r) => r.to_string(),
            })
            .collect(),
    }
}

// ── Headers ────────────────────────────────────────────────────────

fn build_headers(
    operation: &ResolvedOperation,
    bindings: &[Binding],
    replacements: &[(String, ReplacementValue)],
    format: PayloadFormat,
) -> Vec<(String, String)> {
    let mut headers = Vec::new();

    // Apply header bindings
    for binding in bindings {
        if let crate::experiment::selectors::InputSelector::HeaderParam(ref name) = binding.selector
        {
            let value = binding_value_to_string(&binding.value);
            headers.push((name.clone(), value));
        }
    }

    // Apply header replacements: a mutated header replaces the bound one
    // (names compare case-insensitively), so it is never sent twice.
    // `Absent` leaves it out.
    for (selector, value) in replacements {
        if let Some(name) = selector.strip_prefix("header.") {
            headers.retain(|(existing, _)| !existing.eq_ignore_ascii_case(name));
            let val = match value {
                ReplacementValue::HeaderAscii(s) => s.clone(),
                ReplacementValue::JsonString(s) => s.clone(),
                ReplacementValue::JsonValue(Value::String(s)) => s.clone(),
                ReplacementValue::JsonValue(v) => v.to_string(),
                ReplacementValue::Absent => continue,
            };
            headers.push((name.to_string(), val));
        }
    }

    // Add Content-Type for JSON bodies
    if operation.request_body.is_some()
        && !headers
            .iter()
            .any(|(k, _)| k.to_lowercase() == "content-type")
    {
        headers.push(("Content-Type".into(), format.media_type().into()));
    }

    headers
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::http::contract;
    use std::path::PathBuf;

    fn examples_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples")
    }

    #[test]
    fn mutated_headers_replace_bound_ones_and_absent_removes_them() {
        let contract = contract::load_contract(&examples_dir().join("create-record/openapi.yaml")).unwrap();
        let op = contract.operation_by_id("getTemplate").unwrap();
        let base_url = Url::parse("http://127.0.0.1:8080").unwrap();
        let bindings = vec![
            Binding {
                selector: crate::experiment::selectors::InputSelector::PathParam("id".into()),
                value: BindingValue::Literal(Value::from("t1")),
            },
            Binding {
                selector: crate::experiment::selectors::InputSelector::HeaderParam("X-Key".into()),
                value: BindingValue::Literal(Value::from("good-key")),
            },
        ];
        let sent = |replacements: &[(String, ReplacementValue)]| {
            build_step_request(op, &bindings, replacements, &base_url).unwrap()
        };
        let keys = |input: &ResolvedStepInput| -> Vec<String> {
            input.headers.iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case("x-key"))
                .map(|(_, value)| value.clone())
                .collect()
        };
        assert_eq!(keys(&sent(&[])), vec!["good-key"]);
        let mutated = sent(&[("header.x-key".into(), ReplacementValue::HeaderAscii("bad é".into()))]);
        assert_eq!(keys(&mutated), vec!["bad é"]);
        let absent = sent(&[("header.X-Key".into(), ReplacementValue::Absent)]);
        assert!(keys(&absent).is_empty());
        assert_eq!(absent.url.path(), "/templates/t1");
    }

    #[test]
    fn mutated_path_values_are_encoded_as_one_segment() {
        let contract = contract::load_contract(&examples_dir().join("create-record/openapi.yaml")).unwrap();
        let op = contract.operation_by_id("getTemplate").unwrap();
        let base_url = Url::parse("http://127.0.0.1:8080").unwrap();
        let bindings = vec![Binding {
            selector: crate::experiment::selectors::InputSelector::PathParam("id".into()),
            value: BindingValue::Literal(Value::from("t1")),
        }];
        let path = |value: ReplacementValue| {
            build_step_request(op, &bindings, &[("path.id".into(), value)], &base_url).unwrap().url.path().to_owned()
        };
        assert_eq!(path(ReplacementValue::JsonString("../a b?x#é".into())), "/templates/..%2Fa%20b%3Fx%23%C3%A9");
        assert_eq!(path(ReplacementValue::Absent), "/templates/");
        assert_eq!(path(ReplacementValue::JsonString("ok-1._~".into())), "/templates/ok-1._~");

        // A path selector never touches the body.
        let create = contract.operation_by_id("createRecord").unwrap();
        let plain = build_step_request(create, &[], &[], &base_url).unwrap();
        let with_path = build_step_request(
            create, &[], &[("path.title".into(), ReplacementValue::JsonString("x".into()))], &base_url,
        ).unwrap();
        assert_eq!(plain.body, with_path.body);
    }

    #[test]
    fn test_builds_create_record_request() {
        let contract_path = examples_dir().join("create-record/openapi.yaml");
        let contract = contract::load_contract(&contract_path).unwrap();
        let op = contract.operation_by_id("createRecord").unwrap();
        let base_url = Url::parse("http://127.0.0.1:8080").unwrap();

        let bindings: Vec<Binding> = vec![];
        let input = build_step_request(op, &bindings, &[], &base_url).unwrap();

        assert_eq!(input.method, "POST");
        assert!(input.url.path() == "/records", "url: {}", input.url);
        assert!(input.body.is_some());
        assert_eq!(input.content_type.as_deref(), Some("application/json"));
    }

    #[test]
    fn test_replacements_set_typed_values_and_remove_fields() {
        let contract_path = examples_dir().join("create-record/openapi.yaml");
        let contract = contract::load_contract(&contract_path).unwrap();
        let op = contract.operation_by_id("createRecord").unwrap();
        let base_url = Url::parse("http://127.0.0.1:8080").unwrap();
        let body = |replacements: &[(String, ReplacementValue)]| -> Value {
            let input = build_step_request(op, &[], replacements, &base_url).unwrap();
            serde_json::from_slice(&input.body.unwrap()).unwrap()
        };

        let removed = body(&[("body.title".into(), ReplacementValue::Absent)]);
        assert!(removed.get("title").is_none(), "{removed}");
        assert!(removed.get("reference").is_some(), "{removed}");

        let nested = body(&[("body.metadata.source".into(), ReplacementValue::Absent)]);
        assert_eq!(nested["metadata"], serde_json::json!({}), "{nested}");

        let typed = body(&[(
            "body.title".into(),
            ReplacementValue::JsonValue(serde_json::json!([1, 2])),
        )]);
        assert_eq!(typed["title"], serde_json::json!([1, 2]));

        // Removing a field that is not there leaves the body unchanged.
        let untouched = body(&[("body.nope.deeper".into(), ReplacementValue::Absent)]);
        assert_eq!(untouched, body(&[]));
    }

    #[test]
    fn header_replacements_leave_json_and_bodyless_requests_unchanged() {
        let contract = contract::load_contract(&examples_dir().join("create-record/openapi.yaml"))
            .unwrap();
        let base_url = Url::parse("http://127.0.0.1:8080").unwrap();
        let replacements = [("header.X-Test".into(), ReplacementValue::HeaderAscii("changed".into()))];
        for (operation_id, bindings) in [
            ("createRecord", vec![]),
            ("getRecordByReference", vec![Binding {
                selector: crate::experiment::selectors::InputSelector::PathParam("reference".into()),
                value: BindingValue::Literal(Value::String("test-reference".into())),
            }]),
        ] {
            let operation = contract.operation_by_id(operation_id).unwrap();
            let baseline = build_step_request(operation, &bindings, &[], &base_url).unwrap();
            let changed = build_step_request(operation, &bindings, &replacements, &base_url).unwrap();
            assert_eq!(changed.body, baseline.body);
            assert_eq!(changed.content_type, baseline.content_type);
            assert!(changed.headers.contains(&("X-Test".into(), "changed".into())));
        }
    }

    #[test]
    fn test_substitutes_path_params() {
        let contract_path = examples_dir().join("create-record/openapi.yaml");
        let contract = contract::load_contract(&contract_path).unwrap();
        let op = contract.operation_by_id("getRecordByReference").unwrap();
        let base_url = Url::parse("http://127.0.0.1:8080").unwrap();

        use crate::experiment::selectors::InputSelector;
        let bindings = vec![Binding {
            selector: InputSelector::PathParam("reference".into()),
            value: BindingValue::Literal(Value::String("my-ref-001".into())),
        }];

        let input = build_step_request(op, &bindings, &[], &base_url).unwrap();

        assert_eq!(input.method, "GET");
        assert!(
            input.url.path() == "/records/my-ref-001",
            "url: {}",
            input.url
        );
        assert!(input.body.is_none());
    }
}
