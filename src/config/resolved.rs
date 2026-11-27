//! Prepare static workflow configuration and provenance from the same files.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};

use super::{parse, ConfigError};
use crate::adapters::http::contract::{self, ResolvedContract};
use crate::experiment::{resolve_experiment, types::Experiment, ResolutionError};

/// A validated experiment and the contracts needed to prepare its runner.
pub struct ResolvedExperiment {
    pub experiment: Arc<Experiment>,
    pub contracts: Arc<HashMap<String, ResolvedContract>>,
    pub warnings: Vec<String>,
    pub(crate) directory: PathBuf,
    pub(crate) sha256: String,
    pub(crate) openapi_fingerprints: BTreeMap<String, String>,
    pub(crate) schema_cache: Arc<crate::evaluation::schema_validator::SchemaValidatorCache>,
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error(transparent)]
    Schema(#[from] crate::evaluation::schema_validator::SchemaPreparationError),
    #[error("loading experiment {path}: {source}")]
    Experiment { path: PathBuf, source: ConfigError },
    #[error("loading OpenAPI contract {path}: {source}")]
    Contract {
        path: PathBuf,
        source: contract::ContractError,
    },
    #[error("resolution error: {0}")]
    Resolution(#[from] ResolutionError),
}

/// Resolve static configuration without reading runtime credentials or contacting the service.
pub fn load_resolved(path: &Path) -> Result<ResolvedExperiment, LoadError> {
    let configuration_error = |source| LoadError::Experiment {
        path: path.to_owned(),
        source,
    };
    let source = fs::read_to_string(path).map_err(|error| configuration_error(error.into()))?;
    let sha256 = format!("{:x}", Sha256::digest(source.as_bytes()));
    let raw = parse(&source).map_err(configuration_error)?;
    let warnings = raw.experiment.steps.iter()
        .filter(|step| step.cases.as_ref().is_some_and(|cases| {
            cases.mutators.iter().any(|mutator| mutator.mode.as_deref() == Some("schema_valid"))
        }))
        .map(|step| format!("step '{}': schema_valid is an alias for edge_cases; request schemas are not validated", step.id))
        .collect();

    let directory = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut contracts = HashMap::new();
    if let Some(http) = &raw.experiment.protocols.http {
        let path = directory.join(&http.openapi);
        let contract = contract::load_contract(&path)
            .map_err(|source| LoadError::Contract { path, source })?;
        contracts.insert(http.openapi.clone(), contract);
    }
    let experiment = resolve_experiment(raw, &contracts, directory)?;
    let schema_cache =
        crate::evaluation::schema_validator::build_schema_cache(&experiment, &contracts)?;
    let openapi_fingerprints = contract_fingerprints(directory, &contracts);
    Ok(ResolvedExperiment {
        experiment: Arc::new(experiment),
        contracts: Arc::new(contracts),
        warnings,
        directory: directory.to_owned(),
        sha256,
        openapi_fingerprints,
        schema_cache: Arc::new(schema_cache),
    })
}

fn contract_fingerprints(
    directory: &Path,
    contracts: &HashMap<String, ResolvedContract>,
) -> BTreeMap<String, String> {
    let directory = absolute_normalized(directory);
    let mut fingerprints = BTreeMap::new();
    for (path, contract) in contracts {
        fingerprints.insert(
            portable_path(&absolute_normalized(&directory.join(path)), &directory),
            hex_digest(&contract.fingerprint),
        );
        for file in &contract.referenced_files {
            fingerprints.insert(
                portable_path(&absolute_normalized(&file.path), &directory),
                hex_digest(&file.fingerprint),
            );
        }
    }
    fingerprints
}

fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn absolute_normalized(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_owned());
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        if component == std::path::Component::ParentDir {
            if normalized.parent().is_some() {
                normalized.pop();
            }
        } else {
            normalized.push(component);
        }
    }
    normalized
}

fn portable_path(path: &Path, directory: &Path) -> String {
    let mut prefix = PathBuf::new();
    for (base, target) in directory.components().zip(path.components()) {
        if base != target {
            break;
        }
        prefix.push(base);
    }
    let mut relative = PathBuf::new();
    for _ in directory
        .strip_prefix(&prefix)
        .expect("shared prefix")
        .components()
    {
        relative.push("..");
    }
    relative.push(path.strip_prefix(&prefix).expect("shared prefix"));
    relative.to_string_lossy().into_owned()
}
