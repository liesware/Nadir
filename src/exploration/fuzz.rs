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
use crate::exploration::enumerate::{collect_findings, record_process_exit};
use crate::finding::DedupMap;
use crate::mutation::fuzz::generate_case;

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
pub(crate) fn run_random_phase(
    runner: &mut WorkflowRunner,
    experiment: &Experiment,
    budget: &mut RunBudget,
    seed: u64,
    findings: &mut DedupMap,
    cases: &mut Vec<EvaluatedCase>,
) -> Result<(Option<RunStopReason>, FuzzSummary), RunError> {
    let contract = runner.http_contract().cloned();
    let mut summary = FuzzSummary {
        seed,
        executed: 0,
        kept: 0,
        incomplete: 0,
    };
    let mut kept_per_rule: HashMap<(String, String, String), u32> = HashMap::new();

    loop {
        if budget.remaining_time() == Some(Duration::ZERO) {
            return Ok((Some(RunStopReason::DeadlineExpired), summary));
        }
        if budget.execs_exhausted() {
            return Ok((Some(RunStopReason::MaxExecsReached), summary));
        }
        let Some(case) = generate_case(experiment, contract.as_ref(), seed, summary.executed)
        else {
            return Ok((None, summary));
        };
        let execution = runner.execute_replacements(&case.step_id, &case.replacements)?;
        budget.record_request();
        let evaluated = EvaluatedCase::new(
            experiment,
            execution,
            ExecutedPlan::Fuzz {
                step_id: case.step_id,
                replacements: case.replacements,
                seed,
                iteration: summary.executed,
            },
            &runner.schema_cache,
        );
        summary.executed += 1;
        collect_findings(findings, &evaluated, runner);

        if record_process_exit(findings, &evaluated, runner) {
            summary.kept += 1;
            cases.push(evaluated);
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
            if evaluated.is_incomplete() {
                summary.incomplete += 1;
            }
            continue;
        }
        let new_evidence = rules
            .iter()
            .any(|rule| kept_per_rule.get(rule).copied().unwrap_or(0) < KEPT_PER_RULE);
        if new_evidence && summary.kept < MAX_KEPT_CASES {
            for rule in rules {
                *kept_per_rule.entry(rule).or_default() += 1;
            }
            summary.kept += 1;
            cases.push(evaluated);
        }
    }
}

/// A seed for runs that do not set `exploration.seed`. Never zero.
pub(crate) fn random_seed() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos() as u64);
    (nanos ^ (u64::from(std::process::id()) << 32)).max(1)
}
