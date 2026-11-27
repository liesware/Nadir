//! Enumeration engine: execute baseline, validate setup checks, generate
//! variants, execute each non-discarded variant, enforce budgets.
//!
//! # Design notes
//!
//! **Chosen:** `run_enumeration()` orchestrates the full enumeration phase.
//! It returns `EnumerationResult` with baseline, cases, discards, findings,
//! and stop reason.

use crate::evaluation::case::{EvaluatedCase, ExecutedPlan, RunStatus};
use std::time::{Duration, Instant};

use crate::execution::runner::{RunError, StepOutcome, WorkflowRunner};
use crate::experiment::types::{Experiment, ExplorationStrategy, ReplacementValue};
use crate::exploration::fuzz::{random_seed, run_random_phase, FuzzSummary};
use crate::exploration::budget::{RunBudget, RunStopReason};
use crate::finding::{DedupMap, FindingKind, FindingOccurrence, GroupingKey};
use crate::mutation::{RegressionVariant, VariantGenerator};
use crate::target::ProcessStatus;

/// Result of the enumeration phase.
pub struct EnumerationResult {
    pub baseline_case: EvaluatedCase,
    pub cases: Vec<EvaluatedCase>,
    pub discards: Vec<DiscardRecord>,
    pub findings: DedupMap,
    pub stop_reason: RunStopReason,
    pub phase_completed: bool,
    /// The random phase of `strategy: fuzz`, when it ran.
    pub fuzz: Option<FuzzSummary>,
}

#[derive(Debug, Clone)]
pub struct DiscardRecord {
    pub reason: DiscardReason,
    pub plan_index: usize,
}

#[derive(Debug, Clone)]
pub enum DiscardReason {
    /// Candidate rejected before first mutated step was submitted.
    Encoding(String),
    /// Duplicate or unchanged value.
    Duplicate,
}

impl EnumerationResult {
    pub(crate) fn status(&self) -> RunStatus {
        RunStatus::classify(
            self.baseline_case.setup_failure().is_some(),
            !self.findings.is_empty() || self.stop_reason == RunStopReason::ProcessExit,
            !matches!(
                self.stop_reason,
                RunStopReason::EnumerationComplete
                    | RunStopReason::MaxCasesReached
                    | RunStopReason::DeadlineExpired
                    | RunStopReason::MaxExecsReached
            ) || self.baseline_case.is_incomplete()
                || self.cases.iter().any(EvaluatedCase::is_incomplete),
        )
    }
}

/// Run the enumeration phase for an experiment.
pub fn run_enumeration(
    runner: &mut WorkflowRunner,
    experiment: &Experiment,
    budget: &mut RunBudget,
) -> Result<EnumerationResult, RunError> {
    // 1. Execute the baseline case
    let baseline = EvaluatedCase::new(
        experiment,
        runner.execute_baseline()?,
        ExecutedPlan::Baseline,
        &runner.schema_cache,
    );
    budget.record_case();
    let mut findings = DedupMap::new();
    let setup_failed = baseline.setup_failure().is_some();
    if !setup_failed {
        collect_findings(&mut findings, &baseline, runner);
    }
    let baseline_stop = if setup_failed {
        Some(RunStopReason::SetupCheckFailed)
    } else if baseline.is_incomplete() {
        Some(RunStopReason::BaselineIncomplete)
    } else {
        None
    };
    if let Some(stop_reason) = baseline_stop {
        budget.stop(stop_reason.clone());
        return Ok(EnumerationResult {
            baseline_case: baseline,
            cases: Vec::new(),
            discards: Vec::new(),
            findings,
            stop_reason,
            phase_completed: false,
            fuzz: None,
        });
    }

    let plans = VariantGenerator::enumerate(experiment, runner.http_contract());
    let max_cases = experiment.max_cases.get() as usize;

    let mut cases = Vec::new();
    let discards = Vec::new();
    let mut stop_reason = if plans.len() > max_cases.saturating_sub(1) {
        RunStopReason::MaxCasesReached
    } else {
        RunStopReason::EnumerationComplete
    };
    let mut phase_completed = true;

    // 4. Execute each variant (up to max_cases including baseline)
    let available_slots = max_cases.saturating_sub(1); // baseline counts as 1
    let plan_limit = plans.len().min(available_slots);

    for plan in plans.iter().take(plan_limit) {
        if !budget.can_start_case() {
            stop_reason = budget
                .stop_reason
                .clone()
                .unwrap_or(RunStopReason::MaxCasesReached);
            phase_completed = false;
            break;
        }

        let Some((descriptor, value)) = plan
            .descriptors
            .iter()
            .zip(&plan.replacements)
            .find_map(|(descriptor, value)| Some((descriptor, value.as_ref()?)))
        else {
            continue;
        };
        let result = runner.execute_variant(
            &descriptor.step_id,
            &descriptor.canonical_selector,
            value.clone(),
        )?;
        let variant = RegressionVariant::from_replacement(experiment, descriptor, value);
        let result = EvaluatedCase::new(
            experiment,
            result,
            ExecutedPlan::Variant {
                step_id: descriptor.step_id.clone(),
                selector: descriptor.canonical_selector.clone(),
                replacement: value.clone(),
                replay: variant,
            },
            &runner.schema_cache,
        );
        budget.record_case();
        collect_findings(&mut findings, &result, runner);

        // A target that exits mid-case is a finding, and no later case can run.
        if record_process_exit(&mut findings, &result, runner) {
            stop_reason = RunStopReason::ProcessExit;
            phase_completed = false;
            cases.push(result);
            break;
        }

        cases.push(result);
    }

    // 5. Execute example cases (from cases.examples entries)
    // The budget is checked per example, so a budget spent exactly on the
    // variants does not report MaxCasesReached when no example remains.
    for step in &experiment.steps {
        for example in &step.examples {
            if !budget.can_start_case() {
                stop_reason = budget
                    .stop_reason
                    .clone()
                    .unwrap_or(RunStopReason::MaxCasesReached);
                phase_completed = false;
                break;
            }

            let selector = format!("body.{}", example.field);
            let value = ReplacementValue::JsonValue(example.value.clone());
            let exec_result = runner.execute_variant(&step.id, &selector, value.clone())?;

            let result = EvaluatedCase::new(
                experiment,
                exec_result,
                ExecutedPlan::Example {
                    example_id: example.id.clone(),
                    step_id: step.id.clone(),
                    selector: selector.clone(),
                    replacement: value.clone(),
                    expect_status: example.expect_status,
                },
                &runner.schema_cache,
            );
            budget.record_case();
            collect_findings(&mut findings, &result, runner);

            if record_process_exit(&mut findings, &result, runner) {
                stop_reason = RunStopReason::ProcessExit;
                phase_completed = false;
                cases.push(result);
                break;
            }

            cases.push(result);
        }
        if stop_reason == RunStopReason::ProcessExit || !phase_completed {
            break;
        }
    }

    // `strategy: fuzz` continues with random cases until its budget runs out,
    // unless the enumeration already stopped the run.
    let mut fuzz = None;
    if let ExplorationStrategy::Fuzz { seed, .. } = &experiment.exploration.strategy {
        // `max_cases` bounds the enumeration only; the random phase has its own budget.
        if matches!(
            stop_reason,
            RunStopReason::EnumerationComplete | RunStopReason::MaxCasesReached
        ) {
            let seed = seed.unwrap_or_else(random_seed);
            let (random_stop, summary) =
                run_random_phase(runner, experiment, budget, seed, &mut findings, &mut cases)?;
            if let Some(random_stop) = random_stop {
                stop_reason = random_stop;
            }
            fuzz = Some(summary);
        }
    }

    Ok(EnumerationResult {
        baseline_case: baseline,
        cases,
        discards,
        findings,
        stop_reason,
        phase_completed,
        fuzz,
    })
}

/// How long to wait for a target to finish exiting after a transport error.
/// A crashing server usually closes its socket a moment before it is reaped.
const PROCESS_EXIT_GRACE: Duration = Duration::from_millis(500);

/// If a step lost its connection because the monitored target exited, record
/// a process-exit finding and return true.
pub(crate) fn record_process_exit(
    findings: &mut DedupMap,
    case: &EvaluatedCase,
    runner: &mut WorkflowRunner,
) -> bool {
    let Some(failed_step) = case
        .execution
        .step_results
        .iter()
        .find(|step| matches!(step.outcome, StepOutcome::TransportError { .. }))
    else {
        return false;
    };
    let deadline = Instant::now() + PROCESS_EXIT_GRACE;
    let (exit_code, signal) = loop {
        match runner.process_status() {
            Some(ProcessStatus::Exited { code, signal }) => break (code, signal),
            Some(ProcessStatus::Running) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => return false,
        }
    };
    findings.insert(
        GroupingKey::ProcessExit {
            build_fingerprint: [0; 32],
            exit_code,
            signal,
        },
        FindingOccurrence {
            case_id: case.execution.case_id.clone(),
            step_id: failed_step.step_id.clone(),
            oracle_id: None,
            verdict: None,
        },
        None,
        None,
    );
    true
}

pub(crate) fn collect_findings(findings: &mut DedupMap, case: &EvaluatedCase, runner: &WorkflowRunner) {
    for (step, check, category) in case.failures() {
        let step_id = &case.execution.step_results[step].step_id;
        let check = &case.checks[step][check];
        let contract_fingerprint = runner
            .http_contract()
            .map_or([0; 32], |contract| contract.fingerprint);
        findings.insert(
            GroupingKey::OracleFailure {
                contract_fingerprint,
                kind: FindingKind::OracleFailure,
                step_id: step_id.clone(),
                oracle_id: check.oracle.id.clone(),
                violation_category: format!("{category:?}"),
            },
            FindingOccurrence {
                case_id: case.execution.case_id.clone(),
                step_id: step_id.clone(),
                oracle_id: Some(check.oracle.id.clone()),
                verdict: Some(check.evaluation.verdict.clone()),
            },
            None,
            None,
        );
    }
}
