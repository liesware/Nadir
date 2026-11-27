//! JSON encoding, field access, and schema validation shared by protocols.

use std::sync::Arc;

use jsonschema::{Draft, ValidationOptions, Validator};
use serde_json::Value;

use crate::experiment::selectors::JsonPathSegment;

pub(crate) fn decode(bytes: &[u8]) -> Result<Value, serde_json::Error> {
    serde_json::from_slice(bytes)
}

pub(crate) fn encode(value: &Value) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(value)
}

pub(crate) fn get<'a>(root: &'a Value, path: &[JsonPathSegment]) -> Option<&'a Value> {
    path.iter().try_fold(root, |value, segment| match segment {
        JsonPathSegment::Key(key) => value.get(key),
        JsonPathSegment::Index(index) => value.get(*index),
    })
}

pub(crate) fn get_object_path<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(root, |value, key| value.get(key))
}

pub(crate) fn rename(root: &mut Value, path: &str, destination: &str) -> Result<(), &'static str> {
    let mut parts: Vec<_> = path.split('.').collect();
    let key = parts.pop().ok_or("key is missing")?;
    let parent = parts.into_iter().try_fold(root, |value, part| value.get_mut(part));
    let object = parent.and_then(Value::as_object_mut).ok_or("key parent is not an object")?;
    if object.contains_key(destination) {
        return Err("destination key already exists");
    }
    let value = object.remove(key).ok_or("source key is missing")?;
    object.insert(destination.to_owned(), value);
    Ok(())
}

/// Set a value at a JSON path (e.g., `["metadata", "source"]`).
pub(crate) fn set(root: &mut Value, path: &[String], value: Value) {
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
    set(child, &path[1..], value);
}

pub(crate) fn remove(root: &mut Value, path: &[String]) {
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

/// A compiled JSON Schema validator.
///
/// Compile once per schema and validate many instances.
#[derive(Debug, Clone)]
pub struct SchemaValidator {
    compiled: Arc<Validator>,
}

/// Schema diagnostics carry paths from the schema, never values from an instance.
#[derive(Debug, Clone)]
pub enum SchemaValidationError {
    InvalidSchema(String),
    InstanceError(String),
}

impl SchemaValidationError {
    pub fn location(&self) -> &str {
        let path = match self {
            Self::InvalidSchema(path) | Self::InstanceError(path) => path,
        };
        if path.is_empty() {
            "/"
        } else {
            path
        }
    }
}

impl SchemaValidator {
    /// Compile a JSON Schema value into a reusable validator.
    ///
    /// The schema must be a fully resolved JSON Schema object (all `$ref`s
    /// already inlined by the caller). Draft 2020-12 is used and
    /// external resolution is disabled.
    pub fn compile(schema: &Value) -> Result<Self, SchemaValidationError> {
        let compiled = ValidationOptions::default()
            .with_draft(Draft::Draft202012)
            .should_validate_formats(false)
            .build(schema)
            .map_err(|error| {
                SchemaValidationError::InvalidSchema(error.instance_path.to_string())
            })?;
        Ok(Self {
            compiled: Arc::new(compiled),
        })
    }

    /// Validate a JSON instance against the compiled schema.
    ///
    /// Returns the schema location of the first violation without including the instance value.
    pub fn validate(&self, instance: &Value) -> Result<(), SchemaValidationError> {
        match self.compiled.iter_errors(instance).next() {
            Some(error) => Err(SchemaValidationError::InstanceError(
                error.schema_path.to_string(),
            )),
            None => Ok(()),
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_compile_valid_schema() {
        let schema = json!({
            "type": "object",
            "required": ["name"],
            "properties": {
                "name": { "type": "string", "minLength": 1 }
            }
        });
        let validator = SchemaValidator::compile(&schema);
        assert!(validator.is_ok());
    }

    #[test]
    fn test_compile_invalid_schema_rejected() {
        // A schema with an invalid keyword
        let schema = json!({
            "type": "nonexistent_type"
        });
        let result = SchemaValidator::compile(&schema);
        assert!(result.is_err());
        match result {
            Err(SchemaValidationError::InvalidSchema(_)) => {}
            _ => panic!("expected InvalidSchema"),
        }
    }

    #[test]
    fn test_validation_pass() {
        let schema = json!({
            "type": "object",
            "required": ["name"],
            "properties": {
                "name": { "type": "string", "minLength": 1 }
            }
        });
        let validator = SchemaValidator::compile(&schema).unwrap();
        let instance = json!({"name": "test"});
        assert!(validator.validate(&instance).is_ok());
    }

    #[test]
    fn test_validation_fail_wrong_type() {
        let schema = json!({
            "type": "object",
            "required": ["name"],
            "properties": {
                "name": { "type": "string" }
            }
        });
        let validator = SchemaValidator::compile(&schema).unwrap();
        let instance = json!({"name": 42});
        let result = validator.validate(&instance);
        assert!(result.is_err());
        match result {
            Err(SchemaValidationError::InstanceError(_)) => {}
            _ => panic!("expected InstanceError"),
        }
    }

    #[test]
    fn test_validation_fail_missing_required() {
        let schema = json!({
            "type": "object",
            "required": ["name"],
            "properties": {
                "name": { "type": "string" }
            }
        });
        let validator = SchemaValidator::compile(&schema).unwrap();
        let instance = json!({});
        let result = validator.validate(&instance);
        assert!(result.is_err());
    }
    #[test]
    fn get_object_path_reads_nested_fields() {
        let body = serde_json::json!({"metadata": {"source": "nadir"}, "title": "t"});
        assert_eq!(
            get_object_path(&body, "metadata.source"),
            Some(&Value::from("nadir"))
        );
        assert_eq!(get_object_path(&body, "title"), Some(&Value::from("t")));
        assert_eq!(get_object_path(&body, "metadata.missing"), None);
        assert_eq!(get_object_path(&body, "title.deeper"), None);
    }
}
