//! Run budget: deadline, counters, and limit tracking.
//!
//! # Design notes
//!
//! **Chosen:** `RunBudget` is the single authority for deadline and counter
//! enforcement. No other module calls `Instant::now()` for budget purposes.

use std::time::{Duration, Instant};

/// Tracks resource consumption for a run.
#[derive(Debug, Clone)]
pub struct RunBudget {
    start: Instant,
    deadline: Option<Instant>,
    max_cases: u16,
    max_execs: Option<u32>,
    /// Cases executed so far (including baseline).
    cases_executed: u32,
    /// Total HTTP requests made.
    request_count: u32,
    /// Reason the run stopped.
    pub stop_reason: Option<RunStopReason>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunStopReason {
    /// All enumeration cases completed.
    EnumerationComplete,
    /// max_cases reached.
    MaxCasesReached,
    /// max_execs reached.
    MaxExecsReached,
    /// Deadline expired.
    DeadlineExpired,
    /// Target restart limit reached.
    RestartLimit,
    /// Setup check failed on baseline.
    SetupCheckFailed,
    /// The baseline could not complete.
    BaselineIncomplete,
    /// A process exit finding stopped the run.
    ProcessExit,
}

impl RunBudget {
    /// Create a new budget.
    pub fn new(
        max_cases: u16,
        max_execs: Option<u32>,
        duration: Option<Duration>,
    ) -> Self {
        let start = Instant::now();
        let deadline = duration.map(|d| start + d);
        Self {
            start,
            deadline,
            max_cases,
            max_execs,
            cases_executed: 0,
            request_count: 0,
            stop_reason: None,
        }
    }

    /// Check if a new case can be started.
    pub fn can_start_case(&self) -> bool {
        if self.stop_reason.is_some() {
            return false;
        }
        if self.cases_executed >= self.max_cases as u32 {
            return false;
        }
        if let Some(deadline) = self.deadline {
            if Instant::now() >= deadline {
                return false;
            }
        }
        true
    }

    /// Record that a case was executed.
    pub fn record_case(&mut self) {
        self.cases_executed += 1;
    }

    /// Record an HTTP request count increment.
    pub fn record_request(&mut self) {
        self.request_count += 1;
    }

    /// Remaining time before the deadline.
    pub fn remaining_time(&self) -> Option<Duration> {
        self.deadline.map(|d| {
            let now = Instant::now();
            if now >= d {
                Duration::ZERO
            } else {
                d - now
            }
        })
    }

    /// Cases executed.
    pub fn cases_executed(&self) -> u32 {
        self.cases_executed
    }

    /// Request count.
    pub fn request_count(&self) -> u32 {
        self.request_count
    }

    /// Check if max_execs has been reached.
    pub fn execs_exhausted(&self) -> bool {
        self.max_execs
            .map(|max| self.request_count >= max)
            .unwrap_or(false)
    }

    /// Mark the run as stopped with a reason.
    pub fn stop(&mut self, reason: RunStopReason) {
        self.stop_reason = Some(reason);
    }

    /// Elapsed time since start.
    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }
}
