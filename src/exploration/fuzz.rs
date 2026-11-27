//! The random phase of `strategy: fuzz`: generate and run random cases until
//! `duration_s` or `max_execs` runs out.
//!
//! # Design notes
//!
//! **Chosen:** only cases that fail a rule (or end the target process) are
//! kept as evidence, with a cap per finding and overall, so a long run does
//! not write thousands of identical case files. Cases that only lost their
//! response (timeouts, resets) are counted, not kept: without a rule failure
//! they are not findings, and they would otherwise mark the run incomplete.

use std::collections::HashMap;
use std::time::Duration;

use crate::evaluation::case::{EvaluatedCase, ExecutedPlan};
use crate::execution::runner::{RunError, WorkflowRunner};
use crate::experiment::types::Experiment;
use crate::exploration::budget::{RunBudget, RunStopReason};
use crate::exploration::enumerate::{collect_findings, record_process_exit, report_case, unless_stopped};
use crate::finding::DedupMap;
use crate::experiment::types::ExplorationStrategy;
use crate::mutation::fuzz::{generate_case_with, GeneratorVersion};

/// Most random cases kept as evidence in one run.
const MAX_KEPT_CASES: usize = 200;
/// Most random cases kept for the same failing rule.
const KEPT_PER_RULE: u32 = 5;

/// What the random phase did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuzzSummary {
    pub seed: u64,
    /// Random cases executed.
    pub executed: u64,
    /// Random cases kept as evidence (appended to the run's cases).
    pub kept: usize,
    /// Random cases without a usable response that failed no rule.
    pub incomplete: u64,
}

/// Run random cases, appending the kept ones to `cases`. Returns the reason
/// the phase stopped (`None` when the experiment has nothing to mutate).
// The phase shares the enumeration's state and reports through two callbacks.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_random_phase(
    runner: &mut WorkflowRunner,
    experiment: &Experiment,
    budget: &mut RunBudget,
    seed: u64,
    findings: &mut DedupMap,
    cases: &mut Vec<EvaluatedCase>,
    on_progress: &mut impl FnMut(&FuzzSummary, usize),
    on_case_evidence: &mut impl FnMut(&EvaluatedCase, u32, bool),
) -> Result<(Option<RunStopReason>, FuzzSummary), RunError> {
    let mut summary = FuzzSummary {
        seed,
        executed: 0,
        kept: 0,
        incomplete: 0,
    };
    on_progress(&summary, findings.len());
    let max_fields = match experiment.exploration.strategy {
        ExplorationStrategy::Fuzz { max_fields, .. } => max_fields,
        ExplorationStrategy::Enumerate => None,
    };
    let mut kept_per_rule: HashMap<(String, String, String), u32> = HashMap::new();

    loop {
        if budget.remaining_time() == Some(Duration::ZERO) {
            return Ok((Some(RunStopReason::DeadlineExpired), summary));
        }
        if budget.execs_exhausted() {
            return Ok((Some(RunStopReason::MaxExecsReached), summary));
        }
        let Some(case) = generate_case_with(
            experiment, |step, selector| runner.baseline_value(step, selector), seed, summary.executed,
            GeneratorVersion::CURRENT, max_fields,
        )
        else {
            return Ok((None, summary));
        };
        let Some(execution) = unless_stopped(runner.execute_fuzz_case(&case))? else {
            return Ok((Some(RunStopReason::StoppedByUser), summary));
        };
        budget.record_request();
        let key_expectation = case.key_rename.as_ref().and_then(|rename| rename.expectation());
        let mut evaluated = EvaluatedCase::new(
            experiment,
            execution,
            ExecutedPlan::Fuzz {
                step_id: case.step_id,
                replacements: case.replacements,
                seed,
                iteration: summary.executed,
                generator: GeneratorVersion::CURRENT.number(),
                max_fields,
                key_expectation,
            },
            &runner.schema_cache,
        );
        summary.executed += 1;
        collect_findings(findings, &evaluated, runner);

        if let Some(diagnostic) = record_process_exit(findings, &evaluated, runner) {
            evaluated.process_exit = Some(diagnostic);
            summary.kept += 1;
            report_case(on_case_evidence, &evaluated, cases.len() as u32 + 1, true);
            cases.push(evaluated);
            on_progress(&summary, findings.len());
            return Ok((Some(RunStopReason::ProcessExit), summary));
        }

        let rules: Vec<(String, String, String)> = evaluated
            .failures()
            .map(|(step, check, category)| {
                (
                    evaluated.execution.step_results[step].step_id.clone(),
                    evaluated.checks[step][check].oracle.id.clone(),
                    format!("{category:?}"),
                )
            })
            .collect();
        if rules.is_empty() {
            report_case(on_case_evidence, &evaluated, cases.len() as u32 + 1, false);
            if evaluated.is_incomplete() {
                summary.incomplete += 1;
            }
            on_progress(&summary, findings.len());
            continue;
        }
        let new_evidence = rules
            .iter()
            .any(|rule| kept_per_rule.get(rule).copied().unwrap_or(0) < KEPT_PER_RULE);
        let retain = new_evidence && summary.kept < MAX_KEPT_CASES;
        report_case(on_case_evidence, &evaluated, cases.len() as u32 + 1, retain);
        if retain {
            for rule in rules {
                *kept_per_rule.entry(rule).or_default() += 1;
            }
            summary.kept += 1;
            cases.push(evaluated);
        }
        on_progress(&summary, findings.len());
    }
}

/// A seed for runs that do not set `exploration.seed`. Never zero.
pub(crate) fn random_seed() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos() as u64);
    (nanos ^ (u64::from(std::process::id()) << 32)).max(1)
}
