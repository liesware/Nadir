//! LibAFL campaign stages: calibration, mutational, minimization.

/// Calibration policy for the coverage map.
#[derive(Debug, Clone)]
pub struct CalibrationPolicy {
    /// Number of initial repeats per entry (default 8).
    pub initial_repeats: usize,
    /// Maximum repeats for unstable entries.
    pub max_repeats: usize,
    /// Fraction of map entries varying above this → unstable.
    pub unstable_threshold: f64,
}

impl Default for CalibrationPolicy {
    fn default() -> Self {
        Self {
            initial_repeats: 8,
            max_repeats: 64,
            unstable_threshold: 0.2,
        }
    }
}

/// Build the campaign stages.
///
/// Returns stage descriptors; LibAFL types instantiated in the campaign loop.
pub fn build_campaign_stages(
    has_coverage: bool,
    calibration: &CalibrationPolicy,
) -> CampaignStages {
    CampaignStages {
        has_coverage,
        calibration: calibration.clone(),
    }
}

pub struct CampaignStages {
    pub has_coverage: bool,
    pub calibration: CalibrationPolicy,
}
