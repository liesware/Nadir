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
use crate::experiment::types::{ExampleCase, Experiment, ReplacementValue};
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
    Example { step_id: String, example: ExampleCase },
    Fuzz {
        case: crate::mutation::fuzz::FuzzCase,
        reference: crate::artifacts::FuzzCaseRef,
    },
    Corpus {
        reference: crate::mutation::text::CorpusCaseRef,
        value: ReplacementValue,
    },
    Malform {
        step_id: String,
        malformation: crate::adapters::http_raw::Malformation,
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
    if !matches!(
        version,
        1 | CaseRecord::CURRENT_VERSION | CaseRecord::CORPUS_VERSION | CaseRecord::JSON_EXAMPLE_VERSION
            | CaseRecord::MALFORM_VERSION
    ) {
        return Err(ReplayError::InputError(format!(
            "unsupported case record version {version}"
        )));
    }
    let plan = record.regression_plan.ok_or_else(|| {
        ReplayError::InputError("record has no replayable regression plan".into())
    })?;
    if (version == CaseRecord::MALFORM_VERSION) != plan.malform.is_some() {
        return Err(ReplayError::InputError("invalid malform case record version".into()));
    }
    if (version == CaseRecord::CORPUS_VERSION) != plan.corpus.is_some() {
        return Err(ReplayError::InputError("invalid corpus case record version".into()));
    }
    if (version == CaseRecord::JSON_EXAMPLE_VERSION) != plan.json_example.is_some()
        || (plan.json_example.is_some() && (plan.example_id.is_none() || plan.example_step_id.is_none()))
    {
        return Err(ReplayError::InputError("invalid JSON example case record".into()));
    }
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
        || (plan.corpus.is_some() && (plan.fuzz.is_some() || plan.variant.is_some() || plan.example_id.is_some()))
    {
        return Err(ReplayError::InputError(
            "saved case has conflicting example or variant identifiers".into(),
        ));
    }
    let pending = match (plan.variant, &plan.example_id) {
        // A malform case is rebuilt from its name; the step must still run it.
        (None, None) if plan.malform.is_some() => {
            let reference = plan.malform.expect("checked above");
            let malformation = crate::adapters::http_raw::Malformation::from_name(&reference.name)
                .ok_or_else(|| ReplayError::InputError(format!("unknown malformation '{}'", reference.name)))?;
            let declared = runner.experiment().steps.iter()
                .filter(|step| step.id == reference.step_id)
                .flat_map(|step| &step.mutators)
                .any(|mutator| matches!(
                    mutator,
                    crate::experiment::types::MutatorDecl::Malform { malformations } if malformations.contains(&malformation)
                ));
            if !declared {
                return Err(ReplayError::InputError(format!(
                    "step '{}' no longer runs malformation '{}'",
                    reference.step_id, reference.name
                )));
            }
            PendingCase::Malform { step_id: reference.step_id, malformation }
        }
        (None, None) if plan.corpus.is_some() => {
            let reference = plan.corpus.expect("checked above");
            let corpus = runner.experiment().steps.iter()
                .find(|step| step.id == reference.step_id)
                .and_then(|step| step.corpora.get(reference.corpus_index))
                .ok_or_else(|| ReplayError::InputError("saved corpus is unavailable".into()))?;
            if corpus.sha256 != reference.sha256 {
                return Err(ReplayError::InputError("saved corpus fingerprint differs from the current file".into()));
            }
            if reference.iteration >= corpus.repetitions {
                return Err(ReplayError::InputError("saved corpus iteration is outside its repetitions".into()));
            }
            let (actual, value) = corpus.case(
                &reference.step_id, reference.corpus_index, reference.iteration,
            );
            if actual != reference {
                return Err(ReplayError::InputError("saved corpus reference is incompatible with the experiment".into()));
            }
            PendingCase::Corpus { reference, value }
        }
        // A random case is regenerated from its seed and iteration.
        (None, None) if plan.fuzz.is_some() => {
            let reference = plan.fuzz.expect("checked above");
            // Regenerate with the rule and limit the case was saved with.
            let version = crate::mutation::fuzz::GeneratorVersion::from_number(reference.generator)
                .ok_or_else(|| {
                    ReplayError::InputError(format!(
                        "saved by a newer Nadir (generator {})",
                        reference.generator
                    ))
                })?;
            let case = crate::mutation::fuzz::generate_case_with(
                runner.experiment(),
                |step, selector| runner.baseline_value(step, selector),
                reference.seed,
                reference.iteration,
                version,
                reference.max_fields,
            )
            .ok_or_else(|| {
                ReplayError::InputError("the experiment no longer declares mutable fields".into())
            })?;
            PendingCase::Fuzz { case, reference }
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
            if example.json_example() != plan.json_example.as_ref() {
                return Err(ReplayError::InputError(
                    "saved JSON example differs from the experiment".into(),
                ));
            }
            PendingCase::Example { step_id: step.id.clone(), example: example.clone() }
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
        PendingCase::Example { step_id, example } => {
            let execution = runner.execute_example(&step_id, &example)?;
            let plan = ExecutedPlan::Example {
                example_id: example.id.clone(),
                step_id,
                expect_status: example.expect_status,
                json_example: example.json_example().cloned(),
            };
            (execution, plan)
        }
        PendingCase::Fuzz { case, reference } => {
            let execution = runner.execute_fuzz_case(&case)?;
            let key_expectation = case.key_rename.as_ref().and_then(|rename| rename.expectation());
            let plan = ExecutedPlan::Fuzz {
                step_id: case.step_id,
                replacements: case.replacements,
                seed: reference.seed,
                iteration: reference.iteration,
                generator: reference.generator,
                max_fields: reference.max_fields,
                key_expectation,
            };
            (execution, plan)
        }
        PendingCase::Corpus { reference, value } => {
            let selector = format!("body.{}", reference.field);
            let execution = runner.execute_variant(&reference.step_id, &selector, value)?;
            (execution, ExecutedPlan::Corpus(reference))
        }
        PendingCase::Malform { step_id, malformation } => {
            let execution = runner.execute_malform(&step_id, malformation)?;
            (execution, ExecutedPlan::Malform { step_id, malformation })
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
    step.is_some_and(|step| runner.accepts_variant(step, &variant.selector, value))
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
