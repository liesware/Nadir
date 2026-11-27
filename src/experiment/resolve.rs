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
    ProtocolConfigs, DebugMode, HttpField, MAX_DEBUG_AUTO_S, ProtocolKind, AuthConfig, Binding, BindingValue, CompositePart,
    CoverageConfig, CoverageTransport, ExpectCondition, ExplorationConfig,
    ExplorationStrategy, Experiment, FieldSchema, HttpProtocolConfig, JsonMutatorMode,
    MaxCases, MutatorDecl, OracleDecl, OracleKind, OracleType, OutputDecl,
    ReadyConfig, Step, TargetConfig, WhenCondition,
};

// ── Errors ─────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum ResolutionError {
    #[error("target.{field}: {detail}")]
    InvalidTargetReference { field: String, detail: String },
    #[error("unknown protocol name: '{0}'")]
    UnknownProtocol(String),
    #[error("step '{step_id}': operation '{selector}' not found in contract for protocol '{protocol}'")]
    UnknownOperation {
        step_id: String,
        protocol: String,
        selector: String,
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
    #[error("step '{step_id}': unknown mutator mode '{mode}' (expected edge_cases, unconstrained, schema, alpha, alphanum or raw; use: http also takes values and absent)")]
    InvalidMutatorMode { step_id: String, mode: String },
    #[error("step '{step_id}': mutator type (use) missing")]
    MissingMutatorType { step_id: String },
    #[error("{0}")]
    InvalidHidden(String),
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

    // Resolve protocols
    let protocols = resolve_protocols(&block, experiment_dir)?;

    // Resolve steps (ordered, with contract-aware checks)
    let steps = resolve_steps(&block.steps, &protocols, contracts, experiment_dir)?;
    if matches!(exploration.strategy, ExplorationStrategy::Enumerate) {
        if let Some(step) = steps.iter().find(|step| !step.key_mutators.is_empty()) {
            return Err(ResolutionError::UnsupportedConstruct {
                step_id: step.id.clone(),
                detail: "generated key renames require strategy: fuzz".into(),
            });
        }
    }

    let experiment = Experiment {
        name: block.name,
        max_cases: MaxCases::new(block.max_cases),
        exploration,
        target,
        protocols,
        steps,
        hidden: block.hidden.unwrap_or_default(),
    };
    resolve_hidden(&experiment)?;
    Ok(experiment)
}

/// Each `hidden` name must be read by some `${env.NAME}`, once.
fn resolve_hidden(experiment: &Experiment) -> Result<(), ResolutionError> {
    let used = experiment.env_references();
    let mut seen = HashSet::new();
    for name in &experiment.hidden {
        if !seen.insert(name.as_str()) {
            return Err(ResolutionError::InvalidHidden(format!("hidden variable '{name}' is listed twice")));
        }
        if !used.iter().any(|(known, _)| known == name) {
            return Err(ResolutionError::InvalidHidden(format!(
                "hidden variable '{name}' is not used in any ${{env.{name}}} reference"
            )));
        }
    }
    Ok(())
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
            max_fields: expl.max_fields,
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

fn resolve_protocols(
    block: &RawExperimentBlock,
    _experiment_dir: &Path,
) -> Result<ProtocolConfigs, ResolutionError> {
    let http = block.protocols.http.as_ref().map(|http_adapter| {
        HttpProtocolConfig {
            base_url: http_adapter.base_url.clone(),
            openapi_path: http_adapter.openapi.clone(),
            timeout_ms: http_adapter.timeout_ms,
        }
    });

    Ok(ProtocolConfigs { http })
}

// ── Steps ───────────────────────────────────────────────────────────

fn resolve_steps(
    raw_steps: &[RawStep],
    protocols: &ProtocolConfigs,
    contracts: &HashMap<String, ResolvedContract>,
    experiment_dir: &Path,
) -> Result<Vec<Step>, ResolutionError> {
    // Track declared output names per step (for cross-step ref validation)
    let mut declared_outputs: HashMap<String, HashSet<String>> = HashMap::new();
    let mut resolved_steps: Vec<Step> = Vec::new();
    let step_names: HashSet<&str> = raw_steps.iter().map(|s| s.id.as_str()).collect();

    for raw_step in raw_steps {
        // Resolve protocol kind
        let protocol_kind = match raw_step.protocol.as_str() {
            "http" => ProtocolKind::Http,
            "http_raw" => ProtocolKind::HttpRaw,
            other => {
                return Err(ResolutionError::UnknownProtocol(other.into()));
            }
        };

        // Look up the contract and operation
        let contract = match protocol_kind {
            // `http_raw` builds its requests from the same contract.
            ProtocolKind::Http | ProtocolKind::HttpRaw => protocols.http.as_ref().and_then(|http_cfg| {
                contracts.get(http_cfg.openapi_path.as_str())
            }),
            #[cfg(test)]
            ProtocolKind::InMemory => None,
        };

        let invalid = |detail| ResolutionError::UnsupportedConstruct {
            step_id: raw_step.id.clone(),
            detail,
        };
        let format = match raw_step.format.as_deref().unwrap_or("json") {
            "json" => crate::formats::PayloadFormat::Json,
            other => return Err(invalid(format!("unknown format '{other}', expected json"))),
        };
        let (selector, operation) = match (&raw_step.operation_id, &raw_step.operation_path) {
            (Some(id), None) => (id, contract.and_then(|c| c.operation_by_id(id))),
            (None, Some(path)) => {
                let key = contract::parse_operation_path(path).ok_or_else(|| {
                    invalid(format!("invalid operation_path '{path}', expected /path.method"))
                })?;
                (path, contract.and_then(|c| c.operation(&key)))
            }
            _ => return Err(invalid("specify exactly one of operation_id and operation_path".into())),
        };
        let operation = operation.ok_or_else(|| ResolutionError::UnknownOperation {
            step_id: raw_step.id.clone(),
            protocol: raw_step.protocol.clone(),
            selector: selector.clone(),
        })?;

        if raw_step.body_example.is_some()
            && operation
                .request_body
                .as_ref()
                .is_none_or(|body| body.content_type != "application/json")
        {
            return Err(invalid("body_example requires an application/json request body".into()));
        }

        check_layer(raw_step, protocol_kind).map_err(invalid)?;

        // Check auth on unsecured op
        if raw_step.auth.is_some() {
            let has_bearer = operation.security.iter().any(|s| s.scheme_name == "bearerAuth");
            if !has_bearer {
                return Err(ResolutionError::AuthOnUnsecuredOp {
                    step_id: raw_step.id.clone(),
                    operation_id: selector.clone(),
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
        let (mutators, corpora) = resolve_mutators(raw_step, operation, experiment_dir)?;

        let mut declared_statuses: Vec<u16> = operation.responses.keys().copied().collect();
        declared_statuses.sort_unstable();

        // Resolve examples
        let (examples, key_mutators) = resolve_examples(raw_step, operation, &bindings)?;

        let debug = match &raw_step.debug {
            None | Some(crate::config::RawDebug::Enabled(false)) => None,
            Some(crate::config::RawDebug::Enabled(true)) => Some(DebugMode::Manual),
            Some(crate::config::RawDebug::Auto(auto)) => {
                if !(1..=MAX_DEBUG_AUTO_S).contains(&auto.auto) {
                    return Err(ResolutionError::UnsupportedConstruct {
                        step_id: raw_step.id.clone(),
                        detail: format!("debug.auto must be between 1 and {MAX_DEBUG_AUTO_S} seconds"),
                    });
                }
                Some(DebugMode::Auto(std::time::Duration::from_secs(u64::from(auto.auto))))
            }
        };

        resolved_steps.push(Step {
            id: raw_step.id.clone(),
            protocol: protocol_kind,
            format,
            operation_key: operation.key(),
            body_example: raw_step.body_example.clone(),
            bindings,
            auth,
            outputs,
            mutators,
            corpora,
            key_mutators,
            oracles,
            examples,
            declared_statuses,
            print: raw_step.print.unwrap_or(false),
            debug,
        });
    }

    Ok(resolved_steps)
}

/// Each step tests one layer: `http_raw` steps take only `malform`, and
/// `malform` only runs on `http_raw` steps.
fn check_layer(raw_step: &RawStep, protocol: ProtocolKind) -> Result<(), String> {
    let mutators = raw_step.cases.as_ref().map(|cases| cases.mutators.as_slice()).unwrap_or_default();
    if protocol == ProtocolKind::HttpRaw {
        if let Some(other) = mutators.iter().find(|mutator| mutator.r#use != "malform") {
            return Err(format!("protocol: http_raw only takes use: malform, found use: {}", other.r#use));
        }
        if raw_step.cases.as_ref().is_some_and(|cases| !cases.examples.is_empty()) {
            return Err("protocol: http_raw does not take cases.examples".into());
        }
        if raw_step.outputs.as_ref().is_some_and(|outputs| !outputs.is_empty()) {
            return Err("protocol: http_raw does not take outputs: a malformed response cannot feed other steps".into());
        }
    } else if mutators.iter().any(|mutator| mutator.r#use == "malform") {
        return Err("use: malform requires protocol: http_raw".into());
    }
    Ok(())
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

const JSON_MUTATOR_MODES: &[&str] = &[
    "edge_cases", "schema_valid", "unconstrained", "schema", "alpha", "alphanum", "alphanumeric", "raw",
];
/// `use: http` adds `values` (only the listed values) and `absent`.
const HTTP_MUTATOR_MODES: &[&str] = &[
    "edge_cases", "schema_valid", "unconstrained", "schema", "alpha", "alphanum", "alphanumeric", "raw",
    "values", "absent",
];

fn resolve_mutators(
    raw_step: &RawStep,
    operation: &ResolvedOperation,
    experiment_dir: &Path,
) -> Result<(Vec<MutatorDecl>, Vec<crate::mutation::text::Corpus>), ResolutionError> {
    let cases = match &raw_step.cases {
        Some(RawStepCases { mutators, .. }) => mutators,
        None => return Ok((Vec::new(), Vec::new())),
    };
    let invalid = |detail: String| ResolutionError::UnsupportedConstruct {
        step_id: raw_step.id.clone(),
        detail,
    };

    let mut result = Vec::new();
    let mut corpora = Vec::new();
    for raw in cases {
        let mutator = raw.r#use.as_str();
        if raw.keys.is_some() || raw.rename_to.is_some() {
            continue; // Deterministic key renames are resolved alongside the examples.
        }
        if raw.expect_status.is_some() {
            return Err(invalid("mutator expect_status requires keys and rename_to".into()));
        }
        if matches!(mutator, "file" | "file_fuzz") {
            let corpus = resolve_corpus(raw, operation, experiment_dir).map_err(invalid)?;
            corpora.push(corpus);
            continue;
        }
        if raw.path.is_some() || raw.repetitions.is_some() || raw.seed.is_some() {
            return Err(invalid("path, repetitions and seed are corpus options".into()));
        }
        let known_modes = match mutator {
            "json" => Some(JSON_MUTATOR_MODES),
            "http" => Some(HTTP_MUTATOR_MODES),
            _ => None,
        };
        if let (Some(modes), Some(other)) = (known_modes, raw.mode.as_deref()) {
            if !modes.contains(&other) {
                return Err(ResolutionError::InvalidMutatorMode {
                    step_id: raw_step.id.clone(),
                    mode: other.into(),
                });
            }
        }
        if mutator == "headers" && (raw.mode.is_some() || raw.length.is_some()) {
            return Err(invalid(
                "use: headers only takes fields and values; use: http for modes and length".into(),
            ));
        }
        if raw.length.is_some()
            && (!matches!(mutator, "json" | "http")
                || !matches!(raw.mode.as_deref(), Some("alpha" | "alphanum" | "alphanumeric" | "raw")))
        {
            let found = match raw.mode.as_deref() {
                Some(mode) => format!("use: {mutator}, mode: {mode}"),
                None => format!("use: {mutator} without mode"),
            };
            return Err(invalid(format!(
                "length requires use: json or http with mode: alpha, alphanum or raw (found {found})"
            )));
        }
        let decl = match raw.r#use.as_str() {
            "json" => {
                let mode = match raw.mode.as_deref().unwrap_or("edge_cases") {
                    "edge_cases" | "schema_valid" => JsonMutatorMode::EdgeCases,
                    "unconstrained" => JsonMutatorMode::Unconstrained,
                    "schema" => JsonMutatorMode::Schema,
                    "alphanum" | "alphanumeric" if raw.length.is_none() => JsonMutatorMode::Alphanumeric,
                    "alpha" | "alphanum" | "alphanumeric" | "raw" => {
                        JsonMutatorMode::Text(resolve_text_generator(raw, operation).map_err(invalid)?)
                    }
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
                                operation.label()
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
            "http" | "headers" => resolve_http_mutator(raw, operation).map_err(invalid)?,
            "malform" => resolve_malform(raw).map_err(invalid)?,
            _other => {
                return Err(ResolutionError::MissingMutatorType {
                    step_id: raw_step.id.clone(),
                });
            }
        };
        result.push(decl);
    }

    Ok((result, corpora))
}

/// `use: http`, or its alias `use: headers` (header names, `values` only).
fn resolve_http_mutator(
    raw: &crate::config::RawMutator,
    operation: &ResolvedOperation,
) -> Result<MutatorDecl, String> {
    let legacy = raw.r#use == "headers";
    let raw_fields = raw.fields.as_deref().unwrap_or_default();
    if raw_fields.is_empty() {
        return Err(format!("use: {} requires fields", raw.r#use));
    }
    let mut fields = Vec::new();
    for raw_field in raw_fields {
        let field = if legacy {
            HttpField::Header(raw_field.clone())
        } else {
            HttpField::parse(raw_field).ok_or_else(|| {
                format!("http field '{raw_field}' must start with header. or path.")
            })?
        };
        match &field {
            HttpField::Header(name) => {
                http::HeaderName::from_bytes(name.as_bytes())
                    .map_err(|_| format!("invalid header name '{name}'"))?;
            }
            HttpField::Path(name) => {
                if !operation.path_params.iter().any(|param| &param.name == name) {
                    return Err(format!(
                        "path parameter '{name}' is not in the path of '{}'",
                        operation.label()
                    ));
                }
            }
        }
        fields.push(field);
    }
    let mode = match raw.mode.as_deref().unwrap_or(if legacy { "values" } else { "edge_cases" }) {
        "edge_cases" | "schema_valid" => JsonMutatorMode::EdgeCases,
        "unconstrained" => JsonMutatorMode::Unconstrained,
        "schema" => JsonMutatorMode::Schema,
        "values" => JsonMutatorMode::Values,
        "absent" => JsonMutatorMode::Absent,
        "alphanum" | "alphanumeric" if raw.length.is_none() => JsonMutatorMode::Alphanumeric,
        _ => JsonMutatorMode::Text(resolve_text_generator(raw, operation)?),
    };
    let mut field_schemas = HashMap::new();
    for field in &fields {
        match parameter_schema(operation, field) {
            Some(schema) => {
                field_schemas.insert(field.selector(), schema);
            }
            None if mode == JsonMutatorMode::Schema => {
                return Err(format!(
                    "mode: schema needs '{}' declared as a parameter of '{}'",
                    field.selector(),
                    operation.label()
                ));
            }
            None => {}
        }
    }
    let values = raw
        .values
        .iter()
        .flatten()
        .map(|value| match value {
            Value::String(text) => Ok(text.clone()),
            other => Err(format!("{} values must be strings, found {other}", raw.r#use)),
        })
        .collect::<Result<_, _>>()?;
    Ok(MutatorDecl::Http { fields, mode, values, field_schemas })
}

/// `use: malform`: `mode: catalog` (the default) with an optional `only`.
fn resolve_malform(raw: &crate::config::RawMutator) -> Result<MutatorDecl, String> {
    use crate::adapters::http_raw::Malformation;
    match raw.mode.as_deref().unwrap_or("catalog") {
        "catalog" => {}
        "random" => return Err("malform mode: random is available in a later phase; use mode: catalog".into()),
        other => return Err(format!("unknown malform mode '{other}' (expected catalog)")),
    }
    if raw.fields.is_some() || raw.values.is_some() || raw.length.is_some() {
        return Err("use: malform takes only mode and only".into());
    }
    let malformations = match &raw.only {
        None => Malformation::ALL.to_vec(),
        Some(names) if names.is_empty() => return Err("malform only must list at least one name".into()),
        Some(names) => names
            .iter()
            .map(|name| {
                Malformation::from_name(name).ok_or_else(|| {
                    let known: Vec<&str> = Malformation::ALL.iter().map(|known| known.name()).collect();
                    format!("unknown malformation '{name}' (expected one of: {})", known.join(", "))
                })
            })
            .collect::<Result<_, _>>()?,
    };
    Ok(MutatorDecl::Malform { malformations })
}

/// The OpenAPI schema of a header or path parameter.
fn parameter_schema(operation: &ResolvedOperation, field: &HttpField) -> Option<FieldSchema> {
    let param = match field {
        HttpField::Header(name) => operation
            .header_params
            .iter()
            .find(|param| param.name.eq_ignore_ascii_case(name)),
        HttpField::Path(name) => operation.path_params.iter().find(|param| &param.name == name),
    }?;
    // A template-only path parameter has no schema to derive cases from.
    let declared = param.schema_json.as_object().is_some_and(|schema| !schema.is_empty());
    declared.then(|| FieldSchema { schema: param.schema_json.clone(), required: param.required })
}

fn text_field(raw: &crate::config::RawMutator) -> Result<&str, String> {
    match raw.fields.as_deref() {
        Some([field]) if field.split('.').all(|segment| !segment.is_empty()) => Ok(field),
        _ => Err("text length policies and corpus mutators require exactly one field".into()),
    }
}

fn resolve_text_generator(
    raw: &crate::config::RawMutator,
    operation: &ResolvedOperation,
) -> Result<crate::mutation::text::TextGenerator, String> {
    use crate::mutation::text::{Alphabet, TextGenerator, MAX_TEXT_CHARS};
    let field = text_field(raw)?;
    let http_field = (raw.r#use == "http").then(|| HttpField::parse(field)).flatten();
    let alphabet = match raw.mode.as_deref() {
        Some("alpha") => Alphabet::Alpha,
        None | Some("alphanum" | "alphanumeric") => Alphabet::Alphanum,
        // Headers cannot carry control characters.
        Some("raw") if http_field.as_ref().is_some_and(HttpField::is_header) => Alphabet::HeaderRaw,
        Some("raw") => Alphabet::Raw,
        Some(_) => return Err("text corpus mode must be alpha, alphanum or raw".into()),
    };
    let max = raw.length.as_ref().and_then(|length| length.max).unwrap_or(256);
    let (below, above) = if let Some(reference) = raw.length.as_ref().and_then(|length| length.reference) {
        (reference, reference)
    } else {
        let schema = match &http_field {
            Some(http_field) => parameter_schema(operation, http_field),
            None => operation.request_body.as_ref().and_then(|body| field_schema(&body.schema.0, field)),
        };
        let bound = |key| {
            schema.as_ref()?.schema.get(key)?.as_u64().and_then(|value| usize::try_from(value).ok())
        };
        bound("minLength").zip(bound("maxLength"))
            .ok_or_else(|| format!(
                "field '{field}' with mode: {} needs length: {{reference: N}} \
                 (its schema does not declare both minLength and maxLength)",
                raw.mode.as_deref().unwrap_or("alphanum")
            ))?
    };
    if below == 0 || below > above || above >= max || max > MAX_TEXT_CHARS {
        return Err(format!("field '{field}' requires 0 < lower bound <= upper bound < length.max <= {MAX_TEXT_CHARS}"));
    }
    Ok(TextGenerator { alphabet, below, above, max })
}

fn resolve_corpus(
    raw: &crate::config::RawMutator,
    operation: &ResolvedOperation,
    directory: &Path,
) -> Result<crate::mutation::text::Corpus, String> {
    let field = text_field(raw)?;
    if !operation.request_body.as_ref().is_some_and(|body| body.content_type == "application/json") {
        return Err("corpus mutators require an application/json request body".into());
    }
    if raw.values.is_some() {
        return Err("corpus mutators read values from their file".into());
    }
    let path = raw.path.as_deref().filter(|path| !path.is_empty())
        .ok_or("corpus mutators require path")?;
    let repetitions = raw.repetitions.filter(|count| *count > 0)
        .ok_or("corpus mutators require positive repetitions")?;
    let generator = if raw.r#use == "file_fuzz" {
        let seed = raw.seed.filter(|seed| *seed != 0).ok_or("file_fuzz requires a nonzero seed")?;
        Some((resolve_text_generator(raw, operation)?, seed))
    } else {
        if raw.length.is_some() || raw.mode.is_some() || raw.seed.is_some() {
            return Err("file sends each line unchanged; generation options belong to file_fuzz".into());
        }
        None
    };
    crate::mutation::text::Corpus::load(&directory.join(path), field.into(), repetitions, generator)
        .map_err(|error| format!("corpus '{path}': {error}"))
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

use crate::experiment::types::{ExampleAction, ExampleCase, JsonExample, JsonSyntaxFixture};

fn resolve_examples(
    raw_step: &RawStep,
    operation: &ResolvedOperation,
    bindings: &[Binding],
) -> Result<(Vec<ExampleCase>, Vec<crate::mutation::fuzz::KeyMutator>), ResolutionError> {
    let Some(cases) = &raw_step.cases else {
        return Ok((Vec::new(), Vec::new()));
    };
    let invalid = |detail| ResolutionError::UnsupportedConstruct {
        step_id: raw_step.id.clone(),
        detail,
    };
    let has_json_body = operation.request_body.as_ref()
        .is_some_and(|body| body.content_type == "application/json");
    let mut examples = Vec::new();
    let mut key_mutators = Vec::new();
    let mut seen_ids = HashSet::new();

    for raw in &cases.examples {
        let action = match (&raw.field, &raw.value, raw.syntax.as_deref()) {
            (Some(path), Some(value), None) => ExampleAction::Field {
                path: path.clone(),
                value: value.clone(),
            },
            (None, None, Some(syntax)) => {
                if !has_json_body || !(400..500).contains(&raw.expect_status) {
                    return Err(invalid(format!(
                        "syntax example '{}' requires a JSON request body and a 4xx expect_status",
                        raw.id,
                    )));
                }
                let fixture = match syntax {
                    "incomplete_object" => JsonSyntaxFixture::IncompleteObject,
                    "trailing_comma" => JsonSyntaxFixture::TrailingComma,
                    "leading_text" => JsonSyntaxFixture::LeadingText,
                    "trailing_text" => JsonSyntaxFixture::TrailingText,
                    _ => return Err(invalid(format!("unknown JSON syntax fixture '{syntax}'"))),
                };
                ExampleAction::Json(JsonExample::Syntax { fixture })
            }
            _ => return Err(invalid(format!(
                "example '{}' requires field and value, or syntax",
                raw.id,
            ))),
        };
        if !seen_ids.insert(raw.id.clone()) {
            return Err(invalid(format!("duplicate example id '{}'", raw.id)));
        }
        examples.push(ExampleCase { id: raw.id.clone(), action, expect_status: raw.expect_status });
    }

    for raw in &cases.mutators {
        if raw.keys.is_none() && raw.rename_to.is_none() {
            continue;
        }
        if raw.length.is_some() {
            return Err(invalid(
                "length does not apply to key renames (generated names have 4 to 24 characters)".into(),
            ));
        }
        if raw.r#use != "json" || raw.fields.is_some() || raw.values.is_some()
            || raw.path.is_some() || raw.repetitions.is_some() || raw.seed.is_some()
        {
            return Err(invalid("keys require a dedicated use: json mutator".into()));
        }
        if !has_json_body {
            return Err(invalid("key renames require an application/json request body".into()));
        }
        let keys = raw.keys.as_deref().filter(|keys| !keys.is_empty())
            .ok_or_else(|| invalid("key renames require a nonempty keys list".into()))?;
        let expected = match raw.expect_status {
            Some(status) if !(100..=599).contains(&status) => {
                return Err(invalid("key renames require a valid expect_status".into()));
            }
            status => status,
        };
        let body = contract::request_body_with_example(operation, bindings, raw_step.body_example.as_ref());
        let ensure_unbound = |path: &str| {
            if bindings.iter().any(|binding| matches!(
                &binding.selector, selectors::InputSelector::BodyField(parts)
                    if parts.iter().zip(path.split('.')).all(|(bound, part)| bound == part)
            )) {
                return Err(invalid(format!("key rename overlaps bound path '{path}'")));
            }
            Ok(())
        };
        for key in keys {
            if key.split('.').any(str::is_empty) {
                return Err(invalid(format!("invalid key path '{key}'")));
            }
            ensure_unbound(key)?;
            let (parent, source) = match key.rsplit_once('.') {
                Some((path, source)) => (crate::formats::json::get_object_path(&body, path), source),
                None => (Some(&body), key.as_str()),
            };
            let object = parent.and_then(Value::as_object)
                .ok_or_else(|| invalid(format!("cannot rename '{key}': key parent is not an object")))?;
            if !object.contains_key(source) {
                return Err(invalid(format!("cannot rename '{key}': source key is missing")));
            }
            let Some(destinations) = &raw.rename_to else {
                use crate::mutation::text::Alphabet;
                let alphabet = match raw.mode.as_deref() {
                    Some("alpha") => Alphabet::Alpha,
                    Some("alphanum" | "alphanumeric") => Alphabet::Alphanum,
                    Some("raw") => Alphabet::Raw,
                    Some(other) => return Err(invalid(format!(
                        "generated key renames require mode: alpha, alphanum or raw (found mode: {other})"
                    ))),
                    None => return Err(invalid(
                        "key renames need mode: alpha, alphanum or raw, or rename_to with expect_status".into(),
                    )),
                };
                key_mutators.push(crate::mutation::fuzz::KeyMutator {
                    key: key.clone(),
                    alphabet,
                    sibling_names: object.keys().cloned().collect(),
                    expect_status: expected,
                });
                continue;
            };
            if raw.mode.is_some() || destinations.is_empty() {
                return Err(invalid("fixed rename_to requires a nonempty list and no mode".into()));
            }
            let expected = expected
                .ok_or_else(|| invalid("fixed renames require expect_status".into()))?;
            for destination in destinations {
                if destination.is_empty() || !destination.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')
                }) {
                    return Err(invalid(format!("invalid rename destination '{destination}'")));
                }
                let destination_path = match key.rsplit_once('.') {
                    Some((parent, _)) => format!("{parent}.{destination}"),
                    None => destination.clone(),
                };
                ensure_unbound(&destination_path)?;
                if object.contains_key(destination) {
                    return Err(invalid(format!("cannot rename '{key}' to '{destination}': destination key already exists")));
                }
                let id = format!("rename:{key}->{destination}");
                if !seen_ids.insert(id.clone()) {
                    return Err(invalid(format!("duplicate example id '{id}'")));
                }
                examples.push(ExampleCase {
                    id,
                    action: ExampleAction::Json(JsonExample::Rename {
                        key: key.clone(),
                        destination: destination.clone(),
                    }),
                    expect_status: expected,
                });
            }
        }
    }
    Ok((examples, key_mutators))
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
        assert_eq!(login.operation_key, "/login.POST");
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
        assert_eq!(probe.operation_key, "/probe.POST");
        assert!(matches!(
            exp.exploration.strategy,
            ExplorationStrategy::Fuzz { duration_s: 5, max_execs: 2000, seed: Some(1), max_fields: None }
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
            protocols: config::RawProtocols { http: None },
            steps: vec![],
            hidden: None,
        };

        block.steps.push(RawStep {
            id: "s1".into(),
            protocol: "http".into(),
            format: None,
            operation_id: Some("nonexistent".into()),
            operation_path: None,
            body_example: None,
            bind: None,
            auth: None,
            outputs: None,
            cases: None,
            oracles: vec![],
            print: None,
            debug: None,
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
        use crate::config::RawHttpProtocol;

        let mut block = RawExperimentBlock {
            name: "test".into(),
            max_cases: 4,
            exploration: None,
            target: None,
            protocols: config::RawProtocols {
                http: Some(RawHttpProtocol {
                    base_url: "http://127.0.0.1:8080".into(),
                    openapi: "./openapi.yaml".into(),
                    timeout_ms: 2000,
                }),
            },
            steps: vec![],
            hidden: None,
        };

        // login has no security requirements (it's the auth endpoint)
        block.steps.push(RawStep {
            id: "s1".into(),
            protocol: "http".into(),
            format: None,
            operation_id: Some("login".into()),
            operation_path: None,
            body_example: None,
            bind: None,
            auth: Some(config::RawAuth {
                bearer: "${env.TOKEN}".into(),
            }),
            outputs: None,
            cases: None,
            oracles: vec![],
            print: None,
            debug: None,
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
