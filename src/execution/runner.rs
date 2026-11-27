//! Workflow execution: step sequencing, outcome tracking, case context.
//!
//! # Design notes
//!
//! **Chosen:** `WorkflowRunner` holds shared run-scope resources (HTTP client,
//! process monitor). Each case execution creates a fresh
//! `CaseContext` with its own case_id and scratch storage. Step outputs from
//! one case never leak into another.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use url::Url;
use uuid::Uuid;

use crate::adapters::http::contract::{ResolvedContract, ResolvedOperation};
use crate::adapters::http::request;
use crate::adapters::http::transport::{
    FailurePhase, HttpClient, HttpObservation, TransportError, TransportReason,
};
use crate::evaluation::schema_validator::SchemaValidatorCache;
use crate::experiment::selectors::Reference;
use crate::experiment::types::{Binding, BindingValue, CompositePart, Experiment, Step};
use crate::target::ProcessMonitor;

// ── Types ──────────────────────────────────────────────────────────

struct CaseContext<'a> {
    case_id: String,
    runtime: &'a RuntimeInputs,
}

struct RuntimeInputs {
    environment: HashMap<String, String>,
    target_port: Option<u16>,
}

struct PublishedOutput {
    value: Value,
    sensitive: bool,
}

type StepOutputs = HashMap<String, HashMap<String, PublishedOutput>>;

/// Result of executing one full case (all steps in declaration order).
#[derive(Debug)]
pub struct CaseResult {
    pub case_id: String,
    pub step_results: Vec<StepResult>,
    pub was_executed: bool,
    pub requests_sent: u32,
}

/// Result of executing a single step.
#[derive(Debug)]
pub struct StepResult {
    pub step_id: String,
    pub outcome: StepOutcome,
    pub observation: Option<HttpObservation>,
    pub extracted_outputs: HashMap<String, Value>,
    /// Suppress response data in evidence when a binding was derived from a secret.
    pub sensitive_response: bool,
}

/// Outcome of a single step execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepOutcome {
    /// Step completed normally.
    Completed,
    /// Step was skipped because a required output was missing.
    Skipped { reason: String },
    /// Step failed due to transport error.
    TransportError {
        error: String,
        /// Classification: "timeout", "reset", "connection_failed", etc.
        transport_reason: Option<TransportReason>,
        /// Stage reached before the transport failed.
        transport_phase: Option<FailurePhase>,
    },
    /// Step failed due to input building error.
    InputError { error: String },
    /// Step failed due to extraction error.
    ExtractionError { error: String },
}

/// Errors that abort the run (infrastructure).
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("transport error: {0}")]
    Transport(#[from] TransportError),
    #[error("input build error: {0}")]
    InputBuild(#[from] request::InputBuildError),
    #[error("missing contract for adapter")]
    MissingContract,
    #[error("infrastructure error: {0}")]
    Infrastructure(String),
}

// ── WorkflowRunner ─────────────────────────────────────────────────

/// A workflow runner has already resolved its runtime configuration before contacting the target.
pub struct WorkflowRunner {
    experiment: Arc<Experiment>,
    contracts: Arc<HashMap<String, ResolvedContract>>,
    http: HttpClient,
    runtime: RuntimeInputs,
    diagnostic_secrets: Vec<String>,
    monitor: Option<Box<dyn ProcessMonitor>>,
    pub(crate) schema_cache: Arc<SchemaValidatorCache>,
}

#[derive(Debug, thiserror::Error)]
pub enum PrepareError {
    #[error(transparent)]
    Schema(#[from] crate::evaluation::schema_validator::SchemaPreparationError),
    #[error("target.coverage is unavailable in this release")]
    CoverageUnavailable,
    #[error("an HTTP adapter is required")]
    NoHttpAdapter,
    #[error("invalid HTTP base URL")]
    InvalidBaseUrl,
    #[error("HTTP client requires a valid loopback base URL")]
    HttpClient(#[source] TransportError),
    #[error(
        "step '{step_id}': missing or invalid environment variable '{name}' \
         (set it, for example `export {name}=...`, or give a default with ${{env.{name}:-value}})"
    )]
    Environment { step_id: String, name: String },
    #[error("step '{step_id}': cannot determine the port for ${{target.port}}")]
    TargetPort { step_id: String },
}

impl WorkflowRunner {
    /// Prepare a reusable runner and capture the environment needed by its workflow.
    pub fn new(
        experiment: Arc<Experiment>,
        contracts: Arc<HashMap<String, ResolvedContract>>,
    ) -> Result<Self, PrepareError> {
        let schema_cache =
            crate::evaluation::schema_validator::build_schema_cache(&experiment, &contracts)?;
        Self::prepare(experiment, contracts, Arc::new(schema_cache))
    }

    pub(crate) fn from_resolved(
        configuration: &crate::config::ResolvedExperiment,
    ) -> Result<Self, PrepareError> {
        Self::prepare(
            configuration.experiment.clone(),
            configuration.contracts.clone(),
            configuration.schema_cache.clone(),
        )
    }

    fn prepare(
        experiment: Arc<Experiment>,
        contracts: Arc<HashMap<String, ResolvedContract>>,
        schema_cache: Arc<SchemaValidatorCache>,
    ) -> Result<Self, PrepareError> {
        if experiment
            .target
            .as_ref()
            .is_some_and(|target| target.coverage.is_some())
        {
            return Err(PrepareError::CoverageUnavailable);
        }
        let adapter = experiment
            .adapters
            .http
            .as_ref()
            .ok_or(PrepareError::NoHttpAdapter)?;
        let base_url = Url::parse(&adapter.base_url).map_err(|_| PrepareError::InvalidBaseUrl)?;
        let environment: HashMap<_, _> = std::env::vars_os().collect();
        let runtime =
            RuntimeInputs::prepare(&experiment, base_url.port_or_known_default(), |name| {
                let value = environment
                    .get(std::ffi::OsStr::new(name))
                    .ok_or(std::env::VarError::NotPresent)?;
                value
                    .to_str()
                    .map(str::to_owned)
                    .ok_or_else(|| std::env::VarError::NotUnicode(value.clone()))
            })?;
        // Preserve environment-value redaction even for values echoed by the service.
        // Defaults used in place of unset variables are redacted the same way.
        let diagnostic_secrets = environment
            .into_values()
            .filter_map(|value| value.into_string().ok())
            .chain(runtime.environment.values().cloned())
            .filter(|value| !value.is_empty())
            .collect();
        let http = HttpClient::new(base_url, Duration::from_millis(adapter.timeout_ms))
            .map_err(PrepareError::HttpClient)?;
        Ok(Self {
            experiment,
            contracts,
            http,
            runtime,
            diagnostic_secrets,
            monitor: None,
            schema_cache,
        })
    }

    pub fn experiment(&self) -> &Experiment {
        &self.experiment
    }

    pub fn set_monitor(&mut self, monitor: Option<Box<dyn ProcessMonitor>>) {
        self.monitor = monitor;
    }

    pub(crate) fn process_status(&mut self) -> Option<crate::target::ProcessStatus> {
        self.monitor.as_mut().map(|monitor| monitor.status())
    }

    pub(crate) fn http_contract(&self) -> Option<&ResolvedContract> {
        self.experiment
            .adapters
            .http
            .as_ref()
            .and_then(|adapter| self.contracts.get(&adapter.openapi_path))
    }

    pub(crate) fn diagnostic_secrets(&self) -> &[String] {
        &self.diagnostic_secrets
    }

    /// Execute the baseline case (no mutations applied).
    pub fn execute_baseline(&mut self) -> Result<CaseResult, RunError> {
        self.execute_with_scope(&[], None)
    }

    /// Execute a case with a set of replacements.
    pub fn execute_case(
        &mut self,
        plan: &[(String, request::ReplacementValue)],
    ) -> Result<CaseResult, RunError> {
        self.execute_with_scope(plan, None)
    }

    /// Execute a case that replaces several fields of one step.
    pub(crate) fn execute_replacements(
        &mut self,
        step_id: &str,
        replacements: &[(String, crate::experiment::types::ReplacementValue)],
    ) -> Result<CaseResult, RunError> {
        self.execute_with_scope(replacements, Some(step_id))
    }

    pub(crate) fn execute_variant(
        &mut self,
        step_id: &str,
        selector: &str,
        value: crate::experiment::types::ReplacementValue,
    ) -> Result<CaseResult, RunError> {
        self.execute_with_scope(&[(selector.to_owned(), value)], Some(step_id))
    }

    fn execute_with_scope(
        &mut self,
        replacements: &[(String, request::ReplacementValue)],
        step_id: Option<&str>,
    ) -> Result<CaseResult, RunError> {
        let ctx = CaseContext {
            case_id: Uuid::new_v4().to_string(),
            runtime: &self.runtime,
        };
        let variant_step = step_id;
        let requests_before = self.http.requests_sent();
        let mut step_results = Vec::new();
        let mut step_outputs = StepOutputs::new();

        for step in &self.experiment.steps {
            // Check if required outputs from previous steps are available
            if let Some(reason) = check_dependencies(step, &step_outputs) {
                step_results.push(StepResult {
                    step_id: step.id.clone(),
                    outcome: StepOutcome::Skipped { reason },
                    observation: None,
                    extracted_outputs: HashMap::new(),
                    sensitive_response: false,
                });
                continue;
            }

            // Get the operation from the contract
            let operation = self
                .http_contract()
                .and_then(|contract| contract.operations.get(&step.operation_id))
                .ok_or(RunError::MissingContract)?;

            // Build and execute the request
            let step_replacements = if variant_step.is_none_or(|id| id == step.id) {
                replacements
            } else {
                &[]
            };
            let result = execute_step(
                step,
                operation,
                &self.http,
                &ctx,
                step_replacements,
                &step_outputs,
            );

            match result {
                Ok(completed) => {
                    let published = step
                        .outputs
                        .iter()
                        .filter_map(|declaration| {
                            let value = completed.outputs.get(&declaration.name)?.clone();
                            Some((
                                declaration.name.clone(),
                                PublishedOutput {
                                    value,
                                    sensitive: declaration.secret || completed.sensitive_response,
                                },
                            ))
                        })
                        .collect();
                    step_outputs.insert(step.id.clone(), published);
                    step_results.push(StepResult {
                        step_id: step.id.clone(),
                        outcome: StepOutcome::Completed,
                        observation: Some(completed.observation),
                        extracted_outputs: completed.outputs,
                        sensitive_response: completed.sensitive_response,
                    });
                }
                Err(StepError::Transport(e)) => {
                    let transport_reason = Some(e.reason());
                    let transport_phase = e.phase();
                    step_results.push(StepResult {
                        step_id: step.id.clone(),
                        outcome: StepOutcome::TransportError {
                            error: e.to_string(),
                            transport_reason,
                            transport_phase,
                        },
                        observation: None,
                        extracted_outputs: HashMap::new(),
                        sensitive_response: false,
                    });
                }
                Err(StepError::InputBuild(e)) => {
                    step_results.push(StepResult {
                        step_id: step.id.clone(),
                        outcome: StepOutcome::InputError {
                            error: e.to_string(),
                        },
                        observation: None,
                        extracted_outputs: HashMap::new(),
                        sensitive_response: false,
                    });
                }
            }
        }

        Ok(CaseResult {
            case_id: ctx.case_id,
            step_results,
            was_executed: true,
            requests_sent: self.http.requests_sent().wrapping_sub(requests_before),
        })
    }
}

// ── Step execution ─────────────────────────────────────────────────

enum StepError {
    Transport(TransportError),
    InputBuild(request::InputBuildError),
}

struct CompletedStep {
    observation: HttpObservation,
    outputs: HashMap<String, Value>,
    sensitive_response: bool,
}

fn execute_step(
    step: &Step,
    operation: &ResolvedOperation,
    http: &HttpClient,
    ctx: &CaseContext<'_>,
    replacements: &[(String, request::ReplacementValue)],
    step_outputs: &StepOutputs,
) -> Result<CompletedStep, StepError> {
    // Bearer authentication uses its own channel. Ordinary bindings can embed secret data in the request.
    let sensitive_response = step
        .bindings
        .iter()
        .flat_map(|binding| extract_references(&binding.value))
        .any(|reference| match reference {
            Reference::Env { .. } => true,
            Reference::StepOutput {
                step_id,
                output_name,
            } => step_outputs
                .get(&step_id)
                .and_then(|outputs| outputs.get(&output_name))
                .is_some_and(|output| output.sensitive),
            _ => false,
        });
    let mut resolved_bindings = resolve_runtime_bindings(&step.bindings, ctx, step_outputs)
        .map_err(StepError::InputBuild)?;
    if let Some(auth) = &step.auth {
        let token =
            resolve_reference(&auth.bearer, ctx, step_outputs).map_err(StepError::InputBuild)?;
        let token = token
            .as_str()
            .filter(|token| !token.is_empty())
            .ok_or_else(|| {
                StepError::InputBuild(request::InputBuildError::UnresolvedRequiredField {
                    field: "bearer token".into(),
                })
            })?;
        resolved_bindings.push(Binding {
            selector: crate::experiment::selectors::InputSelector::HeaderParam(
                "Authorization".into(),
            ),
            value: BindingValue::Literal(Value::String(format!("Bearer {token}"))),
        });
    }

    let input =
        request::build_step_request(operation, &resolved_bindings, replacements, http.base_url())
            .map_err(StepError::InputBuild)?;
    let observation = http.execute(&input).map_err(StepError::Transport)?;

    // Extract outputs
    let extracted = extract_step_outputs(step, &observation);

    Ok(CompletedStep {
        observation,
        outputs: extracted,
        sensitive_response,
    })
}

/// Resolve runtime references in bindings (env, case_id, step outputs).
fn resolve_runtime_bindings(
    bindings: &[Binding],
    ctx: &CaseContext<'_>,
    step_outputs: &StepOutputs,
) -> Result<Vec<Binding>, request::InputBuildError> {
    bindings
        .iter()
        .map(|binding| {
            let value = match &binding.value {
                BindingValue::Literal(value) => value.clone(),
                BindingValue::Reference(reference) => {
                    resolve_reference(reference, ctx, step_outputs)?
                }
                BindingValue::Composite(parts) => {
                    let mut value = String::new();
                    for part in parts {
                        match part {
                            CompositePart::Literal(text) => value.push_str(text),
                            CompositePart::Reference(reference) => {
                                let resolved = resolve_reference(reference, ctx, step_outputs)?;
                                match resolved {
                                    Value::String(text) => value.push_str(&text),
                                    value_part => value.push_str(&value_part.to_string()),
                                }
                            }
                        }
                    }
                    Value::String(value)
                }
            };
            Ok(Binding {
                selector: binding.selector.clone(),
                value: BindingValue::Literal(value),
            })
        })
        .collect()
}

fn resolve_reference(
    reference: &Reference,
    ctx: &CaseContext<'_>,
    step_outputs: &StepOutputs,
) -> Result<Value, request::InputBuildError> {
    let resolved = match reference {
        Reference::Env { name, .. } => ctx
            .runtime
            .environment
            .get(name)
            .cloned()
            .map(Value::String),
        Reference::CaseId => Some(Value::String(ctx.case_id.clone())),
        Reference::StepOutput {
            step_id,
            output_name,
        } => step_outputs
            .get(step_id)
            .and_then(|outputs| outputs.get(output_name))
            .map(|output| output.value.clone()),
        Reference::TargetPort => ctx
            .runtime
            .target_port
            .map(|port| Value::String(port.to_string())),
    };
    resolved.ok_or_else(|| request::InputBuildError::UnresolvedRequiredField {
        field: reference.to_string(),
    })
}

/// Check if a step's dependencies (outputs from earlier steps) are satisfied.
fn check_dependencies(step: &Step, step_outputs: &StepOutputs) -> Option<String> {
    // Check auth bearer reference
    if let Some(auth) = &step.auth {
        if let crate::experiment::selectors::Reference::StepOutput {
            step_id,
            output_name,
        } = &auth.bearer
        {
            let found = step_outputs.get(step_id).and_then(|o| o.get(output_name));
            if found.is_none() {
                return Some(format!(
                    "required output '{}.{}' not available for auth",
                    step_id, output_name
                ));
            }
        }
    }

    // Check bindings
    for binding in &step.bindings {
        let refs = extract_references(&binding.value);
        for r in refs {
            if let crate::experiment::selectors::Reference::StepOutput {
                step_id,
                output_name,
            } = &r
            {
                let found = step_outputs.get(step_id).and_then(|o| o.get(output_name));
                if found.is_none() {
                    return Some(format!(
                        "required output '{}.{}' not available",
                        step_id, output_name
                    ));
                }
            }
        }
    }

    None
}

fn extract_references(value: &BindingValue) -> Vec<crate::experiment::selectors::Reference> {
    match value {
        BindingValue::Reference(r) => vec![r.clone()],
        BindingValue::Composite(parts) => parts
            .iter()
            .filter_map(|p| match p {
                CompositePart::Reference(r) => Some(r.clone()),
                _ => None,
            })
            .collect(),
        _ => vec![],
    }
}

impl RuntimeInputs {
    fn prepare(
        experiment: &Experiment,
        target_port: Option<u16>,
        read_env: impl Fn(&str) -> Result<String, std::env::VarError>,
    ) -> Result<Self, PrepareError> {
        let mut environment = HashMap::new();
        for step in &experiment.steps {
            let references = step
                .bindings
                .iter()
                .flat_map(|binding| extract_references(&binding.value))
                .chain(step.auth.iter().map(|auth| auth.bearer.clone()));
            for reference in references {
                match reference {
                    Reference::Env { name, default } => {
                        if let std::collections::hash_map::Entry::Vacant(entry) =
                            environment.entry(name)
                        {
                            let value = match (read_env(entry.key()), default) {
                                (Ok(value), Some(default)) if value.is_empty() => default,
                                (Ok(value), _) => value,
                                (Err(std::env::VarError::NotPresent), Some(default)) => default,
                                (Err(_), _) => {
                                    return Err(PrepareError::Environment {
                                        step_id: step.id.clone(),
                                        name: entry.key().clone(),
                                    })
                                }
                            };
                            entry.insert(value);
                        }
                    }
                    Reference::TargetPort if target_port.is_none() => {
                        return Err(PrepareError::TargetPort {
                            step_id: step.id.clone(),
                        });
                    }
                    _ => {}
                }
            }
            if let Some(Reference::Env { name, .. }) = step.auth.as_ref().map(|auth| &auth.bearer) {
                if environment.get(name).is_none_or(|value| {
                    value.is_empty()
                        || http::HeaderValue::from_str(&format!("Bearer {value}")).is_err()
                }) {
                    return Err(PrepareError::Environment {
                        step_id: step.id.clone(),
                        name: name.clone(),
                    });
                }
            }
        }
        Ok(Self {
            environment,
            target_port,
        })
    }
}

/// Extract declared outputs from an HTTP observation.
fn extract_step_outputs(step: &Step, observation: &HttpObservation) -> HashMap<String, Value> {
    let mut outputs = HashMap::new();

    for output_decl in &step.outputs {
        // Check when_status
        if let Some(ref statuses) = output_decl.when_status {
            if !statuses.contains(&observation.status) {
                continue; // status doesn't match — output is absent (not an error)
            }
        }

        if let crate::observation::SelectedValue::Present(value) =
            crate::observation::select_response(&output_decl.from, observation)
        {
            outputs.insert(output_decl.name.clone(), value);
        }
    }
    outputs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::experiment::selectors::ObservationSelector;
    use crate::observation::{select_response, ObservationError, SelectedValue};

    fn select_body(body: &[u8], path: &[&str]) -> SelectedValue {
        let response = HttpObservation {
            status: 200,
            headers: Vec::new(),
            body: body.to_vec(),
            truncated: false,
        };
        let selector = ObservationSelector::Body(path.iter().map(|part| (*part).into()).collect());
        select_response(&selector, &response)
    }

    #[test]
    fn extracts_scalar_and_nested_fields() {
        assert_eq!(
            select_body(br#"{"user":"test"}"#, &["user"]),
            SelectedValue::Present(Value::String("test".into()))
        );
        assert_eq!(
            select_body(
                br#"{"metadata":{"source":"nadir"}}"#,
                &["metadata", "source"]
            ),
            SelectedValue::Present(Value::String("nadir".into()))
        );
    }

    #[test]
    fn distinguishes_missing_null_and_invalid_json() {
        let body = br#"{"present":null}"#;
        assert_eq!(select_body(body, &["missing"]), SelectedValue::Missing);
        assert_eq!(
            select_body(body, &["present"]),
            SelectedValue::Present(Value::Null)
        );
        assert_eq!(
            select_body(b"not json", &["field"]),
            SelectedValue::Unavailable(ObservationError::InvalidJson)
        );
    }

    #[test]
    fn prepares_default_port_and_reads_each_named_variable_once() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("examples/create-record/experiment.yaml");
        let configuration = crate::config::load_resolved(&path).unwrap();
        let mut experiment = configuration.experiment.as_ref().clone();
        let login = &mut experiment.steps[0];
        login.bindings.push(Binding {
            selector: crate::experiment::selectors::InputSelector::PathParam("port".into()),
            value: BindingValue::Reference(Reference::TargetPort),
        });
        login.bindings.push(Binding {
            selector: crate::experiment::selectors::InputSelector::BodyField(vec!["repeat".into()]),
            value: BindingValue::Reference(Reference::Env {
                name: "NADIR_FIXTURE_PASSWORD".into(),
                default: None,
            }),
        });
        let requests = std::cell::Cell::new(0);
        let port = Url::parse("http://127.0.0.1")
            .unwrap()
            .port_or_known_default();
        let runtime = RuntimeInputs::prepare(&experiment, port, |_| {
            requests.set(requests.get() + 1);
            Ok("captured-fixture-value".into())
        })
        .unwrap();
        let context = CaseContext {
            case_id: "test".into(),
            runtime: &runtime,
        };
        assert_eq!(requests.get(), 1);
        assert_eq!(
            resolve_reference(&Reference::TargetPort, &context, &HashMap::new()).unwrap(),
            "80"
        );

        let error = RuntimeInputs::prepare(&experiment, None, |_| Ok("fixture-value".into()));
        assert!(matches!(error, Err(PrepareError::TargetPort { step_id }) if step_id == "login"));
    }
}
