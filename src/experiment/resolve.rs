//! Convert `RawExperiment` into validated `Experiment`.
//!
//! Resolves every reference statically (except `${env.*}`), validates
//! step ordering, checks that every input selector exists in the OpenAPI
//! contract, and computes canonical descriptor ordering.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde_json::Value;

use crate::adapters::http::contract::{self, ResolvedContract, ResolvedOperation};
use crate::config::{RawExperiment, RawExperimentBlock, RawStep, RawStepCases};
use crate::experiment::selectors::{
    self, BindingParseResult, ObservationSelector, Reference,
};
use crate::experiment::types::{
    AdapterConfigs, AdapterKind, AuthConfig, Binding, BindingValue, CompositePart,
    CoverageConfig, CoverageTransport, ExpectCondition, ExplorationConfig,
    ExplorationStrategy, Experiment, FieldSchema, HttpAdapterConfig, JsonMutatorMode,
    MaxCases, MutatorDecl, OracleDecl, OracleKind, OracleType, OutputDecl,
    ReadyConfig, Step, TargetConfig, WhenCondition,
};

// ── Errors ─────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum ResolutionError {
    #[error("target.{field}: {detail}")]
    InvalidTargetReference { field: String, detail: String },
    #[error("unknown adapter name: '{0}'")]
    UnknownAdapter(String),
    #[error("operation '{operation_id}' not found in contract for adapter '{adapter}'")]
    UnknownOperation {
        adapter: String,
        operation_id: String,
    },
    #[error("step '{step_id}': required path parameter '{parameter}' has no binding")]
    MissingPathBinding { step_id: String, parameter: String },
    #[error("step '{step_id}': unknown selector '{selector}'")]
    UnknownSelector { step_id: String, selector: String },
    #[error("step '{step_id}': forward reference to later step '{referenced_step}'")]
    ForwardReference {
        step_id: String,
        referenced_step: String,
    },
    #[error("step '{step_id}': outputs reference unknown step '{referenced_step}'")]
    UnknownStepReference {
        step_id: String,
        referenced_step: String,
    },
    #[error("step '{step_id}': referenced output '{output_name}' not declared on step '{ref_step}'")]
    MissingOutput {
        step_id: String,
        ref_step: String,
        output_name: String,
    },
    #[error("step '{step_id}': auth.bearer set but operation '{operation_id}' has no bearer security")]
    AuthOnUnsecuredOp {
        step_id: String,
        operation_id: String,
    },
    #[error("step '{step_id}': {detail}")]
    UnsupportedConstruct { step_id: String, detail: String },
    #[error("contract error: {0}")]
    Contract(#[from] contract::ContractError),
    #[error("invalid mutator mode '{mode}' in step '{step_id}'")]
    InvalidMutatorMode { step_id: String, mode: String },
    #[error("step '{step_id}': mutator type (use) missing")]
    MissingMutatorType { step_id: String },
}

// ── Main entry point ───────────────────────────────────────────────

/// Convert a `RawExperiment` into a validated `Experiment`, resolving
/// references against OpenAPI contracts.
pub fn resolve_experiment(
    raw: RawExperiment,
    contracts: &HashMap<String, ResolvedContract>,
    experiment_dir: &Path,
) -> Result<Experiment, ResolutionError> {
    let block = raw.experiment;

    // Resolve exploration
    let exploration = resolve_exploration(&block)?;

    // Resolve target
    let target = resolve_target(&block)?;

    // Resolve adapters
    let adapters = resolve_adapters(&block, experiment_dir)?;

    // Resolve steps (ordered, with contract-aware checks)
    let steps = resolve_steps(&block.steps, &adapters, contracts)?;

    Ok(Experiment {
        name: block.name,
        max_cases: MaxCases::new(block.max_cases),
        exploration,
        target,
        adapters,
        steps,
    })
}

// ── Exploration ─────────────────────────────────────────────────────

fn resolve_exploration(
    block: &RawExperimentBlock,
) -> Result<ExplorationConfig, ResolutionError> {
    let expl = match &block.exploration {
        Some(e) => e,
        None => {
            return Ok(ExplorationConfig {
                strategy: ExplorationStrategy::Enumerate,
            });
        }
    };

    let strategy = match expl.strategy.as_str() {
        "enumerate" => ExplorationStrategy::Enumerate,
        "fuzz" => ExplorationStrategy::Fuzz {
            max_execs: expl.max_execs.unwrap_or(10000),
            duration_s: expl.duration_s.unwrap_or(60),
            seed: expl.seed,
        },
        other => {
            return Err(ResolutionError::UnsupportedConstruct {
                step_id: "(experiment)".into(),
                detail: format!("unknown strategy: '{}'", other),
            });
        }
    };

    Ok(ExplorationConfig { strategy })
}

// ── Target ──────────────────────────────────────────────────────────

fn resolve_target(
    block: &RawExperimentBlock,
) -> Result<Option<TargetConfig>, ResolutionError> {
    let raw_target = match &block.target {
        Some(t) => t,
        None => return Ok(None),
    };

    // Only ${target.port} and ${env.NAME[:-default]} expand in the target block.
    let values = raw_target
        .command
        .iter()
        .map(|value| ("command".to_string(), value))
        .chain(raw_target.env.iter().flatten().map(|(key, value)| (format!("env.{key}"), value)));
    for (field, value) in values {
        let references = match selectors::parse_binding_value(&Value::String(value.clone())) {
            selectors::BindingParseResult::Error(error) => {
                return Err(ResolutionError::InvalidTargetReference {
                    field,
                    detail: error.to_string(),
                })
            }
            selectors::BindingParseResult::Reference(reference) => vec![reference],
            selectors::BindingParseResult::Composite(parts) => parts
                .into_iter()
                .filter_map(|part| match part {
                    CompositePart::Reference(reference) => Some(reference),
                    CompositePart::Literal(_) => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        if let Some(reference) = references
            .iter()
            .find(|reference| !matches!(reference, Reference::Env { .. } | Reference::TargetPort))
        {
            return Err(ResolutionError::InvalidTargetReference {
                field,
                detail: format!("{reference} is not available here; use ${{target.port}} or ${{env.NAME}}"),
            });
        }
    }

    let coverage = raw_target.coverage.as_ref().map(|c| {
        let transport = match c.transport.as_str() {
            "mmap_file" => CoverageTransport::MmapFile,
            "afl_shm" => CoverageTransport::AflShm,
            _ => CoverageTransport::MmapFile,
        };
        CoverageConfig {
            transport,
            map_size: c.map_size,
        }
    });

    Ok(Some(TargetConfig {
        command: raw_target.command.clone(),
        ready: ReadyConfig {
            path: raw_target.ready.path.clone(),
            timeout_ms: raw_target.ready.timeout_ms,
        },
        coverage,
        cwd: raw_target.cwd.clone(),
        env: raw_target.env.clone().unwrap_or_default(),
    }))
}

// ── Adapters ────────────────────────────────────────────────────────

fn resolve_adapters(
    block: &RawExperimentBlock,
    _experiment_dir: &Path,
) -> Result<AdapterConfigs, ResolutionError> {
    let http = block.adapters.http.as_ref().map(|http_adapter| {
        HttpAdapterConfig {
            base_url: http_adapter.base_url.clone(),
            openapi_path: http_adapter.openapi.clone(),
            timeout_ms: http_adapter.timeout_ms,
        }
    });

    Ok(AdapterConfigs { http })
}

// ── Steps ───────────────────────────────────────────────────────────

fn resolve_steps(
    raw_steps: &[RawStep],
    adapters: &AdapterConfigs,
    contracts: &HashMap<String, ResolvedContract>,
) -> Result<Vec<Step>, ResolutionError> {
    // Track declared output names per step (for cross-step ref validation)
    let mut declared_outputs: HashMap<String, HashSet<String>> = HashMap::new();
    let mut resolved_steps: Vec<Step> = Vec::new();
    let step_names: HashSet<&str> = raw_steps.iter().map(|s| s.id.as_str()).collect();

    for raw_step in raw_steps {
        // Resolve adapter kind
        let adapter_kind = match raw_step.adapter.as_str() {
            "http" => AdapterKind::Http,
            other => {
                return Err(ResolutionError::UnknownAdapter(other.into()));
            }
        };

        // Look up the contract and operation
        let contract = match adapter_kind {
            AdapterKind::Http => adapters.http.as_ref().and_then(|http_cfg| {
                contracts.get(http_cfg.openapi_path.as_str())
            }),
        };

        let operation = match contract {
            Some(c) => c.operations.get(&raw_step.operation_id).ok_or_else(|| {
                ResolutionError::UnknownOperation {
                    adapter: raw_step.adapter.clone(),
                    operation_id: raw_step.operation_id.clone(),
                }
            })?,
            None => {
                // No contract means we can't validate, but we still resolve
                // (the validate command may run without contracts)
                return Err(ResolutionError::UnknownOperation {
                    adapter: raw_step.adapter.clone(),
                    operation_id: raw_step.operation_id.clone(),
                });
            }
        };

        // Check auth on unsecured op
        if raw_step.auth.is_some() {
            let has_bearer = operation.security.iter().any(|s| s.scheme_name == "bearerAuth");
            if !has_bearer {
                return Err(ResolutionError::AuthOnUnsecuredOp {
                    step_id: raw_step.id.clone(),
                    operation_id: raw_step.operation_id.clone(),
                });
            }
        }

        // Resolve bindings
        let bindings = resolve_bindings(
            raw_step,
            &step_names,
            &resolved_steps,
            &declared_outputs,
            operation,
        )?;

        for parameter in &operation.path_params {
            if parameter.required && !bindings.iter().any(|binding| {
                matches!(&binding.selector, selectors::InputSelector::PathParam(name) if name == &parameter.name)
            }) {
                return Err(ResolutionError::MissingPathBinding {
                    step_id: raw_step.id.clone(),
                    parameter: parameter.name.clone(),
                });
            }
        }

        // Resolve auth
        let auth = resolve_auth(
            raw_step,
            &step_names,
            &resolved_steps,
            &declared_outputs,
        )?;

        // Resolve outputs
        let outputs = resolve_outputs(raw_step)?;

        // Track declared outputs
        let output_names: HashSet<String> =
            outputs.iter().map(|o| o.name.clone()).collect();
        declared_outputs.insert(raw_step.id.clone(), output_names);

        // Resolve oracles
        let oracles = resolve_oracles(
            raw_step,
            &step_names,
            &resolved_steps,
            &declared_outputs,
        )?;

        // Resolve mutators
        let mutators = resolve_mutators(raw_step, operation)?;

        let mut declared_statuses: Vec<u16> = operation.responses.keys().copied().collect();
        declared_statuses.sort_unstable();

        // Resolve examples
        let examples = resolve_examples(raw_step)?;

        resolved_steps.push(Step {
            id: raw_step.id.clone(),
            adapter: adapter_kind,
            operation_id: raw_step.operation_id.clone(),
            bindings,
            auth,
            outputs,
            mutators,
            oracles,
            examples,
            declared_statuses,
        });
    }

    Ok(resolved_steps)
}

// ── Bindings ────────────────────────────────────────────────────────

fn resolve_bindings(
    raw_step: &RawStep,
    step_names: &HashSet<&str>,
    resolved_steps: &[Step],
    declared_outputs: &HashMap<String, HashSet<String>>,
    _operation: &ResolvedOperation,
) -> Result<Vec<Binding>, ResolutionError> {
    let mut bindings = Vec::new();

    let bind_map = match &raw_step.bind {
        Some(m) => m,
        None => return Ok(bindings),
    };

    for (key, value) in bind_map {
        // Parse the key as an input selector
        let selector = selectors::parse_input_selector(key).map_err(|e| {
            ResolutionError::UnknownSelector {
                step_id: raw_step.id.clone(),
                selector: e,
            }
        })?;

        // Parse the value
        let binding_value = match selectors::parse_binding_value(value) {
            BindingParseResult::Literal(s) => BindingValue::Literal(Value::String(s)),
            BindingParseResult::LiteralValue(v) => BindingValue::Literal(v),
            BindingParseResult::Reference(r) => {
                // Validate the reference
                validate_reference(
                    &raw_step.id,
                    &r,
                    step_names,
                    resolved_steps,
                    declared_outputs,
                )?;
                BindingValue::Reference(r)
            }
            BindingParseResult::Composite(parts) => {
                // Validate all references in the composite
                for part in &parts {
                    if let CompositePart::Reference(r) = part {
                        validate_reference(
                            &raw_step.id,
                            r,
                            step_names,
                            resolved_steps,
                            declared_outputs,
                        )?;
                    }
                }
                BindingValue::Composite(parts)
            }
            BindingParseResult::Error(e) => {
                return Err(ResolutionError::UnsupportedConstruct {
                    step_id: raw_step.id.clone(),
                    detail: format!("invalid reference in binding '{}': {}", key, e),
                });
            }
        };

        bindings.push(Binding {
            selector,
            value: binding_value,
        });
    }

    Ok(bindings)
}

// ── Auth ────────────────────────────────────────────────────────────

fn resolve_auth(
    raw_step: &RawStep,
    step_names: &HashSet<&str>,
    resolved_steps: &[Step],
    declared_outputs: &HashMap<String, HashSet<String>>,
) -> Result<Option<AuthConfig>, ResolutionError> {
    let raw_auth = match &raw_step.auth {
        Some(a) => a,
        None => return Ok(None),
    };

    let ref_str = &raw_auth.bearer;
    let parsed = selectors::parse_reference(ref_str);

    match parsed {
        Some(Ok(r)) => {
            validate_reference(
                &raw_step.id,
                &r,
                step_names,
                resolved_steps,
                declared_outputs,
            )?;
            Ok(Some(AuthConfig { bearer: r }))
        }
        Some(Err(e)) => Err(ResolutionError::UnsupportedConstruct {
            step_id: raw_step.id.clone(),
            detail: format!("invalid auth reference: {}", e),
        }),
        None => Err(ResolutionError::UnsupportedConstruct {
            step_id: raw_step.id.clone(),
            detail: format!(
                "auth.bearer must be a reference, got: '{}'",
                ref_str
            ),
        }),
    }
}

// ── Outputs ─────────────────────────────────────────────────────────

fn resolve_outputs(
    raw_step: &RawStep,
) -> Result<Vec<OutputDecl>, ResolutionError> {
    let outputs_map = match &raw_step.outputs {
        Some(m) => m,
        None => return Ok(Vec::new()),
    };

    let mut outputs = Vec::new();
    for (name, raw_output) in outputs_map {
        // Parse `from` as observation selector
        let from_raw = raw_output.from.strip_prefix("response.").unwrap_or(&raw_output.from);
        let from = selectors::parse_observation_selector(from_raw).map_err(|e| {
            ResolutionError::UnknownSelector {
                step_id: raw_step.id.clone(),
                selector: e,
            }
        })?;

        // Parse when_status
        let when_status = raw_output.when_status.as_ref().map(|v| match v {
            Value::Number(n) => n.as_u64().map(|n| vec![n as u16]).unwrap_or_default(),
            Value::Array(arr) => arr
                .iter()
                .filter_map(|v| v.as_u64().map(|n| n as u16))
                .collect(),
            _ => Vec::new(),
        });

        outputs.push(OutputDecl {
            name: name.clone(),
            from,
            when_status,
            secret: raw_output.secret,
        });
    }

    Ok(outputs)
}

// ── Oracles ─────────────────────────────────────────────────────────

fn resolve_oracles(
    raw_step: &RawStep,
    step_names: &HashSet<&str>,
    resolved_steps: &[Step],
    _declared_outputs: &HashMap<String, HashSet<String>>,
) -> Result<Vec<OracleDecl>, ResolutionError> {
    let mut oracles = Vec::new();

    for raw_oracle in &raw_step.oracles {
        let oracle_type = match raw_oracle.r#use.as_str() {
            "openapi" => OracleType::OpenApi,
            "openapi_schema" => OracleType::OpenApiSchema,
            "assert" => OracleType::Assert,
            "record_absent" => OracleType::RecordAbsent,
            other => {
                return Err(ResolutionError::UnsupportedConstruct {
                    step_id: raw_step.id.clone(),
                    detail: format!("unknown oracle type: '{}'", other),
                });
            }
        };

        // Static validation requires an id on every oracle except `openapi`.
        let id = match (&raw_oracle.id, &oracle_type) {
            (Some(id), _) => id.clone(),
            (None, OracleType::OpenApi) => "openapi".into(),
            (None, _) => {
                return Err(ResolutionError::UnsupportedConstruct {
                    step_id: raw_step.id.clone(),
                    detail: format!("oracle 'use: {}' needs an 'id'", raw_oracle.r#use),
                });
            }
        };

        let kind = match raw_oracle.kind.as_deref() {
            None => None,
            Some("security") => Some(OracleKind::Security),
            Some("check") => Some(OracleKind::Check),
            Some(other) => {
                return Err(ResolutionError::UnsupportedConstruct {
                    step_id: raw_step.id.clone(),
                    detail: format!("unknown oracle kind '{other}' (expected 'security' or 'check')"),
                });
            }
        };

        let parse_selector = |source: &str| {
            let (step_id, selector) = selectors::parse_full_observation_selector(source)
                .map_err(|selector| ResolutionError::UnknownSelector {
                    step_id: raw_step.id.clone(),
                    selector,
                })?;
            if let Some(step_id) = step_id {
                if !step_names.contains(step_id.as_str()) {
                    return Err(ResolutionError::UnknownStepReference {
                        step_id: raw_step.id.clone(),
                        referenced_step: step_id,
                    });
                }
                if step_id == raw_step.id {
                    return Ok(selector);
                }
                if !resolved_steps.iter().any(|step| step.id == step_id) {
                    return Err(ResolutionError::ForwardReference {
                        step_id: raw_step.id.clone(),
                        referenced_step: step_id,
                    });
                }
                return Ok(ObservationSelector::Step {
                    step_id,
                    selector: Box::new(selector),
                });
            }
            Ok(selector)
        };

        let select = raw_oracle.select.as_deref().map(parse_selector).transpose()?;
        let when_condition = raw_oracle.r#when.as_ref().map(|when| {
            Ok::<_, ResolutionError>(WhenCondition {
                select: parse_selector(&when.select)?,
                equals: when.equals.clone(),
            })
        }).transpose()?;

        // Parse expect
        let expect = raw_oracle.expect.as_ref().map(|e| {
            if let Some(v) = &e.equals {
                ExpectCondition::Equals(v.clone())
            } else if let Some(v) = &e.one_of {
                ExpectCondition::OneOf(v.clone())
            } else if e.exists == Some(true) {
                ExpectCondition::Exists
            } else if e.absent == Some(true) {
                ExpectCondition::Absent
            } else {
                ExpectCondition::Exists
            }
        });

        // Parse record_reference
        let record_reference = raw_oracle
            .record_reference
            .as_deref()
            .map(|s| {
                let stripped = s.strip_prefix("response.").unwrap_or(s);
                selectors::parse_observation_selector(stripped).map_err(|e| {
                    ResolutionError::UnknownSelector {
                        step_id: raw_step.id.clone(),
                        selector: e,
                    }
                })
            })
            .transpose()?;

        oracles.push(OracleDecl {
            id,
            oracle_type,
            kind,
            setup: raw_oracle.setup.unwrap_or(false),
            select,
            when_condition,
            expect,
            record_reference,
        });
    }

    Ok(oracles)
}

// ── Mutators ────────────────────────────────────────────────────────

fn resolve_mutators(
    raw_step: &RawStep,
    operation: &ResolvedOperation,
) -> Result<Vec<MutatorDecl>, ResolutionError> {
    let cases = match &raw_step.cases {
        Some(RawStepCases { mutators, .. }) => mutators,
        None => return Ok(Vec::new()),
    };
    let invalid = |detail: String| ResolutionError::UnsupportedConstruct {
        step_id: raw_step.id.clone(),
        detail,
    };

    let mut result = Vec::new();
    for raw in cases {
        let decl = match raw.r#use.as_str() {
            "json" => {
                let mode = match raw.mode.as_deref().unwrap_or("edge_cases") {
                    "edge_cases" | "schema_valid" => JsonMutatorMode::EdgeCases,
                    "unconstrained" => JsonMutatorMode::Unconstrained,
                    "schema" => JsonMutatorMode::Schema,
                    other => {
                        return Err(ResolutionError::InvalidMutatorMode {
                            step_id: raw_step.id.clone(),
                            mode: other.into(),
                        });
                    }
                };
                let fields = raw.fields.clone().unwrap_or_default();
                let body_schema = operation.request_body.as_ref().map(|body| &body.schema.0);
                let mut field_schemas = HashMap::new();
                for field in &fields {
                    match body_schema.and_then(|schema| field_schema(schema, field)) {
                        Some(found) => {
                            field_schemas.insert(field.clone(), found);
                        }
                        None if mode == JsonMutatorMode::Schema => {
                            return Err(invalid(format!(
                                "mode: schema needs field '{field}' in the request schema of '{}'",
                                operation.operation_id
                            )));
                        }
                        None => {}
                    }
                }
                MutatorDecl::Json {
                    fields: raw.fields.clone(),
                    mode,
                    values: raw.values.clone().unwrap_or_default(),
                    field_schemas,
                }
            }
            "headers" => {
                let fields = raw.fields.clone().unwrap_or_default();
                let values = raw
                    .values
                    .iter()
                    .flatten()
                    .map(|value| match value {
                        Value::String(text) => Ok(text.clone()),
                        other => Err(invalid(format!(
                            "header values must be strings, found {other}"
                        ))),
                    })
                    .collect::<Result<_, _>>()?;
                MutatorDecl::Headers { fields, values }
            }
            _other => {
                return Err(ResolutionError::MissingMutatorType {
                    step_id: raw_step.id.clone(),
                });
            }
        };
        result.push(decl);
    }

    Ok(result)
}

/// Find a dotted field (`metadata.source`) in a resolved request schema by
/// following `properties`, and record whether its parent requires it.
fn field_schema(body_schema: &Value, path: &str) -> Option<FieldSchema> {
    let keys: Vec<&str> = path.split('.').collect();
    let (last, parents) = keys.split_last()?;
    let parent = parents.iter().try_fold(body_schema, |schema, key| {
        schema.get("properties")?.get(*key)
    })?;
    let schema = parent.get("properties")?.get(*last)?.clone();
    let required = parent
        .get("required")
        .and_then(Value::as_array)
        .is_some_and(|required| required.iter().any(|name| name.as_str() == Some(*last)));
    Some(FieldSchema { schema, required })
}

// ── Examples ────────────────────────────────────────────────────────

use crate::experiment::types::ExampleCase;

fn resolve_examples(raw_step: &RawStep) -> Result<Vec<ExampleCase>, ResolutionError> {
    let raw_examples = match &raw_step.cases {
        Some(cases) => &cases.examples,
        None => return Ok(Vec::new()),
    };

    // Detect duplicate example IDs
    let mut seen_ids: HashSet<&str> = HashSet::new();
    for raw in raw_examples {
        if !seen_ids.insert(&raw.id) {
            return Err(ResolutionError::UnsupportedConstruct {
                step_id: raw_step.id.clone(),
                detail: format!("duplicate example id '{}'", raw.id),
            });
        }
    }

    raw_examples
        .iter()
        .map(|raw| {
            Ok(ExampleCase {
                id: raw.id.clone(),
                field: raw.field.clone(),
                value: raw.value.clone(),
                expect_status: raw.expect_status,
            })
        })
        .collect()
}

// ── Reference validation ────────────────────────────────────────────

fn validate_reference(
    step_id: &str,
    r: &Reference,
    step_names: &HashSet<&str>,
    resolved_steps: &[Step],
    declared_outputs: &HashMap<String, HashSet<String>>,
) -> Result<(), ResolutionError> {
    match r {
        Reference::StepOutput {
            step_id: ref_step,
            output_name,
        } => {
            // Check step exists
            if !step_names.contains(ref_step.as_str()) {
                return Err(ResolutionError::UnknownStepReference {
                    step_id: step_id.into(),
                    referenced_step: ref_step.clone(),
                });
            }

            // Check it's not a forward reference
            if let Some(ref_idx) =
                resolved_steps.iter().position(|s| s.id == *ref_step)
            {
                let cur_idx = resolved_steps.len();
                if ref_idx >= cur_idx {
                    return Err(ResolutionError::ForwardReference {
                        step_id: step_id.into(),
                        referenced_step: ref_step.clone(),
                    });
                }
            }

            // Check output is declared
            if let Some(outputs) = declared_outputs.get(ref_step.as_str()) {
                if !outputs.contains(output_name.as_str()) {
                    return Err(ResolutionError::MissingOutput {
                        step_id: step_id.into(),
                        ref_step: ref_step.clone(),
                        output_name: output_name.clone(),
                    });
                }
            }
        }
        Reference::Env { .. } | Reference::CaseId | Reference::TargetPort => {
            // Runtime references — always valid at resolution time
        }
    }
    Ok(())
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use std::path::PathBuf;

    fn examples_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples")
    }

    fn load_contracts() -> HashMap<String, ResolvedContract> {
        let mut contracts = HashMap::new();
        let openapi_path = examples_dir().join("create-record/openapi.yaml");
        let contract = contract::load_contract(&openapi_path).unwrap();
        // Key by the relative path as it appears in the experiment YAML
        contracts.insert("./openapi.yaml".into(), contract.clone());
        contracts.insert("../create-record/openapi.yaml".into(), contract);
        contracts
    }

    #[test]
    fn test_resolves_create_record_experiment() {
        let path = examples_dir().join("create-record/experiment.yaml");
        let raw = config::load(&path).unwrap();
        let contracts = load_contracts();
        let exp = resolve_experiment(raw, &contracts, &examples_dir()).unwrap();

        assert_eq!(exp.name, "create-record");
        assert_eq!(exp.max_cases.get(), 12);
        assert_eq!(exp.steps.len(), 4);

        // Step 0: login
        let login = &exp.steps[0];
        assert_eq!(login.id, "login");
        assert_eq!(login.operation_id, "login");
        assert_eq!(login.bindings.len(), 2);
        assert_eq!(login.outputs.len(), 1);
        assert_eq!(login.outputs[0].name, "token");
        assert!(login.outputs[0].secret);
        assert_eq!(login.oracles.len(), 2);
        assert_eq!(login.oracles[0].oracle_type, OracleType::OpenApi);
        assert_eq!(login.oracles[1].id, "login-succeeds");
        assert!(login.oracles[1].setup);

        // Step 1: get_template
        let get_tpl = &exp.steps[1];
        assert_eq!(get_tpl.id, "get_template");
        assert!(get_tpl.auth.is_some());
        assert_eq!(get_tpl.mutators.len(), 1);
        assert_eq!(get_tpl.outputs[0].name, "template_id");

        // Step 2: post_record
        let post = &exp.steps[2];
        assert_eq!(post.id, "post_record");
        assert_eq!(post.bindings.len(), 3);

        // Step 3: verify_record
        let verify = &exp.steps[3];
        assert_eq!(verify.id, "verify_record");
        assert_eq!(verify.oracles.len(), 3);
        assert_eq!(verify.oracles[1].id, "accepted-create-persists");
        assert_eq!(verify.oracles[2].id, "rejected-create-does-not-persist");
    }

    #[test]
    fn test_resolves_fuzz_probe_experiment() {
        let path = examples_dir().join("fuzz-probe/experiment.yaml");
        let raw = config::load(&path).unwrap();
        let contracts = load_contracts();
        let exp = resolve_experiment(raw, &contracts, &examples_dir()).unwrap();

        assert_eq!(exp.name, "fuzz-probe");
        assert_eq!(exp.steps.len(), 2);
        assert!(matches!(
            exp.steps[1].mutators.as_slice(),
            [MutatorDecl::Json { mode: JsonMutatorMode::Schema, values, field_schemas, .. }]
                if values.len() == 3 && field_schemas["value"].required
        ));

        let probe = &exp.steps[0];
        assert_eq!(probe.operation_id, "probeString");
        assert!(matches!(
            exp.exploration.strategy,
            ExplorationStrategy::Fuzz { duration_s: 5, max_execs: 2000, seed: Some(1) }
        ));
        assert!(matches!(
            probe.mutators.as_slice(),
            [MutatorDecl::Json { mode: JsonMutatorMode::Unconstrained, .. }]
        ));
        assert!(exp.target.as_ref().is_some_and(|target| target.coverage.is_none()));
    }

    #[test]
    fn test_rejects_unknown_operation() {
        let mut block = RawExperimentBlock {
            name: "test".into(),
            max_cases: 4,
            exploration: None,
            target: None,
            adapters: config::RawAdapters { http: None },
            steps: vec![],
        };

        block.steps.push(RawStep {
            id: "s1".into(),
            adapter: "http".into(),
            operation_id: "nonexistent".into(),
            bind: None,
            auth: None,
            outputs: None,
            cases: None,
            oracles: vec![],
        });

        let raw = RawExperiment {
            version: 1,
            experiment: block,
        };

        let contracts = load_contracts();
        let err = resolve_experiment(raw, &contracts, &examples_dir()).unwrap_err();
        assert!(
            matches!(err, ResolutionError::UnknownOperation { .. }),
            "expected UnknownOperation, got {:?}",
            err
        );
    }

    #[test]
    fn test_field_schema_follows_nested_properties() {
        let schema = serde_json::json!({
            "type": "object",
            "required": ["metadata"],
            "properties": {
                "metadata": {
                    "type": "object",
                    "required": ["source"],
                    "properties": { "source": { "type": "string" }, "note": { "type": "string" } }
                }
            }
        });
        let source = field_schema(&schema, "metadata.source").unwrap();
        assert!(source.required);
        assert_eq!(source.schema["type"], "string");
        assert!(!field_schema(&schema, "metadata.note").unwrap().required);
        assert!(field_schema(&schema, "metadata").unwrap().required);
        assert!(field_schema(&schema, "metadata.missing").is_none());
    }

    #[test]
    fn test_schema_mode_needs_the_field_in_the_request_schema() {
        let path = examples_dir().join("fuzz-probe/experiment.yaml");
        let source = std::fs::read_to_string(&path).unwrap();
        let contracts = load_contracts();
        let resolve = |yaml: &str| {
            resolve_experiment(config::parse(yaml).unwrap(), &contracts, &examples_dir())
        };
        assert!(resolve(&source).is_ok());
        let error = resolve(&source.replace("fields: [value]\n            mode: schema", "fields: [nope]\n            mode: schema"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("needs field 'nope'"), "{error}");
    }

    #[test]
    fn test_rejects_auth_on_unsecured_op() {
        use crate::config::RawHttpAdapter;

        let mut block = RawExperimentBlock {
            name: "test".into(),
            max_cases: 4,
            exploration: None,
            target: None,
            adapters: config::RawAdapters {
                http: Some(RawHttpAdapter {
                    base_url: "http://127.0.0.1:8080".into(),
                    openapi: "./openapi.yaml".into(),
                    timeout_ms: 2000,
                }),
            },
            steps: vec![],
        };

        // login has no security requirements (it's the auth endpoint)
        block.steps.push(RawStep {
            id: "s1".into(),
            adapter: "http".into(),
            operation_id: "login".into(),
            bind: None,
            auth: Some(config::RawAuth {
                bearer: "${env.TOKEN}".into(),
            }),
            outputs: None,
            cases: None,
            oracles: vec![],
        });

        let raw = RawExperiment {
            version: 1,
            experiment: block,
        };

        let contracts = load_contracts();
        let err = resolve_experiment(raw, &contracts, &examples_dir()).unwrap_err();
        assert!(
            matches!(err, ResolutionError::AuthOnUnsecuredOp { .. }),
            "expected AuthOnUnsecuredOp, got {:?}",
            err
        );
    }
}
