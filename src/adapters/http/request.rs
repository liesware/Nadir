//! HTTP request building from resolved operations and case plans.
//!
//! # Design notes
//!
//! **Chosen:** `build_step_request()` takes a `ResolvedOperation`, bindings,
//! and a concrete set of replacements, and produces a `ResolvedStepInput`.
//! This is the bridge between the experiment model and the transport layer.

use serde_json::Value;
use url::Url;

use crate::adapters::http::contract::ResolvedOperation;
use crate::experiment::types::{Binding, BindingValue, CompositePart};

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
    // 1. Build the baseline body from the operation example + bindings
    let body_value = build_body(operation, bindings)?;

    // 2. Apply replacements to the body
    let body_value = apply_replacements(body_value, replacements);

    // 3. Build the URL with path parameters substituted
    let url = build_url(operation, bindings, base_url)?;

    // 4. Collect headers (auth, content-type)
    let headers = build_headers(operation, bindings, replacements);

    // 5. Serialize body if present
    let (body_bytes, content_type) =
        if body_value.is_null() || body_value.as_object().is_some_and(|o| o.is_empty()) {
            (None, None)
        } else {
            let json = serde_json::to_vec(&body_value)?;
            (Some(json), Some("application/json".to_string()))
        };

    Ok(ResolvedStepInput {
        url,
        method: operation.method.as_str().to_string(),
        headers,
        body: body_bytes,
        content_type,
    })
}

// ── Body construction ──────────────────────────────────────────────

fn build_body(
    operation: &ResolvedOperation,
    bindings: &[Binding],
) -> Result<Value, InputBuildError> {
    let body_schema = match &operation.request_body {
        Some(rb) => rb,
        None => return Ok(Value::Null),
    };

    // Start from the example
    let mut body = body_schema
        .example
        .clone()
        .unwrap_or(Value::Object(serde_json::Map::new()));

    // Apply bindings
    for binding in bindings {
        if let crate::experiment::selectors::InputSelector::BodyField(path) = &binding.selector {
            let value = resolve_binding_value_to_json(binding);
            set_json_path(&mut body, path, value);
        }
    }

    Ok(body)
}

fn resolve_binding_value_to_json(binding: &Binding) -> Value {
    match &binding.value {
        BindingValue::Literal(v) => v.clone(),
        BindingValue::Reference(r) => Value::String(r.to_string()),
        BindingValue::Composite(parts) => {
            let s: String = parts
                .iter()
                .map(|p| match p {
                    CompositePart::Literal(s) => s.clone(),
                    CompositePart::Reference(r) => r.to_string(),
                })
                .collect();
            Value::String(s)
        }
    }
}

fn set_json_path(root: &mut Value, path: &[String], value: Value) {
    if path.is_empty() {
        *root = value;
        return;
    }
    let obj = match root {
        Value::Object(ref mut map) => map,
        _ => {
            *root = Value::Object(serde_json::Map::new());
            root.as_object_mut().unwrap()
        }
    };
    if path.len() == 1 {
        obj.insert(path[0].clone(), value);
        return;
    }
    let first = &path[0];
    let child = obj
        .entry(first.clone())
        .or_insert(Value::Object(serde_json::Map::new()));
    set_json_path(child, &path[1..], value);
}

// ── Replacements ───────────────────────────────────────────────────

fn apply_replacements(mut body: Value, replacements: &[(String, ReplacementValue)]) -> Value {
    for (selector, value) in replacements {
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
                remove_json_path(&mut body, &path);
                continue;
            }
        };
        set_json_path(&mut body, &path, new_val);
    }
    body
}

fn remove_json_path(root: &mut Value, path: &[String]) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let parent = parents
        .iter()
        .try_fold(root, |current, key| current.get_mut(key.as_str()));
    if let Some(Value::Object(map)) = parent {
        map.remove(last);
    }
}

// ── URL construction ───────────────────────────────────────────────

fn build_url(
    operation: &ResolvedOperation,
    bindings: &[Binding],
    base_url: &Url,
) -> Result<Url, InputBuildError> {
    let mut path = operation.path_template.clone();

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

    // Apply header replacements
    for (selector, value) in replacements {
        if let Some(name) = selector.strip_prefix("header.") {
            let val = match value {
                ReplacementValue::HeaderAscii(s) => s.clone(),
                ReplacementValue::JsonString(s) => s.clone(),
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
        headers.push(("Content-Type".into(), "application/json".into()));
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
    fn test_builds_create_record_request() {
        let contract_path = examples_dir().join("create-record/openapi.yaml");
        let contract = contract::load_contract(&contract_path).unwrap();
        let op = contract.operations.get("createRecord").unwrap();
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
        let op = contract.operations.get("createRecord").unwrap();
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
    fn test_substitutes_path_params() {
        let contract_path = examples_dir().join("create-record/openapi.yaml");
        let contract = contract::load_contract(&contract_path).unwrap();
        let op = contract.operations.get("getRecordByReference").unwrap();
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
