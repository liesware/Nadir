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

use crate::adapters::cli::envbreak;
use crate::artifacts::{FindingRecord, ReplayPlan, SavedCase, SavedExample};
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
pub(crate) struct PreparedReplay<'a> {
    pending: PendingCase<'a>,
    contract: ReplayContract,
}

enum PendingCase<'a> {
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
    Envar {
        reference: crate::artifacts::EnvarRef,
        value: Option<String>,
    },
    EnvFile(envbreak::DeclaredCase<'a>),
    Baseline,
}

/// Load a saved case and check it against the experiment without sending
/// any request, so a record that cannot be replayed fails before a target
/// process is started.
pub(crate) fn prepare_replay<'a>(
    record_path: &Path,
    configuration: &'a ResolvedExperiment,
    runner: &WorkflowRunner,
) -> Result<PreparedReplay<'a>, ReplayError> {
    let fingerprints = &configuration.openapi_fingerprints;
    let ReplayPlan {
        version,
        experiment_sha256,
        openapi_fingerprints,
        case,
    } = storage::load_case_record(record_path)?
        .into_replay_plan()
        .map_err(ReplayError::InputError)?;
    if experiment_sha256 != configuration.sha256 {
        return Err(ReplayError::InputError(
            "experiment YAML differs from the saved regression plan".into(),
        ));
    }
    let contract = if version == 1 {
        ReplayContract::Legacy
    } else {
        // An experiment without OpenAPI (only cli steps) records no
        // fingerprints; one that declares OpenAPI always records them.
        if openapi_fingerprints.is_empty() && !fingerprints.is_empty() {
            return Err(ReplayError::InputError(
                "version 2 record has no OpenAPI fingerprints".into(),
            ));
        }
        let changed: Vec<_> = openapi_fingerprints
            .keys()
            .chain(fingerprints.keys())
            .filter(|path| openapi_fingerprints.get(*path) != fingerprints.get(*path))
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
    let pending = match case {
        SavedCase::Envar(reference) => {
            let value = crate::exploration::enumerate::envar_enumerated_cases(runner.experiment())
                .into_iter()
                .find(|(step, variable, case, _)| {
                    *step == reference.step_id && *variable == reference.variable && *case == reference.case
                })
                .map(|(_, _, _, value)| value)
                .ok_or_else(|| ReplayError::InputError(match &reference.case {
                    crate::artifacts::EnvarCase::Line { line, .. } => format!(
                        "the file of {} in step '{}' changed since line {line} was saved",
                        reference.variable, reference.step_id
                    ),
                    _ => format!("step '{}' no longer sets {} that way", reference.step_id, reference.variable),
                }))?;
            PendingCase::Envar { reference, value }
        }
        SavedCase::EnvFile(reference) => {
            let declaration = envbreak::find_case(&configuration.experiment, &reference.step_id, &reference.name)
                .ok_or_else(|| ReplayError::InputError(format!(
                    "step '{}' no longer runs envfile case '{}'",
                    reference.step_id, reference.name
                )))?;
            PendingCase::EnvFile(declaration)
        }
        // A malform case is rebuilt from its name; the step must still run it.
        SavedCase::Malform(reference) => {
            let malformation = crate::adapters::http_raw::Malformation::from_name(&reference.name)
                .ok_or_else(|| ReplayError::InputError(format!("unknown malformation '{}'", reference.name)))?;
            let declared = runner.experiment().steps.iter()
                .filter(|step| step.id == reference.step_id)
                .flat_map(|step| &step.mutators)
                .any(|mutator| matches!(
                    mutator,
                    crate::experiment::types::MutatorDecl::Malform { malformations, .. } if malformations.contains(&malformation)
                ));
            if !declared {
                return Err(ReplayError::InputError(format!(
                    "step '{}' no longer runs malformation '{}'",
                    reference.step_id, reference.name
                )));
            }
            PendingCase::Malform { step_id: reference.step_id, malformation }
        }
        SavedCase::Corpus(reference) => {
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
        SavedCase::Fuzz(reference) => {
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
        SavedCase::Variant(variant) => {
            let value = variant
                .resolve(runner.experiment(), version)
                .ok_or_else(|| {
                    ReplayError::InputError(match variant.saved_line() {
                        Some(line) => format!(
                            "the file of {} in step '{}' changed since line {line} was saved",
                            variant.selector, variant.step_id
                        ),
                        None => "saved variant is incompatible with the experiment".into(),
                    })
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
        SavedCase::Example(example) => {
            let (example_id, example_step_id, json_example) = match &example {
                SavedExample::Ordinary { id, step_id } => (id, step_id.as_ref(), None),
                SavedExample::Json { id, step_id, action } => (id, Some(step_id), Some(action)),
            };
            let mut matching = runner
                .experiment()
                .steps
                .iter()
                .filter(|step| example_step_id.is_none_or(|id| id == &step.id))
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
            if example.json_example() != json_example {
                return Err(ReplayError::InputError(
                    "saved JSON example differs from the experiment".into(),
                ));
            }
            PendingCase::Example { step_id: step.id.clone(), example: example.clone() }
        }
        SavedCase::Baseline => PendingCase::Baseline,
    };
    Ok(PreparedReplay { pending, contract })
}

/// Execute a prepared case against the running service.
pub(crate) fn execute_replay(
    prepared: PreparedReplay<'_>,
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
            let body_edits = case.edits_body();
            let plan = ExecutedPlan::Fuzz {
                step_id: case.step_id,
                replacements: case.replacements,
                seed: reference.seed,
                iteration: reference.iteration,
                generator: reference.generator,
                max_fields: reference.max_fields,
                key_expectation,
                body_edits,
            };
            (execution, plan)
        }
        PendingCase::Corpus { reference, value } => {
            let selector = format!("body.{}", reference.field);
            let execution = runner.execute_variant(&reference.step_id, &selector, value)?;
            (execution, ExecutedPlan::Corpus(reference))
        }
        PendingCase::Envar { reference, value } => {
            let execution = runner.execute_variant(
                &reference.step_id,
                &reference.variable,
                crate::experiment::types::envar_replacement(&value),
            )?;
            let plan = ExecutedPlan::Envar {
                step_id: reference.step_id,
                variable: reference.variable,
                case: reference.case,
                value,
            };
            (execution, plan)
        }
        PendingCase::EnvFile(declaration) => {
            let (execution, evidence) = runner.execute_envfile(declaration)?;
            let case = EvaluatedCase::new_envfile(
                runner.experiment(), execution, evidence, &runner.schema_cache,
            );
            return Ok(RegressionReplay { case, contract });
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

/// Load a saved finding and report that finding replay is unavailable.
///
/// To reproduce the finding, pass its associated case record to
/// `nadir replay <experiment> --case <case.json>`.
#[deprecated(note = "use nadir replay <experiment> --case <case.json> with the associated case record")]
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::{CaseRecord, RegressionPlan};
    use std::fs;

    struct TestDirectory(PathBuf);

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn assert_replay_input_error(change_record: impl FnOnce(&mut CaseRecord), expected: &str) {
        let directory = std::env::temp_dir().join(format!("nadir-replay-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        let directory = TestDirectory(directory);
        let experiment_path = directory.0.join("experiment.yaml");
        fs::write(
            &experiment_path,
            r#"
version: 1
experiment:
  name: replay-test
  max_cases: 1
  protocols:
    http:
      base_url: http://127.0.0.1:8080
      openapi: openapi.yaml
  steps:
    - id: read_item
      protocol: http
      operation_id: readItem
"#,
        )
        .unwrap();
        fs::write(
            directory.0.join("openapi.yaml"),
            r#"
openapi: '3.1.0'
paths:
  /item:
    get:
      operationId: readItem
      responses:
        '200':
          description: OK
"#,
        )
        .unwrap();

        let configuration = crate::config::load_resolved(&experiment_path).unwrap();
        assert!(!configuration.openapi_fingerprints.is_empty());
        let runner = WorkflowRunner::from_resolved(&configuration).unwrap();
        let record_path = directory.0.join("case.json");
        let mut record = CaseRecord {
            artifact_version: CaseRecord::CURRENT_VERSION,
            case_id: "baseline".into(),
            case_number: 0,
            step_results: vec![],
            coverage_snapshot_sha256: None,
            regression_plan: Some(RegressionPlan {
                experiment_sha256: configuration.sha256.clone(),
                openapi_fingerprints: configuration.openapi_fingerprints.clone(),
                variant: None,
                example_id: None,
                example_step_id: None,
                fuzz: None,
                corpus: None,
                json_example: None,
                malform: None,
                envar: None,
                envfile: None,
            }),
            process_exit: None,
        };
        fs::write(&record_path, serde_json::to_vec(&record).unwrap()).unwrap();
        let prepared = prepare_replay(&record_path, &configuration, &runner).unwrap();
        assert!(matches!(prepared.pending, PendingCase::Baseline));
        assert!(matches!(prepared.contract, ReplayContract::Matching));

        change_record(&mut record);
        fs::write(&record_path, serde_json::to_vec(&record).unwrap()).unwrap();
        match prepare_replay(&record_path, &configuration, &runner) {
            Err(ReplayError::InputError(message)) => assert_eq!(message, expected),
            Err(error) => panic!("expected input error, got {error}"),
            Ok(_) => panic!("expected input error: {expected}"),
        }
    }

    #[test]
    fn rejects_unknown_case_record_version() {
        assert_replay_input_error(
            |record| record.artifact_version = 999,
            "unsupported case record version 999",
        );
    }

    #[test]
    fn rejects_changed_experiment_yaml() {
        assert_replay_input_error(
            |record| record.regression_plan.as_mut().unwrap().experiment_sha256 = "0".repeat(64),
            "experiment YAML differs from the saved regression plan",
        );
    }

    #[test]
    fn rejects_v2_case_record_without_openapi_fingerprints() {
        assert_replay_input_error(
            |record| {
                record
                    .regression_plan
                    .as_mut()
                    .unwrap()
                    .openapi_fingerprints
                    .clear();
            },
            "version 2 record has no OpenAPI fingerprints",
        );
    }
}
