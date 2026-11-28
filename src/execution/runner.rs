//! Workflow execution: step sequencing, outcome tracking, case context.
//!
//! # Design notes
//!
//! **Chosen:** `WorkflowRunner` holds shared run-scope resources (protocols and
//! process monitor). Each case execution creates a fresh
//! `CaseContext` with its own case_id and scratch storage. Step outputs from
//! one case never leak into another.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;
use uuid::Uuid;

use crate::adapters::http::contract::ResolvedContract;
use crate::adapters::http::execution::HttpStepProtocol;
use crate::adapters::http::transport::{FailurePhase, TransportReason};
use crate::adapters::{AdapterExecution, Inspection, StepProtocol, StepInput};
use crate::inspect::{RequestInspector, RequestTemplate};
use crate::evaluation::schema_validator::SchemaValidatorCache;
use crate::experiment::selectors::Reference;
use crate::experiment::types::{
    ProtocolKind, Binding, BindingValue, CompositePart, ExampleAction, ExampleCase, Experiment, JsonExample, ReplacementValue, Step,
};
use crate::observation::StepObservation;
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
    pub observation: Option<StepObservation>,
    pub request_diagnostic: Option<crate::execution::diagnostic::RequestDiagnostic>,
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
    Transport(#[from] crate::adapters::http::transport::TransportError),
    #[error("input build error: {0}")]
    InputBuild(#[from] crate::adapters::http::request::InputBuildError),
    #[error("missing contract for adapter")]
    MissingContract,
    #[error("infrastructure error: {0}")]
    Infrastructure(String),
    /// The user stopped the run from the `debug` display.
    #[error("stopped by the user")]
    StoppedByUser,
}

// ── WorkflowRunner ─────────────────────────────────────────────────

/// A workflow runner has already resolved its runtime configuration before contacting the target.
pub struct WorkflowRunner {
    experiment: Arc<Experiment>,
    protocols: HashMap<ProtocolKind, Box<dyn StepProtocol>>,
    runtime: RuntimeInputs,
    /// Values of the `hidden` variables, shown as `[hidden]`.
    hidden_values: Vec<String>,
    /// `hidden` variables too short to hide.
    short_hidden: Vec<String>,
    monitor: Option<Box<dyn ProcessMonitor>>,
    pub(crate) schema_cache: Arc<SchemaValidatorCache>,
    inspector: Option<Box<dyn RequestInspector>>,
}

#[derive(Debug, thiserror::Error)]
pub enum PrepareError {
    #[error(transparent)]
    Schema(#[from] crate::evaluation::schema_validator::SchemaPreparationError),
    #[error("target.coverage is unavailable in this release")]
    CoverageUnavailable,
    #[error("an HTTP protocol configuration is required")]
    NoHttpProtocol,
    #[error("invalid HTTP base URL")]
    InvalidBaseUrl,
    #[error("HTTP client requires a valid loopback base URL")]
    HttpClient(#[source] crate::adapters::http::transport::TransportError),
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
        if experiment.target.as_ref().is_some_and(|target| target.coverage.is_some()) {
            return Err(PrepareError::CoverageUnavailable);
        }
        // HTTP adapters need `protocols.http`; an experiment of cli steps has none.
        let mut protocols: HashMap<ProtocolKind, Box<dyn StepProtocol>> = HashMap::new();
        let mut target_port = None;
        let uses_http = experiment
            .steps
            .iter()
            .any(|step| matches!(step.protocol, ProtocolKind::Http | ProtocolKind::HttpRaw));
        if experiment.protocols.http.is_some() || uses_http {
            let http = HttpStepProtocol::prepare(&experiment, contracts.clone())?;
            target_port = http.target_port();
            let raw = crate::adapters::http_raw::HttpRawStepProtocol::prepare(&experiment, contracts)?;
            protocols.insert(ProtocolKind::Http, Box::new(http));
            protocols.insert(ProtocolKind::HttpRaw, Box::new(raw));
        }
        // `protocols.cli` is optional: its timeout defaults to 5000 ms.
        if experiment.protocols.cli.is_some()
            || experiment.steps.iter().any(|step| step.protocol == ProtocolKind::Cli)
        {
            let cli = crate::adapters::cli::CliStepProtocol::prepare(
                experiment.protocols.cli.as_ref().map_or(5000, |c| c.timeout_ms),
            );
            protocols.insert(ProtocolKind::Cli, Box::new(cli));
        }
        Self::with_protocols(experiment, protocols, target_port, schema_cache)
    }

    fn with_protocols(
        experiment: Arc<Experiment>,
        protocols: HashMap<ProtocolKind, Box<dyn StepProtocol>>,
        target_port: Option<u16>,
        schema_cache: Arc<SchemaValidatorCache>,
    ) -> Result<Self, PrepareError> {
        let environment: HashMap<_, _> = std::env::vars_os().collect();
        let runtime =
            RuntimeInputs::prepare(&experiment, target_port, |name| {
                let value = environment
                    .get(std::ffi::OsStr::new(name))
                    .ok_or(std::env::VarError::NotPresent)?;
                value
                    .to_str()
                    .map(str::to_owned)
                    .ok_or_else(|| std::env::VarError::NotUnicode(value.clone()))
            })?;
        // Only the variables listed in `hidden` are hidden, by their effective
        // value: the one a step read, or for target-only variables the
        // environment value or the reference's default.
        let references = experiment.env_references();
        let (mut hidden_values, mut short_hidden) = (Vec::new(), Vec::new());
        for name in &experiment.hidden {
            // A cli step's own value (from `env_file` or a literal `env`).
            let cli_value = experiment
                .steps
                .iter()
                .filter_map(|step| step.env_config.as_ref()?.get(name))
                .find(|value| !value.contains("${"))
                .cloned();
            let value = runtime.environment.get(name).cloned().or(cli_value).or_else(|| {
                let set = environment
                    .get(std::ffi::OsStr::new(name))
                    .and_then(|value| value.to_str())
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned);
                set.or_else(|| references.iter().find(|(known, _)| known == name)?.1.clone())
            });
            match value {
                Some(value) if value.chars().count() >= crate::inspect::MIN_HIDDEN_LEN => hidden_values.push(value),
                _ => short_hidden.push(name.clone()),
            }
        }
        for step in &experiment.steps {
            if let Some(Reference::Env { name, .. }) = step.auth.as_ref().map(|auth| &auth.bearer) {
                if !protocols.get(&step.protocol).is_some_and(|adapter| {
                    runtime.environment.get(name).is_some_and(|value| adapter.accepts_authorization(value))
                }) {
                    return Err(PrepareError::Environment { step_id: step.id.clone(), name: name.clone() });
                }
            }
        }
        Ok(Self {
            experiment,
            protocols,
            runtime,
            hidden_values,
            short_hidden,
            monitor: None,
            schema_cache,
            inspector: None,
        })
    }

    pub fn experiment(&self) -> &Experiment {
        &self.experiment
    }

    /// Install the hook called around each request of a step with `debug`.
    pub fn set_inspector(&mut self, inspector: Option<Box<dyn RequestInspector>>) {
        self.inspector = inspector;
    }

    /// Request templates of the steps with `print`, in step order.
    pub fn request_templates(&self) -> Vec<RequestTemplate> {
        self.experiment
            .steps
            .iter()
            .filter(|step| step.print)
            .filter_map(|step| self.protocols.get(&step.protocol)?.request_template(step))
            .collect()
    }

    pub fn set_monitor(&mut self, monitor: Option<Box<dyn ProcessMonitor>>) {
        self.monitor = monitor;
    }

    pub(crate) fn process_status(&mut self) -> Option<crate::target::ProcessStatus> {
        self.monitor.as_mut().map(|monitor| monitor.status())
    }

    pub(crate) fn baseline_value(&self, step: &Step, selector: &str) -> Option<ReplacementValue> {
        self.protocols.get(&step.protocol)?.baseline_value(step, selector)
    }

    pub(crate) fn accepts_variant(&self, step: &Step, selector: &str, value: &ReplacementValue) -> bool {
        self.protocols
            .get(&step.protocol)
            .is_some_and(|adapter| adapter.accepts_variant(step, selector, value))
    }

    pub(crate) fn contract_fingerprint(&self, step_id: &str) -> Option<[u8; 32]> {
        let step = self.experiment.steps.iter().find(|step| step.id == step_id)?;
        self.protocols.get(&step.protocol)?.contract_fingerprint()
    }

    /// Distinct operations the experiment calls (an `http` and an
    /// `http_raw` step on the same operation count once).
    pub(crate) fn endpoint_count(&self) -> usize {
        self.experiment
            .steps
            .iter()
            .filter(|step| matches!(step.protocol, ProtocolKind::Http | ProtocolKind::HttpRaw))
            .map(|step| step.operation_key.as_str())
            .collect::<std::collections::HashSet<_>>()
            .len()
    }

    /// Values shown as `[hidden]` in the display, records and reports.
    pub(crate) fn hidden_values(&self) -> &[String] {
        &self.hidden_values
    }

    /// `hidden` variables whose value is too short (or empty) to hide.
    pub(crate) fn short_hidden(&self) -> &[String] {
        &self.short_hidden
    }

    /// Execute the baseline case (no mutations applied).
    pub fn execute_baseline(&mut self) -> Result<CaseResult, RunError> {
        self.execute_with_scope(&[], None, None, None)
    }

    /// Execute a case with a set of replacements.
    pub fn execute_case(
        &mut self,
        plan: &[(String, ReplacementValue)],
    ) -> Result<CaseResult, RunError> {
        self.execute_with_scope(plan, None, None, None)
    }

    /// Execute a case that replaces several fields of one step.
    pub(crate) fn execute_replacements(
        &mut self,
        step_id: &str,
        replacements: &[(String, crate::experiment::types::ReplacementValue)],
    ) -> Result<CaseResult, RunError> {
        self.execute_with_scope(replacements, Some(step_id), None, None)
    }

    pub(crate) fn execute_variant(
        &mut self,
        step_id: &str,
        selector: &str,
        value: crate::experiment::types::ReplacementValue,
    ) -> Result<CaseResult, RunError> {
        self.execute_with_scope(&[(selector.to_owned(), value)], Some(step_id), None, None)
    }

    pub(crate) fn execute_fuzz_case(
        &mut self,
        case: &crate::mutation::fuzz::FuzzCase,
    ) -> Result<CaseResult, RunError> {
        if let Some(rename) = &case.key_rename {
            self.execute_with_scope(&[], Some(&case.step_id), Some(&rename.action), None)
        } else {
            self.execute_replacements(&case.step_id, &case.replacements)
        }
    }

    pub(crate) fn execute_example(
        &mut self,
        step_id: &str,
        example: &ExampleCase,
    ) -> Result<CaseResult, RunError> {
        match &example.action {
            ExampleAction::Field { path, value } => self.execute_variant(
                step_id, &format!("body.{path}"), ReplacementValue::JsonValue(value.clone()),
            ),
            ExampleAction::Json(action) => self.execute_with_scope(&[], Some(step_id), Some(action), None),
        }
    }

    /// Execute a case that breaks one `http_raw` step's request.
    pub(crate) fn execute_malform(
        &mut self,
        step_id: &str,
        malformation: crate::adapters::http_raw::Malformation,
    ) -> Result<CaseResult, RunError> {
        self.execute_with_scope(&[], Some(step_id), None, Some(crate::adapters::Breakage::Request(malformation)))
    }

    /// Execute a case that breaks one cli step's `.env`-style file.
    pub(crate) fn execute_envfile(&mut self, step_id: &str, case: &str) -> Result<CaseResult, RunError> {
        self.execute_with_scope(&[], Some(step_id), None, Some(crate::adapters::Breakage::EnvFile(case.to_owned())))
    }

    fn execute_with_scope(
        &mut self,
        replacements: &[(String, ReplacementValue)],
        step_id: Option<&str>,
        json_example: Option<&JsonExample>,
        breakage: Option<crate::adapters::Breakage>,
    ) -> Result<CaseResult, RunError> {
        let ctx = CaseContext {
            case_id: Uuid::new_v4().to_string(),
            runtime: &self.runtime,
        };
        let variant_step = step_id;
        let mut requests_sent = 0u32;
        let mut step_results = Vec::new();
        let mut step_outputs = StepOutputs::new();

        for step in &self.experiment.steps {
            // Check if required outputs from previous steps are available
            if let Some(reason) = check_dependencies(step, &step_outputs) {
                step_results.push(StepResult {
                    step_id: step.id.clone(),
                    outcome: StepOutcome::Skipped { reason },
                    observation: None,
                    request_diagnostic: None,
                    extracted_outputs: HashMap::new(),
                    sensitive_response: false,
                });
                continue;
            }

            let adapter = self.protocols.get_mut(&step.protocol).ok_or_else(|| {
                RunError::Infrastructure(format!("adapter unavailable for step '{}'", step.id))
            })?;
            let selected = variant_step.is_none_or(|id| id == step.id);
            let step_replacements = if selected { replacements } else { &[] };
            let step_example = if selected { json_example } else { None };
            let step_breakage = if selected { breakage.clone() } else { None };
            let inspection = match (&step.debug, self.inspector.as_deref_mut()) {
                (Some(_), Some(inspector)) => Some(Inspection {
                    inspector,
                    secrets: inspection_secrets(&self.hidden_values, &step_outputs),
                }),
                _ => None,
            };
            let changes = StepChanges {
                replacements: step_replacements,
                json_example: step_example,
                breakage: step_breakage,
            };
            let execution = execute_step(step, adapter.as_mut(), &ctx, changes, &step_outputs, inspection)?;
            requests_sent = requests_sent.wrapping_add(execution.requests_sent);
            match execution.result {
                Ok(observation) => {
                    let outputs = extract_step_outputs(step, &observation);
                    let sensitive_response = uses_sensitive_binding(step, &step_outputs);
                    let published = step
                        .outputs
                        .iter()
                        .filter_map(|declaration| {
                            let value = outputs.get(&declaration.name)?.clone();
                            Some((
                                declaration.name.clone(),
                                PublishedOutput {
                                    value,
                                    sensitive: declaration.secret || sensitive_response,
                                },
                            ))
                        })
                        .collect();
                    step_outputs.insert(step.id.clone(), published);
                    step_results.push(StepResult {
                        step_id: step.id.clone(),
                        outcome: StepOutcome::Completed,
                        observation: Some(observation),
                        request_diagnostic: execution.request_diagnostic,
                        extracted_outputs: outputs,
                        sensitive_response,
                    });
                }
                Err(outcome) => {
                    step_results.push(StepResult {
                        step_id: step.id.clone(),
                        outcome,
                        observation: None,
                        request_diagnostic: execution.request_diagnostic,
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
            requests_sent,
        })
    }
}

// ── Step execution ─────────────────────────────────────────────────

/// Whether a step's request carries a secret output (`secret: true`, or
/// one derived from it), so its response data is kept out of evidence.
/// Environment values are hidden by value only, through `hidden`.
fn uses_sensitive_binding(step: &Step, step_outputs: &StepOutputs) -> bool {
    // Bearer authentication uses its own channel. Ordinary bindings can embed secret data in the request.
    step
        .bindings
        .iter()
        .flat_map(|binding| extract_references(&binding.value))
        .any(|reference| match reference {
            Reference::StepOutput {
                step_id,
                output_name,
            } => step_outputs
                .get(&step_id)
                .and_then(|outputs| outputs.get(&output_name))
                .is_some_and(|output| output.sensitive),
            _ => false,
        })
}

/// Values `debug` redacts: the environment variables the experiment reads
/// that `hidden` lists and the secret outputs published so far.
fn inspection_secrets(hidden: &[String], step_outputs: &StepOutputs) -> Vec<String> {
    let outputs = step_outputs
        .values()
        .flat_map(HashMap::values)
        .filter(|output| output.sensitive)
        .map(|output| output.value.as_str().map_or_else(|| output.value.to_string(), str::to_owned));
    hidden.iter().cloned().chain(outputs).collect()
}

/// What a case changes in the step it targets.
struct StepChanges<'a> {
    replacements: &'a [(String, ReplacementValue)],
    json_example: Option<&'a JsonExample>,
    breakage: Option<crate::adapters::Breakage>,
}

fn execute_step(
    step: &Step,
    adapter: &mut dyn StepProtocol,
    ctx: &CaseContext<'_>,
    changes: StepChanges<'_>,
    step_outputs: &StepOutputs,
    inspection: Option<Inspection<'_>>,
) -> Result<AdapterExecution, RunError> {
    let bindings = match resolve_runtime_bindings(&step.bindings, ctx, step_outputs) {
        Ok(bindings) => bindings,
        Err(error) => return Ok(AdapterExecution::input_error(error)),
    };
    let authorization = step.auth.as_ref()
        .map(|auth| resolve_reference(&auth.bearer, ctx, step_outputs))
        .transpose();
    let authorization = match authorization {
        Ok(value) => value,
        Err(error) => return Ok(AdapterExecution::input_error(error)),
    };
    adapter.execute(step, StepInput {
        bindings: &bindings,
        authorization,
        replacements: changes.replacements,
        json_example: changes.json_example,
        inspection,
        breakage: changes.breakage,
    })
}

/// Resolve runtime references in bindings (env, case_id, step outputs).
fn resolve_runtime_bindings(
    bindings: &[Binding],
    ctx: &CaseContext<'_>,
    step_outputs: &StepOutputs,
) -> Result<Vec<Binding>, String> {
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
) -> Result<Value, String> {
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
    resolved.ok_or_else(|| format!("required field '{reference}' has no example, default, or binding"))
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

        }
        Ok(Self {
            environment,
            target_port,
        })
    }
}

/// Extract declared outputs from the adapter observation.
fn extract_step_outputs(step: &Step, observation: &StepObservation) -> HashMap<String, Value> {
    let mut outputs = HashMap::new();

    for output_decl in &step.outputs {
        // Check when_status
        if let Some(ref statuses) = output_decl.when_status {
            if !observation.http_status().is_some_and(|status| statuses.contains(&status)) {
                continue; // status doesn't match — output is absent (not an error)
            }
        }

        if let crate::observation::SelectedValue::Present(value) =
            observation.select(&output_decl.from)
        {
            outputs.insert(output_decl.name.clone(), value);
        }
    }
    outputs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::http::transport::HttpObservation;
    use crate::experiment::selectors::ObservationSelector;
    use crate::observation::{select_response, ObservationError, SelectedValue};

    #[test]
    fn runs_a_workflow_with_a_non_http_adapter() {
        use crate::evaluation::Verdict;
        use crate::experiment::selectors::{InputSelector, JsonPathSegment};
        use crate::experiment::types::{
            ProtocolConfigs, ExpectCondition, ExplorationConfig, ExplorationStrategy, JsonMutatorMode,
            MaxCases, MutatorDecl, OracleDecl, OracleType, OutputDecl,
        };
        use crate::exploration::{run_enumeration, RunBudget};
        use serde_json::json;

        struct InMemory;

        impl StepProtocol for InMemory {
            fn execute(&mut self, step: &Step, input: StepInput<'_>) -> Result<AdapterExecution, RunError> {
                let result = if step.id == "source" {
                    assert!(input.replacements.is_empty());
                    json!({"value": "published"})
                } else {
                    let [binding] = input.bindings else {
                        panic!("expected one resolved binding");
                    };
                    let BindingValue::Literal(value) = &binding.value else {
                        panic!("expected a literal binding");
                    };
                    let variant = match input.replacements {
                        [] => "original",
                        [(selector, ReplacementValue::JsonString(value))] => {
                            assert_eq!(selector, "body.variant");
                            value
                        }
                        _ => panic!("expected one string replacement"),
                    };
                    json!({"value": value, "variant": variant})
                };
                Ok(AdapterExecution {
                    result: Ok(StepObservation::InMemory(result)),
                    requests_sent: 0,
                    request_diagnostic: None,
                })
            }

            fn accepts_authorization(&self, _: &str) -> bool { false }

            fn baseline_value(&self, _: &Step, selector: &str) -> Option<ReplacementValue> {
                assert_eq!(selector, "body.variant");
                Some(ReplacementValue::JsonString("original".into()))
            }

            fn accepts_variant(&self, _: &Step, _: &str, _: &ReplacementValue) -> bool { false }
            fn contract_fingerprint(&self) -> Option<[u8; 32]> { None }
        }

        let body_value = ObservationSelector::Body(vec![JsonPathSegment::Key("value".into())]);
        let body_variant = ObservationSelector::Body(vec![JsonPathSegment::Key("variant".into())]);
        let source = Step {
            id: "source".into(),
            protocol: ProtocolKind::InMemory,
            format: crate::formats::PayloadFormat::Json,
            operation_key: "publish".into(),
            body_example: None,
            bindings: Vec::new(),
            auth: None,
            outputs: vec![OutputDecl {
                name: "value".into(),
                from: body_value.clone(),
                when_status: None,
                secret: true,
            }],
            mutators: Vec::new(),
            corpora: Vec::new(),
            key_mutators: Vec::new(),
            print: false,
            debug: None,
            oracles: Vec::new(),
            examples: Vec::new(),
            declared_statuses: Vec::new(),
            command: None,
            env_config: None,
            cli: None,
        };
        let sink = Step {
            id: "sink".into(),
            operation_key: "consume".into(),
            bindings: vec![Binding {
                selector: InputSelector::BodyField(vec!["value".into()]),
                value: BindingValue::Reference(Reference::StepOutput {
                    step_id: "source".into(),
                    output_name: "value".into(),
                }),
            }],
            outputs: vec![OutputDecl {
                name: "variant".into(),
                from: body_variant,
                when_status: None,
                secret: false,
            }],
            oracles: vec![OracleDecl {
                id: "received-published-value".into(),
                oracle_type: OracleType::Assert,
                kind: None,
                setup: true,
                select: Some(body_value),
                when_condition: None,
                expect: Some(ExpectCondition::Equals(json!("published"))),
                record_reference: None,
            }],
            mutators: vec![MutatorDecl::Json {
                fields: Some(vec!["variant".into()]),
                mode: JsonMutatorMode::Alphanumeric,
                values: vec![json!("original"), json!("changed")],
                field_schemas: HashMap::new(),
                lines: None,
            }],
            ..source.clone()
        };
        let experiment = Arc::new(Experiment {
            name: "in-memory-workflow".into(),
            max_cases: MaxCases::new(2),
            exploration: ExplorationConfig { strategy: ExplorationStrategy::Enumerate },
            target: None,
            protocols: ProtocolConfigs { http: None, cli: None },
            steps: vec![source, sink],
            hidden: Vec::new(),
        });
        let protocols = HashMap::from([(ProtocolKind::InMemory, Box::new(InMemory) as Box<dyn StepProtocol>)]);
        let mut runner = WorkflowRunner::with_protocols(
            experiment.clone(), protocols, None, Arc::new(SchemaValidatorCache::new()),
        ).unwrap();
        let run = run_enumeration(&mut runner, &experiment, &mut RunBudget::new(2, None, None)).unwrap();
        assert_eq!(run.cases.len(), 1);
        for (result, variant) in [(&run.baseline_case, "original"), (&run.cases[0], "changed")] {
            assert_eq!(result.checks[1][0].evaluation.verdict, Verdict::Pass);
            let step = &result.execution.step_results[1];
            assert_eq!(step.extracted_outputs["variant"], json!(variant));
            assert!(step.sensitive_response);
        }
        assert!(run.findings.is_empty());
    }

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
        let port = url::Url::parse("http://127.0.0.1")
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
