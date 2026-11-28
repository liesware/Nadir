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

use crate::execution::runner::{RunError, WorkflowRunner};
use crate::experiment::types::{Experiment, ExplorationStrategy};
use crate::exploration::fuzz::{random_seed, run_random_phase, FuzzSummary};
use crate::exploration::RunProgress;
use crate::exploration::budget::{RunBudget, RunStopReason};
use crate::finding::{DedupMap, FindingKind, FindingOccurrence, GroupingKey};
use crate::mutation::build_descriptors;
use crate::target::ProcessStatus;

mod plan;

pub(crate) use plan::envar_enumerated_cases;
use plan::DeterministicPlan;

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
    /// Enumerated cases the experiment declares (without the baseline).
    pub planned_cases: usize,
    /// `max_cases` stopped the enumeration before all of them ran (the
    /// random phase may still have run, and set `stop_reason`).
    pub capped_by_max_cases: bool,
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
        // A confirmed target exit is a finding; the run failed regardless of
        // setup failures or incomplete cases.
        if self.stop_reason == RunStopReason::ProcessExit {
            return RunStatus::Failed;
        }
        RunStatus::classify(
            self.baseline_case.setup_failure().is_some(),
            !self.findings.is_empty(),
            !matches!(
                self.stop_reason,
                RunStopReason::EnumerationComplete
                    | RunStopReason::MaxCasesReached
                    | RunStopReason::DeadlineExpired
                    | RunStopReason::MaxExecsReached
                    | RunStopReason::StoppedByUser
            ) || self.baseline_case.is_incomplete()
                || self.cases.iter().any(EvaluatedCase::is_incomplete)
                || self.fuzz.as_ref().is_some_and(|summary| summary.incomplete > 0),
        )
    }
}

/// Run the enumeration phase for an experiment.
pub fn run_enumeration(
    runner: &mut WorkflowRunner,
    experiment: &Experiment,
    budget: &mut RunBudget,
) -> Result<EnumerationResult, RunError> {
    run_with_progress(runner, experiment, budget, &mut |_| {}, &mut |_, _, _| {})
}

pub(crate) fn run_with_progress(
    runner: &mut WorkflowRunner,
    experiment: &Experiment,
    budget: &mut RunBudget,
    on_progress: &mut impl FnMut(&RunProgress),
    on_case_evidence: &mut impl FnMut(&EvaluatedCase, u32, bool),
) -> Result<EnumerationResult, RunError> {
    let mut progress = RunProgress::default();
    on_progress(&progress);
    // 1. Execute the baseline case
    // Stopping during the baseline leaves nothing to record: the caller reports it.
    let baseline = runner.execute_baseline()?;
    let mut baseline = EvaluatedCase::new(experiment, baseline, ExecutedPlan::Baseline, &runner.schema_cache);
    budget.record_case();
    let mut findings = DedupMap::new();
    let setup_failed = baseline.setup_failure().is_some();
    if !setup_failed {
        collect_findings(&mut findings, &baseline, runner);
    }
    // A confirmed exit takes precedence over setup-check or incomplete-baseline reasons.
    let process_exit_diag = record_process_exit(&mut findings, &baseline, runner);
    if let Some(diag) = &process_exit_diag {
        baseline.process_exit = Some(diag.clone());
    }
    report_case(on_case_evidence, &baseline, 0, true);
    progress.deterministic_completed = budget.cases_executed();
    progress.findings = findings.len();
    on_progress(&progress);
    let baseline_stop = if process_exit_diag.is_some() {
        Some(RunStopReason::ProcessExit)
    } else if setup_failed {
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
            planned_cases: 0,
            capped_by_max_cases: false,
        });
    }

    let plan = DeterministicPlan::new(experiment, |step, selector| {
        runner.baseline_value(step, selector)
    });
    let available_slots = usize::from(experiment.max_cases.get()).saturating_sub(1);
    let planned_cases = plan.planned_count();
    let selected_sequence_stop = if plan.variants_exceed_slots(available_slots) {
        RunStopReason::MaxCasesReached
    } else {
        RunStopReason::EnumerationComplete
    };

    if let ExplorationStrategy::Fuzz { max_execs, .. } = experiment.exploration.strategy {
        if !build_descriptors(experiment).is_empty()
            || experiment.steps.iter().any(|step| !step.key_mutators.is_empty())
            || experiment.steps.iter().flat_map(|step| &step.mutators)
                .any(|mutator| matches!(mutator, crate::experiment::types::MutatorDecl::Body { .. }))
        {
            progress.random_limit = Some(max_execs);
        }
    }
    progress.enumeration_planned = planned_cases;
    progress.enumeration_slots = available_slots;
    progress.total_limit = Some(
        1 + planned_cases.min(available_slots) as u64 + u64::from(progress.random_limit.unwrap_or(0)),
    );
    on_progress(&progress);

    let mut cases = Vec::new();
    let mut interruption = None;
    for planned in plan.into_cases(available_slots) {
        if !budget.can_start_case() {
            interruption = Some(if budget.remaining_time() == Some(Duration::ZERO) {
                RunStopReason::DeadlineExpired
            } else {
                budget.stop_reason.clone().unwrap_or(RunStopReason::MaxCasesReached)
            });
            break;
        }

        let Some(mut case) = unless_stopped(planned.execute(runner, experiment))? else {
            budget.stop(RunStopReason::StoppedByUser);
            interruption = Some(RunStopReason::StoppedByUser);
            break;
        };
        budget.record_case();
        collect_findings(&mut findings, &case, runner);
        case.process_exit = record_process_exit(&mut findings, &case, runner);
        let exited = case.process_exit.is_some();
        report_case(on_case_evidence, &case, cases.len() as u32 + 1, true);
        progress.deterministic_completed = budget.cases_executed();
        progress.findings = findings.len();
        on_progress(&progress);
        cases.push(case);

        if exited {
            budget.stop(RunStopReason::ProcessExit);
            interruption = Some(RunStopReason::ProcessExit);
            break;
        }
    }
    let phase_completed = interruption.is_none();
    let mut stop_reason = interruption.unwrap_or(selected_sequence_stop);

    // Before the random phase replaces it, note whether `max_cases` cut the
    // enumeration short.
    let capped_by_max_cases = matches!(stop_reason, RunStopReason::MaxCasesReached);
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
            let (random_stop, summary) = run_random_phase(
                runner,
                experiment,
                budget,
                seed,
                &mut findings,
                &mut cases,
                &mut |random, findings_count| {
                    if progress.random_limit.is_some() {
                        progress.random = Some(random.clone());
                        progress.findings = findings_count;
                        on_progress(&progress);
                    }
                },
                on_case_evidence,
            )?;
            if let Some(random_stop) = random_stop {
                stop_reason = random_stop;
            }
            fuzz = Some(summary);
        }
    }

    Ok(EnumerationResult {
        baseline_case: baseline,
        cases,
        discards: Vec::new(),
        findings,
        stop_reason,
        phase_completed,
        fuzz,
        planned_cases,
        capped_by_max_cases,
    })
}

pub(crate) fn report_case(
    on_case_evidence: &mut impl FnMut(&EvaluatedCase, u32, bool),
    case: &EvaluatedCase,
    number: u32,
    keep_oracle_evidence: bool,
) {
    let oracle_evidence = keep_oracle_evidence && case.failures().next().is_some();
    if oracle_evidence || crate::execution::diagnostic::for_case(case).next().is_some() {
        on_case_evidence(case, number, oracle_evidence);
    }
}

/// `Ok(None)` when the user stopped the run during this case: the case was
/// not completed and is discarded.
pub(crate) fn unless_stopped<T>(result: Result<T, RunError>) -> Result<Option<T>, RunError> {
    match result {
        Err(RunError::StoppedByUser) => Ok(None),
        other => other.map(Some),
    }
}

/// How long to wait for a target to finish exiting after a transport error.
/// A crashing server usually closes its socket a moment before it is reaped.
const PROCESS_EXIT_GRACE: Duration = Duration::from_millis(500);

/// If a step lost its connection because the monitored target exited, record
/// a process-exit finding and return the diagnostic data. Returns `None` when
/// the target did not exit or the exit could not be confirmed.
pub(crate) fn record_process_exit(
    findings: &mut DedupMap,
    case: &EvaluatedCase,
    runner: &mut WorkflowRunner,
) -> Option<crate::artifacts::ProcessExitDiagnostic> {
    let Some(failed_step) = case
        .execution
        .step_results
        .iter()
        .find(|step| crate::execution::diagnostic::transport_failure(&step.outcome).is_some())
    else {
        return None;
    };
    let deadline = Instant::now() + PROCESS_EXIT_GRACE;
    let (exit_code, signal) = loop {
        match runner.process_status() {
            Some(ProcessStatus::Exited { code, signal }) => break (code, signal),
            Some(ProcessStatus::Running) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => return None,
        }
    };
    let diagnostic = crate::artifacts::ProcessExitDiagnostic {
        step_id: failed_step.step_id.clone(),
        exit_code,
        signal,
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
    Some(diagnostic)
}

pub(crate) fn collect_findings(findings: &mut DedupMap, case: &EvaluatedCase, runner: &WorkflowRunner) {
    for (step, check, category) in case.failures() {
        let step_id = &case.execution.step_results[step].step_id;
        let check = &case.checks[step][check];
        let contract_fingerprint = runner.contract_fingerprint(step_id).unwrap_or([0; 32]);
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
