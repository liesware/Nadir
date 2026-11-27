//! JSON Schema validator wrapping `jsonschema` Draft 2020-12.
//!
//! # Design notes
//!
//! **Chosen:** `SchemaValidator` compiles schemas during static preparation and
//! validates instances against them. External `$ref` resolution (file and
//! network) is disabled at the crate-feature level. All schemas passed in
//! are already resolved by Nadir's contract loader.
//!
//! **DESIGN-04:** the validation library is hidden behind this module.
//! Callers see only Nadir types — `jsonschema` never leaks out.
//!
//! **Alternative considered:** extending the existing `CompiledJsonSchema` in
//! the contract module with validation methods. Rejected because validation
//! belongs in evaluation, not contract loading.

use std::collections::HashMap;
use std::sync::Arc;

use jsonschema::{Draft, ValidationOptions, Validator};
use serde_json::Value;

/// A compiled JSON Schema validator.
///
/// Compile once per OpenAPI response schema; validate many instances.
#[derive(Debug, Clone)]
pub struct SchemaValidator {
    compiled: Arc<Validator>,
}

/// One entry per declared exact status of an operation using the schema oracle.
pub type SchemaValidatorCache = HashMap<String, HashMap<u16, ResponseCheck>>;

#[derive(Debug)]
pub enum ResponseCheck {
    Bodyless,
    Json(SchemaValidator),
}

#[derive(Debug, thiserror::Error)]
#[error("operation '{operation}'{status}: {reason}", status = .status.map(|code| format!(" HTTP {code}")).unwrap_or_default())]
pub struct SchemaPreparationError {
    operation: String,
    status: Option<u16>,
    reason: String,
}

pub fn build_schema_cache(
    experiment: &crate::experiment::types::Experiment,
    contracts: &HashMap<String, crate::adapters::http::contract::ResolvedContract>,
) -> Result<SchemaValidatorCache, SchemaPreparationError> {
    use crate::experiment::types::OracleType;
    let mut cache = SchemaValidatorCache::new();
    for step in experiment.steps.iter().filter(|step| {
        step.oracles
            .iter()
            .any(|oracle| oracle.oracle_type == OracleType::OpenApiSchema)
    }) {
        if cache.contains_key(&step.operation_id) {
            continue;
        }
        let error = |status, reason| SchemaPreparationError {
            operation: step.operation_id.clone(),
            status,
            reason,
        };
        let contract = experiment
            .adapters
            .http
            .as_ref()
            .and_then(|adapter| contracts.get(&adapter.openapi_path))
            .ok_or_else(|| error(None, "OpenAPI contract is unavailable".into()))?;
        if !contract.openapi_version.starts_with("3.1.") {
            return Err(error(
                None,
                "openapi_schema requires an OpenAPI 3.1 contract".into(),
            ));
        }
        let operation = contract
            .operations
            .get(&step.operation_id)
            .ok_or_else(|| error(None, "OpenAPI operation is unavailable".into()))?;
        let mut checks = HashMap::new();
        for (&status, response) in operation
            .responses
            .iter()
            .filter(|(status, _)| **status != 0)
        {
            let check = match (response.content_type.as_deref(), &response.schema) {
                (None, None) => ResponseCheck::Bodyless,
                (Some("application/json"), Some(schema)) => {
                    ResponseCheck::Json(SchemaValidator::compile(&schema.0).map_err(|failure| {
                        error(
                            Some(status),
                            format!("invalid response schema at {}", failure.location()),
                        )
                    })?)
                }
                _ => return Err(error(
                    Some(status),
                    "expected a bodyless response or an application/json response with a schema"
                        .into(),
                )),
            };
            checks.insert(status, check);
        }
        if checks.is_empty() {
            return Err(error(
                None,
                "openapi_schema requires at least one exact response status".into(),
            ));
        }
        cache.insert(step.operation_id.clone(), checks);
    }
    Ok(cache)
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
    /// already inlined by the contract loader). Draft 2020-12 is used and
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
}
