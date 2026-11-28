//! Convert `RawExperiment` into validated `Experiment`.
//!
//! Resolves every reference statically (except `${env.*}`), validates
//! step ordering, checks that every input selector exists in the OpenAPI
//! contract, and computes canonical descriptor ordering.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde_json::Value;

use crate::adapters::http::contract::{self, ResolvedContract, ResolvedOperation};
use crate::config::{RawExpectBlock, RawExperiment, RawExperimentBlock, RawStep, RawStepCases};
use crate::experiment::cli_command::CliCommand;
use crate::experiment::selectors::{
    self, ObservationSelector, Reference,
};
use crate::experiment::step_references::{StepReferenceError, StepReferences};
use crate::experiment::types::{
    ProtocolConfigs, CliProtocolConfig, DebugMode, EnvarMode, EnvarVariable, EnvarVariableRule,
    HttpField, MAX_DEBUG_AUTO_S, ProtocolKind, AuthConfig, Binding, BindingValue, CompositePart,
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
        if let Some(step) = steps.iter().find(|step| step.mutators.iter().any(|m| matches!(m, MutatorDecl::Body { .. }))) {
            return Err(ResolutionError::UnsupportedConstruct {
                step_id: step.id.clone(),
                detail: "use: body makes random cases and requires strategy: fuzz".into(),
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
    let mut used = experiment.env_references();
    // A cli step's `env_file`/`env` names count too.
    for step in &experiment.steps {
        for name in step.env_config.iter().flat_map(|env| env.keys()) {
            used.push((name.clone(), None));
        }
    }
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
            max_path_chars: http_adapter.max_path_chars.unwrap_or(crate::experiment::types::DEFAULT_MAX_PATH_CHARS),
        }
    });
    if let Some(http) = &http {
        if !(1..=crate::experiment::types::MAX_PATH_CHARS_LIMIT).contains(&http.max_path_chars) {
            return Err(ResolutionError::UnsupportedConstruct {
                step_id: String::new(),
                detail: format!(
                    "protocols.http.max_path_chars must be between 1 and {}",
                    crate::experiment::types::MAX_PATH_CHARS_LIMIT
                ),
            });
        }
    }

    let cli = block.protocols.cli.as_ref().map(|cli_adapter| {
        CliProtocolConfig {
            timeout_ms: cli_adapter.timeout_ms,
        }
    });

    Ok(ProtocolConfigs { http, cli })
}

// ── Steps ───────────────────────────────────────────────────────────

fn resolve_steps(
    raw_steps: &[RawStep],
    protocols: &ProtocolConfigs,
    contracts: &HashMap<String, ResolvedContract>,
    experiment_dir: &Path,
) -> Result<Vec<Step>, ResolutionError> {
    let mut resolved_steps = Vec::new();
    let references = StepReferences::new(raw_steps);

    for (position, raw_step) in raw_steps.iter().enumerate() {
        // Resolve protocol kind
        let protocol_kind = match raw_step.protocol.as_str() {
            "http" => ProtocolKind::Http,
            "http_raw" => ProtocolKind::HttpRaw,
            "cli" => ProtocolKind::Cli,
            other => {
                return Err(ResolutionError::UnknownProtocol(other.into()));
            }
        };

        // CLI steps do not need an OpenAPI contract.
        if protocol_kind == ProtocolKind::Cli {
            resolve_cli_step(
                raw_step, protocol_kind, &mut resolved_steps, &references, position, experiment_dir,
            )?;
            continue;
        }

        // Look up the contract and operation
        let contract = match protocol_kind {
            // `http_raw` builds its requests from the same contract.
            ProtocolKind::Http | ProtocolKind::HttpRaw => protocols.http.as_ref().and_then(|http_cfg| {
                contracts.get(http_cfg.openapi_path.as_str())
            }),
            ProtocolKind::Cli => None,
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

        let bindings = resolve_bindings(raw_step, &references, position)?;

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

        let auth = resolve_auth(raw_step, &references, position)?;
        let outputs = resolve_outputs(raw_step)?;
        let oracles = resolve_oracles(raw_step, &references, position)?;

        // Resolve mutators
        let (mutators, corpora) = resolve_mutators(raw_step, operation, experiment_dir)?;

        let mut declared_statuses: Vec<u16> = operation.responses.keys().copied().collect();
        declared_statuses.sort_unstable();

        // Resolve examples
        let (examples, key_mutators) = resolve_examples(raw_step, operation, &bindings)?;

        let debug = resolve_debug(raw_step)?;

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
            command: None,
            env_config: None,
            cli: None,
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

/// `debug: true | false | {auto: N}` of any step.
fn resolve_debug(raw_step: &RawStep) -> Result<Option<DebugMode>, ResolutionError> {
    Ok(match &raw_step.debug {
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
        })
}

// ── Bindings ────────────────────────────────────────────────────────

fn resolve_bindings(
    raw_step: &RawStep,
    references: &StepReferences<'_>,
    position: usize,
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

        let binding_value = selectors::parse_binding_value(value)
            .into_binding_value()
            .map_err(|error| ResolutionError::UnsupportedConstruct {
                step_id: raw_step.id.clone(),
                detail: format!("invalid reference in binding '{key}': {error}"),
            })?;
        for reference in binding_value.references() {
            validate_reference(&raw_step.id, reference, references, position)?;
        }

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
    references: &StepReferences<'_>,
    position: usize,
) -> Result<Option<AuthConfig>, ResolutionError> {
    let raw_auth = match &raw_step.auth {
        Some(a) => a,
        None => return Ok(None),
    };

    let ref_str = &raw_auth.bearer;
    let parsed = selectors::parse_reference(ref_str);

    match parsed {
        Some(Ok(r)) => {
            validate_reference(&raw_step.id, &r, references, position)?;
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
        let from = local_observation_selector(raw_step, &raw_output.from)
            .map_err(|error| ResolutionError::UnsupportedConstruct {
                step_id: raw_step.id.clone(),
                detail: format!("outputs.{name}.from: {error}"),
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

fn check_observation_protocol(
    step: &RawStep,
    selector: &ObservationSelector,
    source: &str,
) -> Result<(), String> {
    let compatible = match selector {
        ObservationSelector::Status | ObservationSelector::Header(_) | ObservationSelector::Body(_) => {
            matches!(step.protocol.as_str(), "http" | "http_raw")
        }
        ObservationSelector::ProcessExitCode
        | ObservationSelector::ProcessStdout
        | ObservationSelector::ProcessStderr
        | ObservationSelector::ProcessTimedOut
        | ObservationSelector::ProcessReady
        | ObservationSelector::ProcessStoppedCode => step.protocol == "cli",
        ObservationSelector::Step { .. } => false,
    };
    if !compatible {
        return Err(format!(
            "selector '{source}' is incompatible with protocol '{}' of step '{}'",
            step.protocol, step.id
        ));
    }
    Ok(())
}

fn local_observation_selector(step: &RawStep, source: &str) -> Result<ObservationSelector, String> {
    let selector = selectors::parse_observation_selector(
        source.strip_prefix("response.").unwrap_or(source),
    )?;
    check_observation_protocol(step, &selector, source)?;
    Ok(selector)
}

fn resolve_expect(expect: &RawExpectBlock) -> Result<ExpectCondition, String> {
    match (&expect.equals, &expect.one_of, expect.exists, expect.absent) {
        (Some(value), None, None, None) => Ok(ExpectCondition::Equals(value.clone())),
        (None, Some(values), None, None) if !values.is_empty() => {
            Ok(ExpectCondition::OneOf(values.clone()))
        }
        (None, None, Some(true), None) => Ok(ExpectCondition::Exists),
        (None, None, None, Some(true)) => Ok(ExpectCondition::Absent),
        _ => Err("requires exactly one of equals, non-empty one_of, exists: true, or absent: true".into()),
    }
}

fn resolve_oracles(
    raw_step: &RawStep,
    references: &StepReferences<'_>,
    position: usize,
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

        let invalid_field = |field: &str, error: String| ResolutionError::UnsupportedConstruct {
            step_id: raw_step.id.clone(),
            detail: format!("oracle '{id}' {field}: {error}"),
        };
        if matches!(&oracle_type, OracleType::Assert) {
            if raw_oracle.select.is_none() {
                return Err(invalid_field("select", "required for use: assert".into()));
            }
            if raw_oracle.expect.is_none() {
                return Err(invalid_field("expect", "required for use: assert".into()));
            }
        } else if raw_step.protocol == "cli" {
            return Err(invalid_field(
                "use",
                format!("'{}' requires an HTTP response", raw_oracle.r#use),
            ));
        }

        let parse_selector = |field: &str, source: &str| -> Result<ObservationSelector, ResolutionError> {
            let (step_id, selector) = selectors::parse_full_observation_selector(source)
                .map_err(|error| invalid_field(field, error))?;
            if let Some(step_id) = step_id {
                let (relation, observed_step) = references.check(position, &step_id, true)
                    .map_err(|error| reference_error(error, &raw_step.id, &step_id))?;
                check_observation_protocol(observed_step, &selector, source)
                    .map_err(|error| invalid_field(field, error))?;
                if relation == Ordering::Less {
                    return Ok(ObservationSelector::Step { step_id, selector: Box::new(selector) });
                }
            } else {
                check_observation_protocol(raw_step, &selector, source)
                    .map_err(|error| invalid_field(field, error))?;
            }
            Ok(selector)
        };

        let select = raw_oracle.select.as_deref()
            .map(|source| parse_selector("select", source))
            .transpose()?;
        let when_condition = raw_oracle.r#when.as_ref().map(|when| {
            Ok::<_, ResolutionError>(WhenCondition {
                select: parse_selector("when.select", &when.select)?,
                equals: when.equals.clone(),
            })
        }).transpose()?;
        let expect = raw_oracle.expect.as_ref().map(resolve_expect).transpose()
            .map_err(|error| invalid_field("expect", error))?;
        let record_reference = raw_oracle.record_reference.as_deref()
            .map(|source| local_observation_selector(raw_step, source)
                .map_err(|error| invalid_field("record_reference", error)))
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
    "edge_cases", "schema_valid", "unconstrained", "schema", "alpha", "alphanum", "alphanumeric", "raw", "num",
];
/// `use: http` adds `values` (only the listed values) and `absent`.
const HTTP_MUTATOR_MODES: &[&str] = &[
    "edge_cases", "schema_valid", "unconstrained", "schema", "alpha", "alphanum", "alphanumeric", "raw", "num",
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
        if mutator == "body" {
            use crate::mutation::text::Alphabet;
            if !operation.request_body.as_ref().is_some_and(|body| body.content_type == "application/json") {
                return Err(invalid("use: body requires an application/json request body".into()));
            }
            let alphabet = match raw.mode.as_deref().unwrap_or("raw") {
                "alpha" => Alphabet::Alpha,
                "alphanum" => Alphabet::Alphanum,
                "num" => Alphabet::Num,
                _ => Alphabet::Raw,
            };
            result.push(MutatorDecl::Body { alphabet, max_edits: raw.edits.map_or(4, |edits| edits.max) });
            continue;
        }
        if raw.repetitions.is_some() || raw.seed.is_some() {
            return Err(invalid("repetitions and seed are corpus options".into()));
        }
        if raw.path.is_some() && !matches!(mutator, "json" | "http") {
            return Err(invalid(format!("path is not an option of use: {mutator}")));
        }
        if raw.path.is_some() && (raw.mode.is_some() || raw.length.is_some()) {
            return Err(invalid("path takes no mode or length: each line of the file is a value".into()));
        }
        // `path`: one case per line, any UTF-8 (NUL included: JSON escapes it,
        // a path segment encodes it, a header leaves out what it cannot carry).
        let lines = match raw.path.as_deref() {
            None => None,
            Some("") => return Err(invalid("path must not be empty".into())),
            Some(path) => {
                let (lines, sha256) = crate::mutation::text::load_value_lines(&experiment_dir.join(path), true)
                    .map_err(|error| invalid(format!("path '{path}': {error}")))?;
                Some(crate::experiment::types::ValueLines { path: path.to_owned(), sha256, lines })
            }
        };
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
                || !matches!(raw.mode.as_deref(), Some("alpha" | "alphanum" | "alphanumeric" | "raw" | "num")))
        {
            let found = match raw.mode.as_deref() {
                Some(mode) => format!("use: {mutator}, mode: {mode}"),
                None => format!("use: {mutator} without mode"),
            };
            return Err(invalid(format!(
                "length requires use: json or http with mode: alpha, alphanum, num or raw (found {found})"
            )));
        }
        let decl = match raw.r#use.as_str() {
            "json" => {
                let default = if lines.is_some() { "file" } else { "edge_cases" };
                let mode = match raw.mode.as_deref().unwrap_or(default) {
                    "file" => JsonMutatorMode::Lines,
                    "edge_cases" | "schema_valid" => JsonMutatorMode::EdgeCases,
                    "unconstrained" => JsonMutatorMode::Unconstrained,
                    "schema" => JsonMutatorMode::Schema,
                    "alphanum" | "alphanumeric" if raw.length.is_none() => JsonMutatorMode::Alphanumeric,
                    "alpha" | "alphanum" | "alphanumeric" | "raw" | "num" => {
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
                    lines,
                }
            }
            "http" | "headers" => resolve_http_mutator(raw, operation, lines).map_err(invalid)?,
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
    lines: Option<crate::experiment::types::ValueLines>,
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
    let default = match (legacy, &lines) {
        (true, _) => "values",
        (false, Some(_)) => "file",
        (false, None) => "edge_cases",
    };
    let mode = match raw.mode.as_deref().unwrap_or(default) {
        "file" => JsonMutatorMode::Lines,
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
    Ok(MutatorDecl::Http { fields, mode, values, field_schemas, lines })
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
    // Up to 16 MiB: past the limits of any server, short of exhausting memory.
    const MAX_SIZE: usize = 16 * 1024 * 1024;
    let mut sizes = crate::adapters::http_raw::MalformSizes::default();
    if let Some(raw_sizes) = raw.sizes {
        for (name, value, slot) in [
            ("uri", raw_sizes.uri, &mut sizes.uri),
            ("header", raw_sizes.header, &mut sizes.header),
            ("headers", raw_sizes.headers, &mut sizes.headers),
        ] {
            if let Some(value) = value {
                if value == 0 || value > MAX_SIZE {
                    return Err(format!("malform sizes.{name} must be between 1 and {MAX_SIZE}"));
                }
                *slot = value;
            }
        }
    }
    Ok(MutatorDecl::Malform { malformations, sizes })
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
        Some("num") => Alphabet::Num,
        Some(_) => return Err("text corpus mode must be alpha, alphanum, num or raw".into()),
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

fn reference_error(error: StepReferenceError, step: &str, referenced: &str) -> ResolutionError {
    let step_id = step.to_owned();
    let referenced_step = referenced.to_owned();
    match error {
        StepReferenceError::Unknown => ResolutionError::UnknownStepReference { step_id, referenced_step },
        StepReferenceError::Forward => ResolutionError::ForwardReference { step_id, referenced_step },
    }
}

fn validate_reference(
    step_id: &str,
    reference: &Reference,
    references: &StepReferences<'_>,
    position: usize,
) -> Result<(), ResolutionError> {
    let Reference::StepOutput { step_id: referenced, output_name } = reference else {
        return Ok(());
    };
    references.check(position, referenced, false)
        .map_err(|error| reference_error(error, step_id, referenced))?;
    if !references.declares_output(referenced, output_name) {
        return Err(ResolutionError::MissingOutput {
            step_id: step_id.to_owned(),
            ref_step: referenced.clone(),
            output_name: output_name.clone(),
        });
    }
    Ok(())
}

fn resolve_cli_step(
    raw_step: &RawStep,
    protocol_kind: ProtocolKind,
    resolved_steps: &mut Vec<Step>,
    references: &StepReferences<'_>,
    position: usize,
    experiment_dir: &Path,
) -> Result<(), ResolutionError> {
    let invalid = |detail| ResolutionError::UnsupportedConstruct {
        step_id: raw_step.id.clone(),
        detail,
    };

    CliCommand::parse(raw_step.command.as_deref()).map_err(|error| invalid(error.to_string()))?;
    // Only the YAML `env` may hold references; `env_file` values are literal.
    let env_config = raw_step.env.clone();
    // `env` values with `${env.NAME}`, `${case.id}` or `${target.port}` become
    // bindings: read from Nadir's environment once, resolved for each case,
    // and covered by `hidden` like any other reference.
    let mut bindings = Vec::new();
    let mut names: Vec<&String> = env_config.iter().flatten().map(|(name, _)| name).collect();
    names.sort();
    for name in names {
        let text = &env_config.as_ref().expect("names come from env")[name];
        let value = match selectors::parse_binding_value(&Value::String(text.clone())) {
            selectors::BindingParseResult::Error(error) => {
                return Err(invalid(format!("env.{name}: {error}")));
            }
            selectors::BindingParseResult::Reference(reference) => BindingValue::Reference(reference),
            selectors::BindingParseResult::Composite(parts) => BindingValue::Composite(parts),
            _ => continue,
        };
        if let Some(reference) = value.references()
            .find(|reference| matches!(reference, Reference::StepOutput { .. }))
        {
            return Err(invalid(format!(
                "env.{name}: {reference} is not available in a cli step; use ${{env.NAME}}, ${{case.id}} or ${{target.port}}"
            )));
        }
        bindings.push(Binding { selector: selectors::InputSelector::EnvVar(name.clone()), value });
    }

    let format = crate::formats::PayloadFormat::Json; // not used by CLI

    // The base environment: `env_file`, then the YAML `env` on top.
    let yaml_env = raw_step.env.clone().unwrap_or_default();
    let mut merged: HashMap<String, String> = HashMap::new();
    let mut from_env_file = Vec::new();
    if let Some(file) = &raw_step.env_file {
        for (name, value) in crate::adapters::cli::envfile::load(&experiment_dir.join(file)).map_err(&invalid)? {
            if !yaml_env.contains_key(&name) {
                from_env_file.push(name.clone());
            }
            merged.insert(name, value);
        }
    }
    merged.extend(yaml_env.clone());
    let workdir_copy = match &raw_step.workdir {
        None => None,
        Some(workdir) => {
            let mut copies = Vec::new();
            for entry in &workdir.copy {
                let (from, to) = match entry {
                    crate::config::RawCopy::Same(path) => (path.clone(), path.clone()),
                    crate::config::RawCopy::Renamed { from, to } => (from.clone(), to.clone()),
                };
                if !experiment_dir.join(&from).is_file() {
                    return Err(invalid(format!("workdir.copy: '{from}' is not a file next to the experiment")));
                }
                // The destination stays inside the temporary directory.
                let inside = !to.is_empty()
                    && Path::new(&to).components().all(|part| matches!(part, std::path::Component::Normal(_)));
                if !inside {
                    return Err(invalid(format!("workdir.copy: '{to}' must be a relative path without '..'")));
                }
                copies.push((from, to));
            }
            Some(copies)
        }
    };
    let service = match &raw_step.service {
        None => None,
        Some(service) => {
            let url = url::Url::parse(&service.ready.url)
                .map_err(|error| invalid(format!("service.ready.url: {error}")))?;
            crate::adapters::http::transport::validate_loopback(&url)
                .map_err(|error| invalid(format!("service.ready.url: {error}")))?;
            if service.ready.timeout_ms == 0 {
                return Err(invalid("service.ready.timeout_ms must be at least 1".into()));
            }
            Some(crate::experiment::types::ServiceConfig {
                ready_url: url,
                ready_timeout: std::time::Duration::from_millis(service.ready.timeout_ms),
            })
        }
    };

    let outputs = resolve_outputs(raw_step)?;
    let oracles = resolve_oracles(raw_step, references, position)?;

    // Resolve mutators
    let (mutators, corpora) = resolve_mutators_cli(raw_step, experiment_dir, &merged)?;

    resolved_steps.push(Step {
        id: raw_step.id.clone(),
        protocol: protocol_kind,
        format,
        operation_key: raw_step.id.clone(),
        body_example: None,
        bindings,
        auth: None,
        outputs,
        mutators,
        corpora,
        key_mutators: Vec::new(),
        oracles,
        examples: Vec::new(),
        declared_statuses: Vec::new(),
        print: raw_step.print.unwrap_or(false),
        debug: resolve_debug(raw_step)?,
        command: raw_step.command.clone(),
        env_config: if env_config.is_some() || raw_step.env_file.is_some() { Some(merged) } else { None },
        cli: Some(crate::experiment::types::CliSetup {
            dir: experiment_dir.to_path_buf(),
            from_env_file,
            env_file: raw_step.env_file.clone(),
            workdir_copy,
            service,
        }),
    });

    Ok(())
}

fn resolve_mutators_cli(
    raw_step: &RawStep,
    experiment_dir: &Path,
    base_env: &HashMap<String, String>,
) -> Result<(Vec<MutatorDecl>, Vec<crate::mutation::text::Corpus>), ResolutionError> {
    let invalid = |detail| ResolutionError::UnsupportedConstruct {
        step_id: raw_step.id.clone(),
        detail,
    };
    let Some(cases) = &raw_step.cases else {
        return Ok((Vec::new(), Vec::new()));
    };
    // `variables: all` / `spec` take what the other mutators leave, so they
    // are expanded after the explicit ones.
    let mut mutators: Vec<Option<MutatorDecl>> = Vec::new();
    let mut deferred = Vec::new();
    for raw in &cases.mutators {
        match raw.r#use.as_str() {
            "envar" if raw.mode.as_deref() == Some("spec") => {
                mutators.push(Some(resolve_envar_spec(raw, experiment_dir, base_env).map_err(&invalid)?));
            }
            "envfile" => {
                mutators.push(Some(resolve_envfile(raw_step, raw, experiment_dir).map_err(&invalid)?));
            }
            "envar" if matches!(
                raw.variables,
                Some(crate::config::EnvarVariables::All | crate::config::EnvarVariables::Spec)
            ) => {
                deferred.push((mutators.len(), raw));
                mutators.push(None);
            }
            "envar" => {
                let variables = raw.variables.as_ref().ok_or_else(|| {
                    invalid("'use: envar' requires 'variables'".into())
                })?;
                let resolved = resolve_envar_variables(variables, raw, experiment_dir, &[]).map_err(&invalid)?;
                // With `env_file`, the variables are only known now.
                if raw_step.env_file.is_some() {
                    if let Some(missing) = resolved.iter().find(|variable| !base_env.contains_key(&variable.name)) {
                        return Err(invalid(format!(
                            "envar variable '{}' is not in env_file or env",
                            missing.name
                        )));
                    }
                }
                mutators.push(Some(MutatorDecl::Envar { variables: resolved, skipped: Vec::new() }));
            }
            other => {
                return Err(invalid(format!("unknown mutator 'use: {other}' for protocol 'cli'")));
            }
        }
    }
    // A variable takes one rule per step: two would collide in replay.
    let mut claimed = HashSet::new();
    for mutator in mutators.iter().flatten() {
        if let MutatorDecl::Envar { variables, .. } = mutator {
            for variable in variables {
                if !claimed.insert(variable.name.clone()) {
                    return Err(invalid(format!(
                        "envar variable '{}' appears in more than one mutator of the step",
                        variable.name
                    )));
                }
            }
        }
    }
    for (index, raw) in deferred {
        let (candidates, skipped) = match raw.variables {
            Some(crate::config::EnvarVariables::Spec) => {
                let path = raw.spec.as_deref().ok_or_else(|| invalid("envar variables: spec requires 'spec'".into()))?;
                let spec = load_envar_spec(path, raw.only.as_deref(), experiment_dir).map_err(&invalid)?;
                let (used, skipped) = crate::adapters::cli::spec::applicable(&spec, base_env);
                (used.into_iter().map(|(name, _)| name.clone()).collect::<Vec<_>>(), skipped)
            }
            _ => {
                let mut names: Vec<String> = base_env.keys().cloned().collect();
                names.sort();
                (names, Vec::new())
            }
        };
        let names: Vec<String> = candidates.into_iter().filter(|name| !claimed.contains(name)).collect();
        if names.is_empty() {
            return Err(invalid("envar variables: all/spec leaves no variable (the other mutators vary them all)".into()));
        }
        claimed.extend(names.iter().cloned());
        let variables = raw.variables.as_ref().expect("deferred mutators have variables");
        let resolved = resolve_envar_variables(variables, raw, experiment_dir, &names).map_err(&invalid)?;
        mutators[index] = Some(MutatorDecl::Envar { variables: resolved, skipped });
    }
    Ok((mutators.into_iter().flatten().collect(), Vec::new()))
}

/// `use: envfile`: the file comes from a `workdir.copy` entry; the catalog
/// takes `limits`, `prohibited` and `only`.
fn resolve_envfile(raw_step: &RawStep, raw: &crate::config::RawMutator, experiment_dir: &Path) -> Result<MutatorDecl, String> {
    use crate::adapters::cli::envbreak;
    let file = raw.file.clone().ok_or("use: envfile requires 'file'")?;
    let source = raw_step
        .workdir
        .iter()
        .flat_map(|workdir| &workdir.copy)
        .find_map(|entry| match entry {
            crate::config::RawCopy::Same(path) if *path == file => Some(path.clone()),
            crate::config::RawCopy::Renamed { from, to } if *to == file => Some(from.clone()),
            _ => None,
        })
        .ok_or_else(|| format!("envfile '{file}' must be a destination of workdir.copy"))?;
    let base = std::fs::read(experiment_dir.join(&source)).map_err(|error| format!("envfile source '{source}': {error}"))?;
    if !envbreak::has_assignment(&base) {
        return Err(format!("envfile source '{source}' has no NAME=value line to break"));
    }
    let limits = raw.limits.unwrap_or_default();
    let mut cases = envbreak::catalog(
        envbreak::Limits { file_bytes: limits.file_bytes, value_bytes: limits.value_bytes, name_bytes: limits.name_bytes },
        raw.prohibited.as_deref().unwrap_or_default(),
    );
    if let Some(only) = &raw.only {
        let known = envbreak::known_names(&cases).join(", ");
        if let Some(unknown) = only.iter().find(|name| !cases.iter().any(|case| &case.name == *name)) {
            return Err(format!("envfile only: unknown case '{unknown}' (cases: {known})"));
        }
        cases.retain(|case| only.contains(&case.name));
    }
    Ok(MutatorDecl::EnvFile { file, base, cases })
}

/// A specification file with `only` applied.
fn load_envar_spec(
    path: &str,
    only: Option<&[String]>,
    experiment_dir: &Path,
) -> Result<Vec<(String, crate::adapters::cli::spec::SpecRule)>, String> {
    let mut spec = crate::adapters::cli::spec::load(&experiment_dir.join(path))?;
    if let Some(only) = only {
        for name in only {
            if !spec.iter().any(|(known, _)| known == name) {
                return Err(format!("only: '{name}' is not in {path}"));
            }
        }
        spec.retain(|(name, _)| only.contains(name));
    }
    Ok(spec)
}

/// `use: envar`, `mode: spec`: the specification's variables (or `only`),
/// minus those whose `used_when` the base environment does not satisfy.
fn resolve_envar_spec(
    raw: &crate::config::RawMutator,
    experiment_dir: &Path,
    base_env: &HashMap<String, String>,
) -> Result<MutatorDecl, String> {
    let path = raw.spec.as_deref().ok_or("envar mode: spec requires 'spec'")?;
    if raw.variables.is_some() {
        return Err("envar mode: spec takes 'only', not 'variables'".into());
    }
    let spec = load_envar_spec(path, raw.only.as_deref(), experiment_dir)?;
    let (used, skipped) = crate::adapters::cli::spec::applicable(&spec, base_env);
    let variables = used
        .into_iter()
        .map(|(name, rule)| EnvarVariable { name: name.clone(), rule: EnvarVariableRule::Spec(rule.clone()) })
        .collect();
    Ok(MutatorDecl::Envar { variables, skipped })
}

/// `expanded`: the names `variables: all` / `spec` stand for.
fn resolve_envar_variables(
    variables: &crate::config::EnvarVariables,
    raw: &crate::config::RawMutator,
    experiment_dir: &Path,
    expanded: &[String],
) -> Result<Vec<EnvarVariable>, String> {
    let file = |path: &str| -> Result<EnvarVariableRule, String> {
        let (lines, sha256) = crate::mutation::text::load_value_lines(&experiment_dir.join(path), false)
            .map_err(|error| format!("envar path '{path}': {error}"))?;
        Ok(EnvarVariableRule::File { path: path.to_owned(), sha256, lines })
    };
    match variables {
        crate::config::EnvarVariables::List(_)
        | crate::config::EnvarVariables::All
        | crate::config::EnvarVariables::Spec => {
            let names = match variables {
                crate::config::EnvarVariables::List(names) => names.as_slice(),
                _ => expanded,
            };
            let rule = match (raw.mode.as_deref(), raw.path.as_deref(), &raw.values) {
                (None, Some(path), None) => file(path)?,
                (Some(mode), None, None) => envar_generator(mode, raw.length.as_ref())?,
                (None, None, Some(values)) => EnvarVariableRule::Values(values.iter().map(envar_value_to_string).collect()),
                _ => return Err("list-form envar variables require exactly one of 'mode', 'path' or 'values'".into()),
            };
            Ok(names.iter().map(|name| EnvarVariable { name: name.clone(), rule: rule.clone() }).collect())
        }
        crate::config::EnvarVariables::Map(rules) => {
            let mut resolved = Vec::new();
            for (name, rule) in rules {
                let rule = match (&rule.value, &rule.values, &rule.path) {
                    (Some(value), None, None) => EnvarVariableRule::Fixed(envar_value_to_string(value)),
                    (None, Some(values), None) => {
                        EnvarVariableRule::Values(values.iter().map(envar_value_to_string).collect())
                    }
                    (None, None, Some(path)) => file(path)?,
                    _ => return Err(format!("envar variable '{name}' needs exactly one of 'value', 'values' or 'path'")),
                };
                resolved.push(EnvarVariable { name: name.clone(), rule });
            }
            // The map has no order of its own: keep cases stable for replay.
            resolved.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(resolved)
        }
    }
}

/// A generative `envar` mode: 1 to 64 characters, or the shared length
/// policy with `length: {reference, max}` (there is no schema to read a
/// range from, so `reference` is required).
fn envar_generator(mode: &str, length: Option<&crate::config::RawLength>) -> Result<EnvarVariableRule, String> {
    use crate::mutation::text::{Alphabet, TextGenerator, MAX_TEXT_CHARS};
    let (mode, alphabet) = match mode {
        "alpha" => (EnvarMode::Alpha, Alphabet::Alpha),
        "alphanum" => (EnvarMode::Alphanum, Alphabet::Alphanum),
        "num" => (EnvarMode::Num, Alphabet::Num),
        "raw" => (EnvarMode::Raw, Alphabet::EnvRaw),
        "absent" | "edge_cases" | "unconstrained" if length.is_some() => {
            return Err(format!("envar mode: {mode} takes no 'length'"));
        }
        "absent" => return Ok(EnvarVariableRule::Absent),
        "edge_cases" => return Ok(EnvarVariableRule::Catalog { unconstrained: false }),
        "unconstrained" => return Ok(EnvarVariableRule::Catalog { unconstrained: true }),
        other => return Err(format!("unknown envar mode '{other}'")),
    };
    let Some(length) = length else {
        return Ok(EnvarVariableRule::Mode(mode));
    };
    let reference = length.reference.ok_or("envar length requires 'reference'")?;
    let max = length.max.unwrap_or(256);
    if reference == 0 || reference >= max || max > MAX_TEXT_CHARS {
        return Err(format!("envar length requires 0 < reference < max <= {MAX_TEXT_CHARS}"));
    }
    Ok(EnvarVariableRule::Text(TextGenerator { alphabet, below: reference, above: reference, max }))
}

/// Preserve leading zeros by treating numbers as their string representation.
fn envar_value_to_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
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

    fn create_record_config() -> RawExperiment {
        serde_yaml::from_str(include_str!("../../examples/create-record/experiment.yaml")).unwrap()
    }

    #[test]
    fn binding_references_validate_order_and_declared_outputs() {
        let contracts = load_contracts();
        for (value, raw_error, resolution_error) in [
            ("${steps.login.outputs.token}", None, None),
            ("prefix-${steps.login.outputs.token}-${case.id}", None, None),
            ("prefix-${steps.missing.outputs.token}", Some("unknown step"), Some("unknown step")),
            ("${steps.get_template.outputs.template_id}", Some("forward reference"), Some("forward reference")),
            ("${steps.post_record.outputs.reference}", Some("forward reference"), Some("forward reference")),
            ("${steps.login.outputs.missing}", None, Some("output 'missing' not declared")),
            ("${steps.login.output.token}", None, Some("invalid reference in binding 'header.Accept-Language'")),
        ] {
            let mut raw = create_record_config();
            raw.experiment.steps[1].bind.as_mut().unwrap()
                .insert("header.Accept-Language".into(), Value::String(value.into()));

            match raw_error {
                Some(expected) => {
                    let error = config::validate_raw_experiment(&raw).unwrap_err();
                    assert!(error.contains(expected), "{value}: {error}");
                }
                None => config::validate_raw_experiment(&raw).unwrap(),
            }
            let resolved = resolve_experiment(raw, &contracts, &examples_dir());
            match resolution_error {
                Some(expected) => {
                    let error = resolved.unwrap_err().to_string();
                    assert!(error.contains(expected), "{value}: {error}");
                }
                None => { resolved.unwrap(); }
            }
        }
    }

    #[test]
    fn bearer_cannot_reference_a_later_step() {
        let mut raw = create_record_config();
        raw.experiment.steps[1].auth.as_mut().unwrap().bearer =
            "${steps.post_record.outputs.reference}".into();
        assert!(config::validate_raw_experiment(&raw).unwrap_err().contains("forward reference"));
        assert!(matches!(
            resolve_experiment(raw, &load_contracts(), &examples_dir()),
            Err(ResolutionError::ForwardReference { step_id, referenced_step })
                if step_id == "get_template" && referenced_step == "post_record"
        ));
    }

    #[test]
    fn oracle_selectors_can_observe_the_current_or_an_earlier_step() {
        let mut raw = create_record_config();
        let oracle = &mut raw.experiment.steps[1].oracles[1];
        oracle.select = Some("steps.get_template.response.status".into());
        oracle.r#when = Some(config::RawWhenBlock {
            select: "steps.login.response.status".into(),
            equals: Value::from(200),
        });
        config::validate_raw_experiment(&raw).unwrap();
        let resolved = resolve_experiment(raw, &load_contracts(), &examples_dir()).unwrap();
        let oracle = &resolved.steps[1].oracles[1];
        assert!(matches!(oracle.select, Some(ObservationSelector::Status)));
        assert!(matches!(
            oracle.when_condition.as_ref().map(|when| &when.select),
            Some(ObservationSelector::Step { step_id, selector })
                if step_id == "login" && matches!(selector.as_ref(), ObservationSelector::Status)
        ));
    }

    #[test]
    fn oracle_selectors_reject_future_and_unknown_steps() {
        let contracts = load_contracts();
        for step in ["missing", "post_record"] {
            let mut raw = create_record_config();
            raw.experiment.steps[1].oracles[1].r#when = Some(config::RawWhenBlock {
                select: format!("steps.{step}.response.status"),
                equals: Value::from(200),
            });
            let validation = config::validate_raw_experiment(&raw);
            let error = resolve_experiment(raw, &contracts, &examples_dir()).unwrap_err();
            if step == "missing" {
                assert!(validation.unwrap_err().contains("references unknown step 'missing'"));
                assert!(matches!(
                    error,
                    ResolutionError::UnknownStepReference { referenced_step, .. }
                        if referenced_step == step
                ));
            } else {
                validation.unwrap();
                assert!(matches!(
                    error,
                    ResolutionError::ForwardReference { referenced_step, .. }
                        if referenced_step == step
                ));
            }
        }
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
            protocols: config::RawProtocols { http: None, cli: None },
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
            command: None,
            env: None,
            env_file: None,
            workdir: None,
            service: None,
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
                    max_path_chars: None,
                }),
                cli: None,
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
            command: None,
            env: None,
            env_file: None,
            workdir: None,
            service: None,
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
