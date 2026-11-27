//! Managed target process supervision, readiness polling, and lifecycle.
//!
//! # Design notes
//!
//! **Chosen:** `ManagedTarget` encapsulates all process management: spawn,
//! readiness polling, stderr capture, and SIGTERM/SIGKILL shutdown. The
//! `ProcessMonitor` trait allows library callers to supply custom monitors.

pub mod launch;
pub mod monitor;
pub mod supervisor;

pub use launch::{launch, LaunchError, RunningTarget};
pub use monitor::ProcessMonitor;
pub use supervisor::{ManagedTarget, ProcessStatus, StartupError, StopError};

use crate::target::supervisor::ProcessStatus as Ps;

impl ProcessMonitor for ManagedTarget {
    fn status(&mut self) -> Ps {
        self.is_running()
    }
}
