//! LibAFL scheduler: QueueScheduler (no coverage) or
//! IndexesLenTimeMinimizerScheduler over StdWeightedScheduler (with coverage).

/// Build a scheduler appropriate for the current run mode.
///
/// Returns a scheduler description; the actual LibAFL types are
/// instantiated in the campaign loop.
pub enum SchedulerMode {
    /// Simple FIFO queue (enumeration or no-coverage mode).
    Queue,
    /// Weighted with FAST power schedule (coverage-guided mode).
    WeightedWithMinimizer,
}

pub fn select_scheduler_mode(has_coverage: bool) -> SchedulerMode {
    if has_coverage {
        SchedulerMode::WeightedWithMinimizer
    } else {
        SchedulerMode::Queue
    }
}
