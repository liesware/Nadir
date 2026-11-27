//! LibAFL observers: coverage map snapshot + case result carrier.
//!
//! The `NadirMapObserver` owns a snapshot buffer filled after each case.
//! The `CaseResultObserver` carries the structured result for feedback evaluation.

use crate::execution::runner::CaseResult;

/// Observer holding a snapshot of the coverage map.
pub struct NadirMapObserver {
    /// Snapshot buffer, filled by the executor after each case.
    pub snapshot: Vec<u8>,
}

impl NadirMapObserver {
    pub fn new(map_size: usize) -> Self {
        Self {
            snapshot: vec![0u8; map_size],
        }
    }

    /// Update the snapshot from a coverage map copy.
    pub fn update(&mut self, data: &[u8]) {
        let len = data.len().min(self.snapshot.len());
        self.snapshot[..len].copy_from_slice(&data[..len]);
    }
}

/// Observer carrying the structured case result for feedback.
#[derive(Default)]
pub struct CaseResultObserver {
    pub last_result: Option<CaseResult>,
}

impl CaseResultObserver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, result: CaseResult) {
        self.last_result = Some(result);
    }
}
