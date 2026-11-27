use serde::Serialize;

use crate::adapters::http::transport::{FailurePhase, TransportReason};
use crate::evaluation::case::EvaluatedCase;
use crate::evaluation::Verdict;
use crate::execution::runner::StepOutcome;
use crate::experiment::types::ExpectCondition;

#[derive(Debug, Serialize)]
pub struct RequestDiagnostic {
    pub(crate) method: &'static str,
    pub(crate) path_template: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) content_type: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) body_size_bytes: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) body_shape: Option<BodyShape>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BodyShape {
    Object,
    Array,
    String,
    Number,
    Boolean,
    Null,
}

#[derive(Debug, Serialize)]
pub(crate) struct ExecutionDiagnostic<'a> {
    pub step_id: &'a str,
    pub request: Option<&'a RequestDiagnostic>,
    pub outcome: ExecutionOutcome,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum ExecutionOutcome {
    ProcessExit {
        exit_code: Option<i32>,
        signal: Option<i32>,
    },
    TransportError {
        reason: Option<TransportReason>,
        phase: Option<FailurePhase>,
    },
}

impl ExecutionOutcome {
    pub(crate) fn kind(self) -> &'static str {
        match self {
            Self::ProcessExit { .. } => "process_exit",
            Self::TransportError { .. } => "transport_error",
        }
    }
}

pub(crate) fn transport_failure(
    outcome: &StepOutcome,
) -> Option<(Option<TransportReason>, Option<FailurePhase>)> {
    match outcome {
        StepOutcome::TransportError {
            transport_reason: Some(TransportReason::InvalidUrl | TransportReason::NonLoopback),
            ..
        } => None,
        StepOutcome::TransportError {
            transport_reason,
            transport_phase,
            ..
        } => Some((*transport_reason, *transport_phase)),
        _ => None,
    }
}

pub(crate) fn for_case(case: &EvaluatedCase) -> impl Iterator<Item = ExecutionDiagnostic<'_>> {
    let exit = case.process_exit.as_ref().map(|exit| ExecutionDiagnostic {
        step_id: &exit.step_id,
        request: case
            .execution
            .step_results
            .iter()
            .find(|step| step.step_id == exit.step_id)
            .and_then(|step| step.request_diagnostic.as_ref()),
        outcome: ExecutionOutcome::ProcessExit {
            exit_code: exit.exit_code,
            signal: exit.signal,
        },
    });
    let transport = case
        .execution
        .step_results
        .iter()
        .enumerate()
        .filter(|_| case.process_exit.is_none())
        .filter_map(|(index, step)| {
            let (reason, phase) = transport_failure(&step.outcome)?;
            if expected_disconnect(case, index) {
                return None;
            }
            Some(ExecutionDiagnostic {
                step_id: &step.step_id,
                request: step.request_diagnostic.as_ref(),
                outcome: ExecutionOutcome::TransportError { reason, phase },
            })
        });
    exit.into_iter().chain(transport)
}

pub(crate) fn expected_disconnect(case: &EvaluatedCase, index: usize) -> bool {
    if case.process_exit.is_some() {
        return false;
    }
    let failure = case
        .execution
        .step_results
        .get(index)
        .and_then(|step| transport_failure(&step.outcome));
    matches!(
        failure,
        Some((
            Some(TransportReason::ConnectionClosed | TransportReason::Reset),
            Some(FailurePhase::BeforeHeaders)
        ))
    ) && case.checks.get(index).is_some_and(|checks| {
        checks.iter().any(|check| {
            matches!(check.oracle.expect, Some(ExpectCondition::Handled))
                && check.evaluation.verdict == Verdict::Pass
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::ProcessExitDiagnostic;
    use crate::evaluation::case::{CaseCheck, ExecutedPlan};
    use crate::evaluation::OracleEvaluation;
    use crate::execution::runner::{CaseResult, StepResult};
    use crate::experiment::types::{OracleDecl, OracleType};

    fn closed_connection() -> EvaluatedCase {
        EvaluatedCase {
            execution: CaseResult {
                case_id: "test".into(),
                was_executed: true,
                requests_sent: 1,
                step_results: vec![StepResult {
                    step_id: "example".into(),
                    request_diagnostic: None,
                    outcome: StepOutcome::TransportError {
                        error: "connection closed".into(),
                        transport_reason: Some(TransportReason::ConnectionClosed),
                        transport_phase: Some(FailurePhase::BeforeHeaders),
                    },
                    observation: None,
                    extracted_outputs: Default::default(),
                    sensitive_response: false,
                }],
            },
            plan: ExecutedPlan::Baseline,
            checks: vec![vec![CaseCheck {
                oracle: OracleDecl {
                    id: "handled".into(),
                    oracle_type: OracleType::Assert,
                    kind: None,
                    setup: false,
                    select: None,
                    when_condition: None,
                    expect: Some(ExpectCondition::Handled),
                    record_reference: None,
                },
                evaluation: OracleEvaluation {
                    verdict: Verdict::Pass,
                    observed: None,
                    reason: None,
                    schema_location: None,
                },
                record_observation_on_pass: true,
            }]],
            process_exit: None,
        }
    }

    #[test]
    fn an_accepted_disconnect_is_not_saved_as_a_transport_error() {
        let mut case = closed_connection();
        assert!(expected_disconnect(&case, 0));
        assert!(for_case(&case).next().is_none());

        case.checks[0].clear();
        assert!(matches!(
            for_case(&case).next().unwrap().outcome,
            ExecutionOutcome::TransportError { .. },
        ));
    }

    #[test]
    fn a_confirmed_exit_is_retained_even_if_the_step_accepted_the_disconnect() {
        let mut case = closed_connection();
        case.process_exit = Some(ProcessExitDiagnostic {
            step_id: "example".into(),
            exit_code: Some(7),
            signal: None,
        });
        assert!(!expected_disconnect(&case, 0));
        let incidents: Vec<_> = for_case(&case).collect();
        assert_eq!(incidents.len(), 1);
        assert!(matches!(
            incidents[0].outcome,
            ExecutionOutcome::ProcessExit {
                exit_code: Some(7),
                ..
            },
        ));
    }
}
