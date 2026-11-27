//! LibAFL executor: calls Nadir's `WorkflowRunner` for each execution.
//!
//! Maps case results to LibAFL's `ExitKind`: Crash for unexpected exits,
//! Timeout for timed-out steps, Ok for everything else.

use crate::execution::runner::{CaseResult, WorkflowRunner};
use crate::experiment::types::Experiment;
use crate::mutation::PartDescriptor;
use crate::target::ProcessMonitor;

/// The Nadir executor wraps the workflow runner for LibAFL.
///
/// In the MVP single-threaded mode, the executor borrows the runner mutably.
/// For multi-core (later milestone), this would use `Arc<Mutex<…>>`.
pub struct NadirExecutor<'a> {
    pub runner: &'a mut WorkflowRunner,
    pub experiment: &'a Experiment,
    pub descriptors: &'a [PartDescriptor],
    pub monitor: Option<&'a mut dyn ProcessMonitor>,
}

/// Maps LibAFL's execution result categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NadirExitKind {
    /// Normal execution completed.
    Ok,
    /// Target process exited unexpectedly.
    Crash,
    /// A step timed out.
    Timeout,
}

impl<'a> NadirExecutor<'a> {
    /// Execute one case through the workflow runner.
    ///
    /// Returns the case result and an exit kind for LibAFL.
    pub fn run_target(
        &mut self,
        replacements: &[(String, crate::experiment::types::ReplacementValue)],
    ) -> (CaseResult, NadirExitKind) {
        let result = match self.runner.execute_case(replacements) {
            Ok(r) => r,
            Err(_e) => {
                // Infrastructure error — create a minimal result
                return (
                    CaseResult {
                        case_id: String::new(),
                        step_results: Vec::new(),
                        was_executed: false,
                        requests_sent: 0,
                    },
                    NadirExitKind::Crash,
                );
            }
        };

        // Check process status
        let exit_kind = if let Some(ref mut monitor) = self.monitor {
            match monitor.status() {
                crate::target::ProcessStatus::Exited {
                    code: Some(c), ..
                } if c != 0 => NadirExitKind::Crash,
                crate::target::ProcessStatus::Exited {
                    signal: Some(_), ..
                } => NadirExitKind::Crash,
                _ => {
                    // Check for transport errors indicating timeout
                    let has_timeout = result.step_results.iter().any(|sr| {
                        matches!(
                            sr.outcome,
                            crate::execution::runner::StepOutcome::TransportError { .. }
                        )
                    });
                    if has_timeout {
                        NadirExitKind::Timeout
                    } else {
                        NadirExitKind::Ok
                    }
                }
            }
        } else {
            NadirExitKind::Ok
        };

        (result, exit_kind)
    }

    /// Execute the baseline case.
    pub fn run_baseline(&mut self) -> (CaseResult, NadirExitKind) {
        let result = match self.runner.execute_baseline() {
            Ok(r) => r,
            Err(_) => {
                return (
                    CaseResult {
                        case_id: String::new(),
                        step_results: Vec::new(),
                        was_executed: false,
                        requests_sent: 0,
                    },
                    NadirExitKind::Crash,
                );
            }
        };
        (result, NadirExitKind::Ok)
    }
}
