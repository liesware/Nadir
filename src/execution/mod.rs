//! Workflow execution: step sequencing, outcome tracking, case context.

pub mod runner;

pub use runner::{CaseResult, PrepareError, RunError, StepOutcome, StepResult, WorkflowRunner};
