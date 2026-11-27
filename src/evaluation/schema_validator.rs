//! Prepared response checks indexed by OpenAPI operation and HTTP status.

use std::collections::HashMap;

pub use crate::formats::json::{SchemaValidationError, SchemaValidator};

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
        if cache.contains_key(&step.operation_key) {
            continue;
        }
        let error = |status, reason| SchemaPreparationError {
            operation: step.operation_key.clone(),
            status,
            reason,
        };
        let contract = experiment
            .protocols
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
            .operation(&step.operation_key)
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
        cache.insert(step.operation_key.clone(), checks);
    }
    Ok(cache)
}
