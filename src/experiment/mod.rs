//! Validated experiment model: steps, bindings, oracles, selectors.
//!
//! # Design notes
//!
//! **Chosen:** resolved `Experiment` is a separate type from `RawExperiment`.
//! Resolution happens once during `experiment::resolve()`, producing an
//! immutable, validated `Experiment` that the rest of the system uses.

pub mod resolve;
pub mod selectors;
pub mod types;

pub use resolve::{resolve_experiment, ResolutionError};
pub use selectors::{InputSelector, ObservationSelector, Reference, ReferenceParseError};
pub use types::*;
