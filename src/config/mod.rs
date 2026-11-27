//! YAML v1 deserialization, static validation, and file loading.
//!
//! # Design notes
//!
//! **Chosen:** `load(path) -> Result<RawExperiment>` as the single entry point.
//! Deserialization, unknown-field rejection, and static validation are composed
//! inside this module. Callers never see `serde_yaml`.
//!
//! **Alternative considered:** exposing `RawExperiment` directly and requiring
//! callers to validate afterwards. Rejected because it leaks the validation
//! responsibility and risks callers forgetting to call `validate()`.

mod resolved;
mod types;
mod validate;

pub use resolved::{load_resolved, LoadError, ResolvedExperiment};
pub use types::*;
pub use validate::validate_raw_experiment;

use std::fs;
use std::path::Path;

/// Errors that can occur when loading a configuration file.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config file: {0}")]
    Io(#[from] std::io::Error),
    #[error("YAML parse error: {0}")]
    Parse(String),
    #[error("validation error: {0}")]
    Validation(String),
}

impl From<serde_yaml::Error> for ConfigError {
    fn from(e: serde_yaml::Error) -> Self {
        Self::Parse(e.to_string())
    }
}

/// Load and statically validate a Nadir experiment YAML file.
///
/// This is the single entry point for configuration loading. It:
/// 1. Reads the file
/// 2. Deserializes YAML into `RawExperiment` (rejecting unknown fields)
/// 3. Runs static validation
pub fn load(path: &Path) -> Result<RawExperiment, ConfigError> {
    parse(&fs::read_to_string(path)?)
}

pub(crate) fn parse(contents: &str) -> Result<RawExperiment, ConfigError> {
    let raw: RawExperiment = serde_yaml::from_str(contents)?;
    validate_raw_experiment(&raw).map_err(ConfigError::Validation)?;
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn examples_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples")
    }

    #[test]
    fn test_accepts_create_record() {
        let path = examples_dir().join("create-record/experiment.yaml");
        let result = load(&path);
        assert!(
            result.is_ok(),
            "failed to load create-record: {:?}",
            result.err()
        );
        let exp = result.unwrap();
        assert_eq!(exp.version, 1);
        assert_eq!(exp.experiment.name, "create-record");
        assert_eq!(exp.experiment.max_cases, 12);
        assert_eq!(exp.experiment.steps.len(), 4);
    }

    #[test]
    fn test_accepts_fuzz_probe() {
        let path = examples_dir().join("fuzz-probe/experiment.yaml");
        let result = load(&path);
        assert!(
            result.is_ok(),
            "failed to load fuzz-probe: {:?}",
            result.err()
        );
        let exp = result.unwrap();
        assert_eq!(exp.version, 1);
        assert_eq!(exp.experiment.name, "fuzz-probe");
    }
}
