use std::{fmt, io};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProcessDiagnostic {
    SpawnError {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        os_error: Option<i32>,
    },
    Signaled {
        signal: i32,
    },
    StatusError {
        operation: ProcessStatusOperation,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        os_error: Option<i32>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessStatusOperation {
    TryWait,
    Wait,
}

impl ProcessDiagnostic {
    pub(crate) fn message_mut(&mut self) -> Option<&mut String> {
        match self {
            Self::SpawnError { message, .. } | Self::StatusError { message, .. } => Some(message),
            Self::Signaled { .. } => None,
        }
    }

    pub(super) fn status_error(operation: ProcessStatusOperation, error: io::Error) -> Self {
        Self::StatusError {
            operation,
            message: error.to_string(),
            os_error: error.raw_os_error(),
        }
    }
}

impl fmt::Display for ProcessDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SpawnError { message, .. } => f.write_str(message),
            Self::Signaled { signal } => write!(f, "terminated by signal {signal}"),
            Self::StatusError {
                operation, message, ..
            } => {
                let operation = match operation {
                    ProcessStatusOperation::TryWait => "try_wait",
                    ProcessStatusOperation::Wait => "wait",
                };
                write!(
                    f,
                    "could not observe process status ({operation}): {message}"
                )
            }
        }
    }
}
