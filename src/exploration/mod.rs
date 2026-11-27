//! Exploration strategies: enumeration engine, campaign orchestration.

pub mod budget;
pub mod campaign;
pub mod enumerate;
pub mod fuzz;
pub mod libafl;

pub use budget::{RunBudget, RunStopReason};
pub use campaign::{run_campaign, CampaignCounters, CampaignPhases, CampaignResult};
pub use enumerate::{run_enumeration, DiscardReason, DiscardRecord, EnumerationResult};
