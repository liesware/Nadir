//! Campaign orchestration: full fuzz loop wiring enumeration, mutation,
//! oracles, findings, and budgets into a single campaign.

use std::path::Path;

use crate::execution::runner::{CaseResult, RunError, WorkflowRunner};
use crate::experiment::types::Experiment;
use crate::exploration::budget::{RunBudget, RunStopReason};
use crate::exploration::enumerate;
use crate::finding::DedupMap;

/// Result of a full campaign run.
pub struct CampaignResult {
    pub baseline: Option<CaseResult>,
    pub enumeration_cases: Vec<CaseResult>,
    pub campaign_cases: Vec<CaseResult>,
    pub findings: DedupMap,
    pub run_counters: CampaignCounters,
    pub stop_reason: RunStopReason,
    pub phases: CampaignPhases,
}

#[derive(Debug, Clone)]
pub struct CampaignCounters {
    pub cases_executed: u32,
    pub enumeration_variants: usize,
    pub campaign_execs: u32,
    pub findings_count: usize,
}

#[derive(Debug, Clone)]
pub struct CampaignPhases {
    pub enumeration_completed: bool,
    pub campaign_started: bool,
    pub campaign_completed: bool,
}

/// Run a full campaign: enumeration + optional fuzz phase.
pub fn run_campaign(
    runner: &mut WorkflowRunner,
    experiment: &Experiment,
    mut budget: RunBudget,
    _corpus_import: Option<&Path>,
) -> Result<CampaignResult, RunError> {
    // Phase 1: Enumeration
    let enum_result = enumerate::run_enumeration(runner, experiment, &mut budget)?;

    let findings = enum_result.findings;
    let findings_count = findings.len();
    let enumeration_variants = enum_result.cases.len();
    let campaign_cases = Vec::new();
    let mut campaign_execs: u32 = 0;

    // Phase 2: Fuzz campaign (only if strategy is Fuzz)
    let campaign_started = matches!(
        experiment.exploration.strategy,
        crate::experiment::types::ExplorationStrategy::Fuzz { .. }
    );

    if campaign_started && budget.can_start_case() {
        // In full Phase 5e: run the LibAFL fuzz loop
        // For now: just note that campaign would run here
        campaign_execs = 0;
    }

    let stop_reason = budget
        .stop_reason
        .clone()
        .unwrap_or(enum_result.stop_reason);

    Ok(CampaignResult {
        baseline: Some(enum_result.baseline_case.execution),
        enumeration_cases: enum_result
            .cases
            .into_iter()
            .map(|case| case.execution)
            .collect(),
        campaign_cases,
        findings,
        run_counters: CampaignCounters {
            cases_executed: budget.cases_executed(),
            enumeration_variants,
            campaign_execs,
            findings_count,
        },
        stop_reason,
        phases: CampaignPhases {
            enumeration_completed: enum_result.phase_completed,
            campaign_started,
            campaign_completed: false,
        },
    })
}
