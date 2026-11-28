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

use crate::execution::runner::{CaseResult, RunError, WorkflowRunner};
use crate::experiment::types::{Experiment, ExplorationStrategy};
use crate::exploration::fuzz::{random_seed, run_random_phase, FuzzSummary};
use crate::exploration::RunProgress;
use crate::exploration::budget::{RunBudget, RunStopReason};
use crate::finding::{DedupMap, FindingKind, FindingOccurrence, GroupingKey};
use crate::mutation::{build_descriptors, RegressionVariant, VariantGenerator};
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
        });
    }

    let plans = VariantGenerator::enumerate_with(experiment, |step, selector| runner.baseline_value(step, selector));
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
    let example_count: usize = experiment.steps.iter().map(|step| step.examples.len()).sum();
    let corpus_count: usize = experiment.steps.iter().flat_map(|step| &step.corpora)
        .map(|corpus| usize::from(corpus.repetitions))
        .fold(0usize, usize::saturating_add);
    let malform_cases = malform_cases(experiment);
    let envfile_cases = envfile_cases(experiment);
    let envar_enumerated_count = envar_enumerated_cases(experiment).len();
    if let ExplorationStrategy::Fuzz { max_execs, .. } = experiment.exploration.strategy {
        if !build_descriptors(experiment).is_empty()
            || experiment.steps.iter().any(|step| !step.key_mutators.is_empty())
        {
            progress.random_limit = Some(max_execs);
        }
    }
    progress.total_limit = Some(
        1 + (plans.len() + example_count)
            .saturating_add(corpus_count)
            .saturating_add(malform_cases.len())
            .saturating_add(envfile_cases.len())
            .saturating_add(envar_enumerated_count)
            .min(available_slots) as u64
            + u64::from(progress.random_limit.unwrap_or(0)),
    );
    on_progress(&progress);

    for plan in plans.iter().take(plan_limit) {
        let Some((descriptor, value)) = plan
            .descriptors
            .iter()
            .zip(&plan.replacements)
            .find_map(|(descriptor, value)| Some((descriptor, value.as_ref()?)))
        else {
            continue;
        };
        let variant = RegressionVariant::from_replacement(experiment, descriptor, value);
        let (should_break, reason) = try_next_case(
            runner,
            budget,
            |r| r.execute_variant(
                &descriptor.step_id,
                &descriptor.canonical_selector,
                value.clone(),
            ),
            |execution, schema_cache| EvaluatedCase::new(
                experiment,
                execution,
                ExecutedPlan::Variant {
                    step_id: descriptor.step_id.clone(),
                    selector: descriptor.canonical_selector.clone(),
                    replacement: value.clone(),
                    replay: variant,
                },
                schema_cache,
            ),
            &mut findings,
            &mut cases,
            &mut progress,
            &mut *on_progress,
            &mut *on_case_evidence,
        )?;
        if should_break {
            stop_reason = reason;
            phase_completed = false;
            break;
        }
    }

    // 5. Execute example cases (from cases.examples entries)
    // The budget is checked per example, so a budget spent exactly on the
    // variants does not report MaxCasesReached when no example remains.
    for step in &experiment.steps {
        for example in &step.examples {
            let (should_break, reason) = try_next_case(
                runner,
                budget,
                |r| r.execute_example(&step.id, example),
                |execution, schema_cache| EvaluatedCase::new(
                    experiment,
                    execution,
                    ExecutedPlan::Example {
                        example_id: example.id.clone(),
                        step_id: step.id.clone(),
                        expect_status: example.expect_status,
                        json_example: example.json_example().cloned(),
                    },
                    schema_cache,
                ),
                &mut findings,
                &mut cases,
                &mut progress,
                &mut *on_progress,
                &mut *on_case_evidence,
            )?;
            if should_break {
                stop_reason = reason;
                phase_completed = false;
                break;
            }
        }
        if !phase_completed {
            break;
        }
    }

    if phase_completed {
        let corpus_cases = experiment.steps.iter().flat_map(|step| {
            step.corpora.iter().enumerate().flat_map(move |(index, corpus)| {
                (0..corpus.repetitions).map(move |iteration| (step, corpus, index, iteration))
            })
        });
        for (step, corpus, index, iteration) in corpus_cases {
            let (reference, value) = corpus.case(&step.id, index, iteration);
            let selector = format!("body.{}", reference.field);
            let (should_break, reason) = try_next_case(
                runner,
                budget,
                |r| r.execute_variant(&step.id, &selector, value),
                |execution, schema_cache| EvaluatedCase::new(
                    experiment, execution, ExecutedPlan::Corpus(reference), schema_cache,
                ),
                &mut findings,
                &mut cases,
                &mut progress,
                &mut *on_progress,
                &mut *on_case_evidence,
            )?;
            if should_break {
                stop_reason = reason;
                phase_completed = false;
                break;
            }
        }
    }

    // `envfile`: one case per way to break each cli step's file.
    if phase_completed {
        for (step_id, name) in &envfile_cases {
            let (sid, sid2) = (step_id.clone(), step_id.clone());
            let (case, case2) = (name.clone(), name.clone());
            let (should_break, reason) = try_next_case(
                runner,
                budget,
                move |r| r.execute_envfile(&sid, &case),
                move |execution, schema_cache| EvaluatedCase::new(
                    experiment,
                    execution,
                    ExecutedPlan::EnvFile { step_id: sid2.clone(), name: case2.clone() },
                    schema_cache,
                ),
                &mut findings,
                &mut cases,
                &mut progress,
                &mut *on_progress,
                &mut *on_case_evidence,
            )?;
            if should_break {
                stop_reason = reason;
                phase_completed = false;
                break;
            }
        }
    }

    // `malform`: one case per malformation of each `http_raw` step.
    if phase_completed {
        for (step_id, malformation) in &malform_cases {
            let mal = *malformation;
            let sid = step_id.clone();
            let sid2 = sid.clone();
            let (should_break, reason) = try_next_case(
                runner,
                budget,
                move |r| r.execute_malform(&sid, mal),
                move |execution, schema_cache| EvaluatedCase::new(
                    experiment,
                    execution,
                    ExecutedPlan::Malform { step_id: sid2, malformation: mal },
                    schema_cache,
                ),
                &mut findings,
                &mut cases,
                &mut progress,
                &mut *on_progress,
                &mut *on_case_evidence,
            )?;
            if should_break {
                stop_reason = reason;
                phase_completed = false;
                break;
            }
        }
    }

    // envar: one case per fixed value of each variable with `value`/`values`.
    if phase_completed {
        let envar_cases = envar_enumerated_cases(experiment);
        for (step_id, var_name, case, value) in &envar_cases {
            let sid = step_id.clone();
            let vname = var_name.clone();
            let val = value.clone();
            let case = case.clone();
            let (should_break, reason) = try_next_case(
                runner,
                budget,
                |r| r.execute_variant(&sid, &vname, crate::experiment::types::envar_replacement(&val)),
                |execution, schema_cache| EvaluatedCase::new(
                    experiment,
                    execution,
                    crate::evaluation::case::ExecutedPlan::Envar {
                        step_id: sid.clone(),
                        variable: vname.clone(),
                        case: case.clone(),
                        value: val.clone(),
                    },
                    schema_cache,
                ),
                &mut findings,
                &mut cases,
                &mut progress,
                &mut *on_progress,
                &mut *on_case_evidence,
            )?;
            if should_break {
                stop_reason = reason;
                phase_completed = false;
                break;
            }
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
        discards,
        findings,
        stop_reason,
        phase_completed,
        fuzz,
    })
}

/// Attempt the next case in the run: gate against stop conditions, execute,
/// collect findings, and check for a target exit. Returns `(true, reason)`
/// when further cases must not start (the caller must break out of its loop);
/// `(false, _)` when the case completed and the loop should continue.
fn try_next_case<E, B>(
    runner: &mut WorkflowRunner,
    budget: &mut RunBudget,
    execute: E,
    build: B,
    findings: &mut DedupMap,
    cases: &mut Vec<EvaluatedCase>,
    progress: &mut RunProgress,
    on_progress: &mut impl FnMut(&RunProgress),
    on_case_evidence: &mut impl FnMut(&EvaluatedCase, u32, bool),
) -> Result<(bool, RunStopReason), RunError>
where
    E: FnOnce(&mut WorkflowRunner) -> Result<CaseResult, RunError>,
    B: FnOnce(CaseResult, &crate::evaluation::schema_validator::SchemaValidatorCache) -> EvaluatedCase,
{
    if !budget.can_start_case() {
        let reason = if budget.remaining_time() == Some(Duration::ZERO) {
            RunStopReason::DeadlineExpired
        } else {
            budget.stop_reason.clone().unwrap_or(RunStopReason::MaxCasesReached)
        };
        return Ok((true, reason));
    }

    let Some(execution) = unless_stopped(execute(runner))? else {
        budget.stop(RunStopReason::StoppedByUser);
        return Ok((true, RunStopReason::StoppedByUser));
    };

    let schema_cache = &runner.schema_cache;
    let mut case = build(execution, schema_cache);
    budget.record_case();
    collect_findings(findings, &case, runner);
    let diagnostic = record_process_exit(findings, &case, runner);
    let exited = diagnostic.is_some();
    case.process_exit = diagnostic;
    report_case(on_case_evidence, &case, cases.len() as u32 + 1, true);
    progress.deterministic_completed = budget.cases_executed();
    progress.findings = findings.len();
    on_progress(progress);
    cases.push(case);
    if exited {
        budget.stop(RunStopReason::ProcessExit);
        return Ok((true, RunStopReason::ProcessExit));
    }
    Ok((false, RunStopReason::EnumerationComplete))
}

/// Every enumerated `envar` case, `(step, variable, case, value)`: each
/// `value`/`values` entry, then each invalid value of each `mode: spec` rule.
pub(crate) fn envar_enumerated_cases(
    experiment: &Experiment,
) -> Vec<(String, String, crate::artifacts::EnvarCase, Option<String>)> {
    use crate::artifacts::EnvarCase;
    use crate::experiment::types::{EnvarVariableRule, MutatorDecl};
    let mut cases = Vec::new();
    for step in &experiment.steps {
        for mutator in &step.mutators {
            let MutatorDecl::Envar { variables, .. } = mutator else {
                continue;
            };
            for var in variables {
                let mut push = |case, value: Option<String>| cases.push((step.id.clone(), var.name.clone(), case, value));
                match &var.rule {
                    EnvarVariableRule::Fixed(value) => push(EnvarCase::Value(0), Some(value.clone())),
                    EnvarVariableRule::Values(values) => {
                        for (index, value) in values.iter().enumerate() {
                            push(EnvarCase::Value(index), Some(value.clone()));
                        }
                    }
                    EnvarVariableRule::Spec(rule) => {
                        for (name, value) in crate::adapters::cli::spec::cases_for(&var.name, rule, step.env_config.as_ref()) {
                            match value {
                                Some(value) => push(EnvarCase::Spec(name), Some(value)),
                                None => push(EnvarCase::Unset, None),
                            }
                        }
                    }
                    EnvarVariableRule::File { sha256, lines, .. } => {
                        for (index, value) in lines.iter().enumerate() {
                            push(EnvarCase::Line { line: index + 1, sha256: sha256.clone() }, Some(value.clone()));
                        }
                    }
                    EnvarVariableRule::Absent => push(EnvarCase::Unset, None),
                    EnvarVariableRule::Catalog { unconstrained } => {
                        for (name, value) in crate::mutation::envar_catalog(*unconstrained) {
                            push(EnvarCase::Catalog(name), Some(value));
                        }
                    }
                    // Generative modes are handled by the fuzz phase.
                    EnvarVariableRule::Mode(_) | EnvarVariableRule::Text(_) => {}
                }
            }
        }
    }
    cases
}

/// Every `(step, case name)` the experiment's `envfile` mutators run.
pub(crate) fn envfile_cases(experiment: &Experiment) -> Vec<(String, String)> {
    experiment
        .steps
        .iter()
        .flat_map(|step| {
            step.mutators.iter().flat_map(move |mutator| match mutator {
                crate::experiment::types::MutatorDecl::EnvFile { cases, .. } => {
                    cases.iter().map(|case| (step.id.clone(), case.name.clone())).collect()
                }
                _ => Vec::new(),
            })
        })
        .collect()
}

/// Every `(step, malformation)` the experiment's `malform` mutators run, in
/// step and catalog order.
fn malform_cases(experiment: &Experiment) -> Vec<(String, crate::adapters::http_raw::Malformation)> {
    experiment
        .steps
        .iter()
        .flat_map(|step| {
            step.mutators.iter().flat_map(move |mutator| match mutator {
                crate::experiment::types::MutatorDecl::Malform { malformations } => {
                    malformations.iter().map(|malformation| (step.id.clone(), *malformation)).collect()
                }
                _ => Vec::new(),
            })
        })
        .collect()
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
