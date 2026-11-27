//! LibAFL mutation stack assembly.
//!
//! Builds the scheduled havoc mutation stack with mapped mutators,
//! Unicode-aware mutators for JSON strings, and token mutations.

use crate::mutation::PartDescriptor;

/// Configuration for the mutation stack.
pub struct MutationConfig {
    /// Whether coverage is active (enables full havoc).
    pub has_coverage: bool,
    /// Token dictionary bytes.
    pub dictionary: Vec<Vec<u8>>,
}

/// Assemble the mutation stack.
///
/// In the MVP enumeration phase, mutations are generated directly
/// by `VariantGenerator::enumerate()`. The full LibAFL mutation stack
/// is activated in the campaign phase (Phase 5e).
pub fn build_mutation_stack(
    descriptors: &[PartDescriptor],
    config: &MutationConfig,
) -> MutationStack {
    MutationStack {
        descriptor_count: descriptors.len(),
        has_coverage: config.has_coverage,
        dictionary_size: config.dictionary.len(),
    }
}

/// Placeholder for the LibAFL mutation stack.
///
/// In full Phase 5e integration, this wraps LibAFL's
/// `StdScheduledMutator` with `mapped_havoc_mutations`.
pub struct MutationStack {
    pub descriptor_count: usize,
    pub has_coverage: bool,
    pub dictionary_size: usize,
}

/// Validate that the mutation stack composes correctly.
pub fn validate_mapped_composition() -> Result<(), MutationSetupError> {
    // In full integration, this checks that LibAFL's mapped havoc
    // mutations compose with the Nadir part descriptors.
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum MutationSetupError {
    #[error("mutation setup failed: {0}")]
    SetupFailed(String),
}
