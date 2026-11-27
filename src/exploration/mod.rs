//! Exploration strategies: enumeration engine, campaign orchestration.

pub mod budget;
pub mod campaign;
pub mod enumerate;
pub mod fuzz;
pub mod libafl;

pub use budget::{RunBudget, RunStopReason};
pub use campaign::{run_campaign, CampaignCounters, CampaignPhases, CampaignResult};
pub use enumerate::{run_enumeration, DiscardReason, DiscardRecord, EnumerationResult};

#[derive(Clone, Default)]
pub(crate) struct RunProgress {
    pub deterministic_completed: u32,
    pub total_limit: Option<u64>,
    pub random_limit: Option<u32>,
    pub random: Option<fuzz::FuzzSummary>,
    pub findings: usize,
}

impl RunProgress {
    pub(crate) fn completed(&self) -> u64 {
        u64::from(self.deterministic_completed)
            + self.random.as_ref().map_or(0, |random| random.executed)
    }
}
