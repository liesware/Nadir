//! Process monitor trait for observing target process status.
//!
//! # Design notes
//!
//! **Chosen:** `ProcessMonitor` trait with a single `status()` method.
//! `ManagedTarget` implements it. A library caller can supply a custom
//! monitor for services that Nadir does not manage.

use crate::target::supervisor::ProcessStatus;

/// Observable status of a target process.
pub trait ProcessMonitor {
    /// Query the current status of the process.
    fn status(&mut self) -> ProcessStatus;
}
