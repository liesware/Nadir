//! LibAFL composition: input, executor, observers, feedbacks, objectives,
//! mutators, scheduler, stages, and monitor.
//! LibAFL types never cross this module's boundary.

pub mod executor;
pub mod feedback;
pub mod input;
pub mod mutators;
pub mod objectives;
pub mod observers;
pub mod scheduler;
pub mod stages;

pub use executor::{NadirExecutor, NadirExitKind};
pub use feedback::{build_case_signature, CaseSignature, SignatureFeedback};
pub use input::{case_plan_to_parts, parts_to_case_plan, InputConversionError};
pub use mutators::{build_mutation_stack, validate_mapped_composition, MutationConfig, MutationSetupError, MutationStack};
pub use objectives::{check_crash_objective, check_oracle_objectives, Objective};
pub use observers::{CaseResultObserver, NadirMapObserver};
pub use scheduler::{select_scheduler_mode, SchedulerMode};
pub use stages::{build_campaign_stages, CalibrationPolicy, CampaignStages};
