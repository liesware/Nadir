//! Exploration strategies: enumeration engine, campaign orchestration.

pub mod budget;
pub mod campaign;
pub mod enumerate;
pub mod fuzz;
pub mod libafl;

pub use budget::{RunBudget, RunStopReason};
#[allow(deprecated)]
pub use campaign::run_campaign;
pub use campaign::{CampaignCounters, CampaignPhases, CampaignResult};
pub use enumerate::{run_enumeration, DiscardReason, DiscardRecord, EnumerationResult};

#[derive(Clone, Default)]
pub(crate) struct RunProgress {
    pub deterministic_completed: u32,
    pub total_limit: Option<u64>,
    /// Enumerated cases the experiment declares (without the baseline).
    pub enumeration_planned: usize,
    /// How many of them `max_cases` lets run.
    pub enumeration_slots: usize,
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
