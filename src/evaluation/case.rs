use crate::execution::runner::{CaseResult, StepOutcome};
use crate::experiment::selectors::ObservationSelector;
use crate::experiment::types::{
    ExpectCondition, Experiment, OracleDecl, OracleType, ReplacementValue,
};
use crate::mutation::RegressionVariant;

use super::schema_validator::SchemaValidatorCache;

/// Implicit rule added to `mode: schema` cases whose value the schema forbids.
pub(crate) const SCHEMA_REJECTION_RULE: &str = "schema:rejects-invalid";
/// The implicit rule of every `malform` case.
pub(crate) const WIRE_HANDLED_RULE: &str = "wire:handled";
use super::{evaluate_check, OracleContext, OracleEvaluation, Verdict};

pub struct EvaluatedCase {
    pub execution: CaseResult,
    pub plan: ExecutedPlan,
    pub(crate) checks: Vec<Vec<CaseCheck>>,
    /// Set when the target process exited during this case.
    pub process_exit: Option<crate::artifacts::ProcessExitDiagnostic>,
}

pub enum ExecutedPlan {
    Baseline,
    Corpus(crate::mutation::text::CorpusCaseRef),
    Variant {
        step_id: String,
        selector: String,
        replacement: ReplacementValue,
        replay: Option<RegressionVariant>,
    },
    /// A random case from `strategy: fuzz`, reproducible from its seed and iteration.
    Fuzz {
        step_id: String,
        replacements: Vec<(String, ReplacementValue)>,
        seed: u64,
        iteration: u64,
        /// `GeneratorVersion` number that built the case.
        generator: u32,
        max_fields: Option<u16>,
        key_expectation: Option<(String, u16)>,
    },
    /// A `malform` case: one `http_raw` step's request broken one way.
    Malform {
        step_id: String,
        malformation: crate::adapters::http_raw::Malformation,
    },
    /// A case generated from a `cases.examples` entry.
    Example {
        example_id: String,
        step_id: String,
        expect_status: u16,
        json_example: Option<crate::experiment::types::JsonExample>,
    },
}

pub(crate) struct CaseCheck {
    pub oracle: OracleDecl,
    pub evaluation: OracleEvaluation,
    pub record_observation_on_pass: bool,
}

impl EvaluatedCase {
    pub(crate) fn new(
        experiment: &Experiment,
        execution: CaseResult,
        plan: ExecutedPlan,
        schema_cache: &SchemaValidatorCache,
    ) -> Self {
        let outputs = std::collections::HashMap::new();
        let checks = execution
            .step_results
            .iter()
            .enumerate()
            .map(|(index, result)| {
                let step = experiment
                    .steps
                    .iter()
                    .find(|step| step.id == result.step_id);
                let operation_id = step.map(|s| s.operation_key.as_str()).unwrap_or("");
                let declared_statuses = step.map(|s| s.declared_statuses.as_slice()).unwrap_or(&[]);
                let context = OracleContext {
                    current_step: result,
                    previous_steps: &execution.step_results[..index],
                    case_id: &execution.case_id,
                    step_outputs: &outputs,
                    operation_id,
                    declared_statuses,
                    schema_cache,
                };
                // A malformed request is judged by `wire:handled` alone: the
                // step's own oracles describe the well-formed baseline.
                let malformed = matches!(&plan, ExecutedPlan::Malform { step_id, .. } if step_id == &result.step_id);
                let mut checks: Vec<_> = step
                    .into_iter()
                    .flat_map(|step| &step.oracles)
                    .map(|oracle| CaseCheck {
                        oracle: oracle.clone(),
                        evaluation: if malformed {
                            OracleEvaluation { verdict: Verdict::Skipped, observed: None, reason: None, schema_location: None }
                        } else {
                            evaluate_check(oracle, &context)
                        },
                        record_observation_on_pass: false,
                    })
                    .collect();
                if malformed {
                    checks.push(CaseCheck {
                        oracle: OracleDecl {
                            id: WIRE_HANDLED_RULE.into(),
                            oracle_type: OracleType::Assert,
                            kind: None,
                            setup: false,
                            select: Some(ObservationSelector::Status),
                            when_condition: None,
                            expect: Some(ExpectCondition::Handled),
                            record_reference: None,
                        },
                        evaluation: wire_handled(result),
                        record_observation_on_pass: true,
                    });
                }
                // A schema-invalid value must be rejected by the step that received it.
                if let ExecutedPlan::Variant {
                    step_id,
                    replay: Some(variant),
                    ..
                } = &plan
                {
                    if step_id == &result.step_id && variant.expects_rejection() {
                        let oracle = OracleDecl {
                            id: SCHEMA_REJECTION_RULE.into(),
                            oracle_type: OracleType::Assert,
                            kind: None,
                            setup: false,
                            select: Some(ObservationSelector::Status),
                            when_condition: None,
                            expect: Some(ExpectCondition::Rejected),
                            record_reference: None,
                        };
                        checks.push(CaseCheck {
                            evaluation: evaluate_check(&oracle, &context),
                            record_observation_on_pass: true,
                            oracle,
                        });
                    }
                }
                let expectation = match &plan {
                    ExecutedPlan::Example { example_id, step_id, expect_status, .. } =>
                        Some((step_id, format!("example:{example_id}"), *expect_status)),
                    ExecutedPlan::Fuzz { step_id, key_expectation: Some((key, status)), .. } =>
                        Some((step_id, format!("fuzz:key:{key}"), *status)),
                    _ => None,
                };
                if let Some((_, id, status)) = expectation.filter(|(step, _, _)| *step == &result.step_id) {
                        let oracle = OracleDecl {
                            id,
                            oracle_type: OracleType::Assert,
                            kind: None,
                            setup: false,
                            select: Some(ObservationSelector::Status),
                            when_condition: None,
                            expect: Some(ExpectCondition::Equals(serde_json::Value::from(
                                status,
                            ))),
                            record_reference: None,
                        };
                        checks.push(CaseCheck {
                            evaluation: evaluate_check(&oracle, &context),
                            record_observation_on_pass: true,
                            oracle,
                        });
                }
                checks
            })
            .collect();
        Self {
            execution,
            plan,
            checks,
            process_exit: None,
        }
    }

    pub(crate) fn setup_failure(&self) -> Option<(usize, usize)> {
        if !matches!(self.plan, ExecutedPlan::Baseline) {
            return None;
        }
        self.checks.iter().enumerate().find_map(|(step, checks)| {
            checks
                .iter()
                .position(|check| check.oracle.setup && check.evaluation.verdict != Verdict::Pass)
                .map(|check| (step, check))
        })
    }

    pub(crate) fn is_incomplete(&self) -> bool {
        self.execution.step_results.iter().enumerate().any(|(index, step)| {
            // A malformed request that ended in a close was judged, not lost.
            let judged = self.checks.get(index).is_some_and(|checks| {
                checks.iter().any(|check| {
                    check.oracle.id == WIRE_HANDLED_RULE && check.evaluation.verdict != Verdict::Inconclusive
                })
            });
            !judged && (step.outcome != StepOutcome::Completed
                || step
                    .observation
                    .as_ref()
                    .is_none_or(|response| response.is_truncated()))
        }) || self
            .checks
            .iter()
            .flatten()
            .any(|check| check.evaluation.verdict == Verdict::Inconclusive)
    }

    pub(crate) fn failures(
        &self,
    ) -> impl Iterator<Item = (usize, usize, &super::ViolationCategory)> + '_ {
        self.checks
            .iter()
            .enumerate()
            .flat_map(move |(step, checks)| {
                checks
                    .iter()
                    .enumerate()
                    .filter_map(move |(check, result)| match &result.evaluation.verdict {
                        Verdict::Fail { category } => Some((step, check, category)),
                        _ => None,
                    })
            })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Passed,
    Failed,
    Incomplete,
}

impl RunStatus {
    pub(crate) fn classify(setup_failed: bool, has_findings: bool, incomplete: bool) -> Self {
        match (setup_failed, has_findings, incomplete) {
            (true, _, _) | (false, false, true) => Self::Incomplete,
            (false, true, _) => Self::Failed,
            (false, false, false) => Self::Passed,
        }
    }

    pub(crate) fn for_case(case: &EvaluatedCase) -> Self {
        Self::classify(
            case.setup_failure().is_some(),
            case.failures().next().is_some(),
            case.is_incomplete(),
        )
    }
}

/// `wire:handled`: a malformed request must get an answer without a 5xx, or
/// a closed connection, before the timeout. Accepting it (2xx/3xx) is
/// tolerated: HTTP/1.1 allows recipients to accept some forms.
fn wire_handled(result: &crate::execution::runner::StepResult) -> OracleEvaluation {
    use crate::adapters::http::transport::TransportReason;
    use crate::observation::SelectedValue;
    let evaluation = |verdict, observed: String| OracleEvaluation {
        verdict,
        observed: Some(SelectedValue::Present(serde_json::Value::String(observed))),
        reason: None,
        schema_location: None,
    };
    match (&result.outcome, result.observation.as_ref().and_then(|observation| observation.http_status())) {
        (StepOutcome::Completed, Some(status)) if (500..600).contains(&status) => evaluation(
            Verdict::Fail { category: super::ViolationCategory::ServerError },
            format!("HTTP {status}"),
        ),
        (StepOutcome::Completed, Some(status)) => evaluation(Verdict::Pass, format!("HTTP {status}")),
        (StepOutcome::TransportError { transport_reason: Some(TransportReason::ConnectionClosed), .. }, _) => {
            evaluation(Verdict::Pass, "connection closed".into())
        }
        (StepOutcome::TransportError { transport_reason: Some(TransportReason::Reset), .. }, _) => {
            evaluation(Verdict::Pass, "connection reset".into())
        }
        (StepOutcome::TransportError { transport_reason: Some(TransportReason::Timeout), .. }, _) => evaluation(
            Verdict::Fail { category: super::ViolationCategory::Unresponsive },
            "no answer and no close before the timeout".into(),
        ),
        (StepOutcome::TransportError { error, .. }, _) => evaluation(Verdict::Inconclusive, error.clone()),
        _ => OracleEvaluation { verdict: Verdict::Inconclusive, observed: None, reason: None, schema_location: None },
    }
}

#[cfg(test)]
mod tests {
    use super::RunStatus;

    #[test]
    fn classifies_combined_outcomes() {
        assert_eq!(RunStatus::classify(false, false, false), RunStatus::Passed);
        assert_eq!(
            RunStatus::classify(false, false, true),
            RunStatus::Incomplete
        );
        assert_eq!(RunStatus::classify(false, true, true), RunStatus::Failed);
        assert_eq!(RunStatus::classify(true, true, true), RunStatus::Incomplete);
    }

    #[test]
    fn wire_handled_accepts_answers_and_closes_and_flags_5xx_and_silence() {
        use super::{wire_handled, Verdict};
        use crate::evaluation::ViolationCategory;
        use crate::adapters::http::transport::{FailurePhase, HttpObservation, TransportReason};
        use crate::execution::runner::{StepOutcome, StepResult};
        use crate::observation::StepObservation;
        let answered = |status| StepResult {
            request_diagnostic: None,
            step_id: "axum".into(),
            outcome: StepOutcome::Completed,
            observation: Some(StepObservation::Http(HttpObservation {
                status,
                headers: Vec::new(),
                body: Vec::new(),
                truncated: false,
            })),
            extracted_outputs: Default::default(),
            sensitive_response: false,
        };
        let lost = |reason| StepResult {
            request_diagnostic: None,
            step_id: "axum".into(),
            outcome: StepOutcome::TransportError {
                error: "lost".into(),
                transport_reason: Some(reason),
                transport_phase: Some(FailurePhase::BeforeHeaders),
            },
            observation: None,
            extracted_outputs: Default::default(),
            sensitive_response: false,
        };
        assert_eq!(wire_handled(&answered(400)).verdict, Verdict::Pass);
        assert_eq!(wire_handled(&answered(200)).verdict, Verdict::Pass);
        assert_eq!(wire_handled(&lost(TransportReason::ConnectionClosed)).verdict, Verdict::Pass);
        assert_eq!(wire_handled(&lost(TransportReason::Reset)).verdict, Verdict::Pass);
        assert_eq!(
            wire_handled(&answered(502)).verdict,
            Verdict::Fail { category: ViolationCategory::ServerError }
        );
        assert_eq!(
            wire_handled(&lost(TransportReason::Timeout)).verdict,
            Verdict::Fail { category: ViolationCategory::Unresponsive }
        );
        assert_eq!(wire_handled(&lost(TransportReason::ConnectionFailed)).verdict, Verdict::Inconclusive);
    }
}
