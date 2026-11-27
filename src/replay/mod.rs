//! Replay engine: load a saved case or finding, execute through the
//! workflow runner, and compare results.
//!
//! # Design notes
//!
//! **Chosen:** replay reuses `WorkflowRunner` — no duplicated step-execution
//! logic. Dynamic outputs and secrets are re-acquired at runtime rather than
//! replayed from saved values.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::artifacts::{CaseRecord, FindingRecord};
use crate::config::ResolvedExperiment;
use crate::evaluation::case::{EvaluatedCase, ExecutedPlan};
use crate::execution::runner::{CaseResult, WorkflowRunner};
use crate::experiment::selectors::{parse_input_selector, InputSelector};
use crate::experiment::types::{Experiment, ReplacementValue};
use crate::mutation::RegressionVariant;
use crate::storage;

/// Errors during replay.
#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("deserialization error: {0}")]
    Deserialize(#[from] serde_json::Error),
    #[error("runner error: {0}")]
    Runner(#[from] crate::execution::runner::RunError),
    #[error("contract has changed: {0:?}")]
    ChangedContract(Vec<PathBuf>),
    #[error("input error: {0}")]
    InputError(String),
    #[error("storage error: {0}")]
    Storage(#[from] crate::storage::StorageError),
}

/// Result of replaying a case.
pub struct ReplayResult {
    pub case_result: CaseResult,
    pub comparison: ReplayComparison,
    pub contract_changes: Vec<PathBuf>,
}

/// Comparison between the saved and replayed results.
pub enum ReplayComparison {
    /// Results match (equivalent signatures).
    Match,
    /// Results differ.
    Mismatch { differences: Vec<String> },
    /// The OpenAPI contract has changed since the original run.
    ChangedContract,
}

pub(crate) struct RegressionReplay {
    pub case: EvaluatedCase,
    pub contract: ReplayContract,
}

pub(crate) enum ReplayContract {
    Legacy,
    Matching,
    Changed(Vec<String>),
}

/// A saved case that has been checked against the experiment and is ready to run.
pub(crate) struct PreparedReplay {
    pending: PendingCase,
    contract: ReplayContract,
}

enum PendingCase {
    Variant {
        variant: RegressionVariant,
        value: ReplacementValue,
    },
    Example {
        example_id: String,
        step_id: String,
        selector: String,
        value: ReplacementValue,
        expect_status: u16,
    },
    Fuzz {
        case: crate::mutation::fuzz::FuzzCase,
        seed: u64,
        iteration: u64,
    },
    Baseline,
}

/// Load a saved case and check it against the experiment without sending
/// any request, so a record that cannot be replayed fails before a target
/// process is started.
pub(crate) fn prepare_replay(
    record_path: &Path,
    configuration: &ResolvedExperiment,
    runner: &WorkflowRunner,
) -> Result<PreparedReplay, ReplayError> {
    let fingerprints = &configuration.openapi_fingerprints;
    let record = storage::load_case_record(record_path)?;
    let version = record.artifact_version;
    if version != 1 && version != CaseRecord::CURRENT_VERSION {
        return Err(ReplayError::InputError(format!(
            "unsupported case record version {version}"
        )));
    }
    let plan = record.regression_plan.ok_or_else(|| {
        ReplayError::InputError("record has no replayable regression plan".into())
    })?;
    if plan.experiment_sha256 != configuration.sha256 {
        return Err(ReplayError::InputError(
            "experiment YAML differs from the saved regression plan".into(),
        ));
    }
    let contract = if version == 1 {
        ReplayContract::Legacy
    } else {
        if plan.openapi_fingerprints.is_empty() {
            return Err(ReplayError::InputError(
                "version 2 record has no OpenAPI fingerprints".into(),
            ));
        }
        let changed: Vec<_> = plan
            .openapi_fingerprints
            .keys()
            .chain(fingerprints.keys())
            .filter(|path| plan.openapi_fingerprints.get(*path) != fingerprints.get(*path))
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if changed.is_empty() {
            ReplayContract::Matching
        } else {
            ReplayContract::Changed(changed)
        }
    };
    if (plan.example_id.is_none() && plan.example_step_id.is_some())
        || (plan.example_id.is_some() && plan.variant.is_some())
        || (plan.fuzz.is_some() && (plan.variant.is_some() || plan.example_id.is_some()))
    {
        return Err(ReplayError::InputError(
            "saved case has conflicting example or variant identifiers".into(),
        ));
    }
    let pending = match (plan.variant, &plan.example_id) {
        // A random case is regenerated from its seed and iteration.
        (None, None) if plan.fuzz.is_some() => {
            let reference = plan.fuzz.expect("checked above");
            let case = crate::mutation::fuzz::generate_case(
                runner.experiment(),
                runner.http_contract(),
                reference.seed,
                reference.iteration,
            )
            .ok_or_else(|| {
                ReplayError::InputError("the experiment no longer declares mutable fields".into())
            })?;
            PendingCase::Fuzz {
                case,
                seed: reference.seed,
                iteration: reference.iteration,
            }
        }
        (Some(variant), _) => {
            let value = variant
                .resolve(runner.experiment(), version)
                .ok_or_else(|| {
                    ReplayError::InputError(
                        "saved variant is incompatible with the experiment".into(),
                    )
                })?;
            if matches!(contract, ReplayContract::Changed(_))
                && !selector_in_contract(&variant, &value, runner)
            {
                return Err(ReplayError::InputError(
                    "saved selector is absent or its variant is incompatible with the changed OpenAPI operation".into(),
                ));
            }
            PendingCase::Variant { variant, value }
        }
        (None, Some(example_id)) => {
            let mut matching = runner
                .experiment()
                .steps
                .iter()
                .filter(|step| {
                    plan.example_step_id
                        .as_ref()
                        .is_none_or(|id| id == &step.id)
                })
                .flat_map(|step| {
                    step.examples
                        .iter()
                        .filter(move |example| &example.id == example_id)
                        .map(move |example| (step, example))
                });
            let (step, example) = matching.next().ok_or_else(|| {
                ReplayError::InputError(format!(
                    "example '{example_id}' not found in the saved step or experiment"
                ))
            })?;
            if matching.next().is_some() {
                return Err(ReplayError::InputError(format!(
                    "example '{example_id}' is ambiguous without a saved step ID"
                )));
            }
            PendingCase::Example {
                example_id: example_id.clone(),
                step_id: step.id.clone(),
                selector: format!("body.{}", example.field),
                value: ReplacementValue::JsonValue(example.value.clone()),
                expect_status: example.expect_status,
            }
        }
        (None, None) => PendingCase::Baseline,
    };
    Ok(PreparedReplay { pending, contract })
}

/// Execute a prepared case against the running service.
pub(crate) fn execute_replay(
    prepared: PreparedReplay,
    runner: &mut WorkflowRunner,
) -> Result<RegressionReplay, ReplayError> {
    let PreparedReplay { pending, contract } = prepared;
    let (execution, replayed_plan) = match pending {
        PendingCase::Variant { variant, value } => {
            let execution =
                runner.execute_variant(&variant.step_id, &variant.selector, value.clone())?;
            let plan = ExecutedPlan::Variant {
                step_id: variant.step_id.clone(),
                selector: variant.selector.clone(),
                replacement: value,
                replay: Some(variant),
            };
            (execution, plan)
        }
        PendingCase::Example {
            example_id,
            step_id,
            selector,
            value,
            expect_status,
        } => {
            let execution = runner.execute_variant(&step_id, &selector, value.clone())?;
            let plan = ExecutedPlan::Example {
                example_id,
                step_id,
                selector,
                replacement: value,
                expect_status,
            };
            (execution, plan)
        }
        PendingCase::Fuzz {
            case,
            seed,
            iteration,
        } => {
            let execution = runner.execute_replacements(&case.step_id, &case.replacements)?;
            let plan = ExecutedPlan::Fuzz {
                step_id: case.step_id,
                replacements: case.replacements,
                seed,
                iteration,
            };
            (execution, plan)
        }
        PendingCase::Baseline => (runner.execute_baseline()?, ExecutedPlan::Baseline),
    };
    let case = EvaluatedCase::new(
        runner.experiment(),
        execution,
        replayed_plan,
        &runner.schema_cache,
    );
    Ok(RegressionReplay { case, contract })
}

fn selector_in_contract(
    variant: &RegressionVariant,
    value: &ReplacementValue,
    runner: &WorkflowRunner,
) -> bool {
    let step = runner
        .experiment()
        .steps
        .iter()
        .find(|step| step.id == variant.step_id);
    let operation =
        step.and_then(|step| runner.http_contract()?.operations.get(&step.operation_id));
    let Some(operation) = operation else {
        return false;
    };
    match (parse_input_selector(&variant.selector), value) {
        (Ok(InputSelector::HeaderParam(header)), ReplacementValue::HeaderAscii(value)) => {
            operation.header_params.iter().any(|param| {
                param.name.eq_ignore_ascii_case(&header)
                    && param
                        .schema
                        .enum_values
                        .as_ref()
                        .is_none_or(|allowed| allowed.contains(value))
            })
        }
        // Typed values and removals come from the field's schema or the
        // experiment itself, so the field only has to still exist.
        (Ok(InputSelector::BodyField(path)), ReplacementValue::JsonValue(_))
        | (Ok(InputSelector::BodyField(path)), ReplacementValue::Absent) => {
            operation.request_body.as_ref().is_some_and(|body| {
                path.iter()
                    .try_fold(&body.schema.0, |schema, field| {
                        schema.get("properties")?.get(field)
                    })
                    .is_some()
            })
        }
        (Ok(InputSelector::BodyField(path)), ReplacementValue::JsonString(_)) => {
            operation.request_body.as_ref().is_some_and(|body| {
                path.iter()
                    .try_fold(&body.schema.0, |schema, field| {
                        schema.get("properties")?.get(field)
                    })
                    .is_some_and(|schema| {
                        schema.get("type").is_none_or(|kind| {
                            kind.as_str() == Some("string")
                                || kind.as_array().is_some_and(|types| {
                                    types.iter().any(|kind| kind.as_str() == Some("string"))
                                })
                        })
                    })
            })
        }
        _ => false,
    }
}

/// Replay a saved finding.
pub fn replay_finding(
    _experiment: &Experiment,
    finding_path: &Path,
    _runner: &mut WorkflowRunner,
) -> Result<ReplayResult, ReplayError> {
    let _finding: FindingRecord = storage::load_finding_record(finding_path)?;

    Err(ReplayError::InputError(
        "finding replay not yet fully implemented".into(),
    ))
}
