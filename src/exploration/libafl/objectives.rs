//! LibAFL objectives: oracle failures and process exits.
//!
//! Maps failed oracles and confirmed process exits to LibAFL objectives.
//! The enumeration phase uses these to detect findings.

use crate::evaluation::Verdict;
use crate::execution::runner::CaseResult;
use crate::target::ProcessStatus;

/// An objective discovered during a case execution.
#[derive(Debug, Clone)]
pub enum Objective {
    /// One or more oracles failed.
    OracleFailure {
        step_id: String,
        oracle_id: String,
        verdict: Verdict,
    },
    /// The target process exited unexpectedly.
    ProcessExit {
        exit_code: Option<i32>,
        signal: Option<i32>,
    },
}

/// Check whether a case result contains any oracle failures.
pub fn check_oracle_objectives(
    case: &CaseResult,
    experiment: &crate::experiment::types::Experiment,
) -> Vec<Objective> {
    let mut objectives = Vec::new();

    for step in &experiment.steps {
        let step_result = match case.step_results.iter().find(|sr| sr.step_id == step.id) {
            Some(sr) => sr,
            None => continue,
        };

        for oracle in &step.oracles {
            // Evaluate the oracle
            use crate::evaluation::{evaluate_oracle, OracleContext};

            let ctx = OracleContext {
                current_step: step_result,
                previous_steps: &[],
                case_id: &case.case_id,
                step_outputs: &std::collections::HashMap::new(),
                operation_id: &step.operation_key,
                declared_statuses: &step.declared_statuses,
                schema_cache: &std::collections::HashMap::new(),
            };

            let verdict = evaluate_oracle(oracle, &ctx);

            if matches!(verdict, Verdict::Fail { .. }) {
                objectives.push(Objective::OracleFailure {
                    step_id: step.id.clone(),
                    oracle_id: oracle.id.clone(),
                    verdict,
                });
            }
        }
    }

    objectives
}

/// Check whether the target process exited unexpectedly.
///
/// Returns `Some(Objective)` if the process exited with a non-zero
/// exit code or a signal.
pub fn check_crash_objective(status: &ProcessStatus) -> Option<Objective> {
    match status {
        ProcessStatus::Exited { code, signal } => {
            if code.is_none_or(|c| c != 0) || signal.is_some() {
                Some(Objective::ProcessExit {
                    exit_code: *code,
                    signal: *signal,
                })
            } else {
                None
            }
        }
        _ => None,
    }
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_crash_objective_on_nonzero_exit() {
        let status = ProcessStatus::Exited {
            code: Some(1),
            signal: None,
        };
        let obj = check_crash_objective(&status);
        assert!(obj.is_some());
        match obj.unwrap() {
            Objective::ProcessExit {
                exit_code,
                signal,
            } => {
                assert_eq!(exit_code, Some(1));
                assert_eq!(signal, None);
            }
            _ => panic!("expected ProcessExit"),
        }
    }

    #[test]
    fn test_no_crash_on_zero_exit() {
        let status = ProcessStatus::Exited {
            code: Some(0),
            signal: None,
        };
        let obj = check_crash_objective(&status);
        assert!(obj.is_none());
    }

    #[test]
    fn test_crash_objective_on_signal() {
        let status = ProcessStatus::Exited {
            code: None,
            signal: Some(9), // SIGKILL
        };
        let obj = check_crash_objective(&status);
        assert!(obj.is_some());
    }
}
