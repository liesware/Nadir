//! On-disk artifact storage and corpus import/export.
//!
//! # Design notes
//!
//! **Chosen:** artifacts are stored under `.nadir/runs/<run-id>/` in the
//! experiment directory. Each case gets `cases/<NNNN>.json`, findings get
//! `findings/<NNNN>.json`, and the corpus uses SHA-256 filenames.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::artifacts::{FuzzCaseRef, FuzzRecord, 
    regression_record, CaseRecord, CorpusEntryRecord, FindingRecord, RegressionPlan, RunRecord,
};
use crate::config::ResolvedExperiment;
use crate::evaluation::case::{EvaluatedCase, ExecutedPlan};
use crate::execution::diagnostic::{ExecutionDiagnostic, ExecutionOutcome};
use crate::exploration::EnumerationResult;
use crate::finding::{DedupMap, GroupingKey};

/// Errors during storage operations.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{action} {path}: {source}")]
    Path {
        action: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("serializing {path}: {source}")]
    SerializeAt {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("serialization error: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("serializing {path}: {source}")]
    SerializeYaml {
        path: PathBuf,
        source: serde_yaml::Error,
    },
}

/// Generates a run ID in the format `YYYYMMDDTHHMMSSZ-XXXXXX`.
pub fn generate_run_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    // Simple format: timestamp + random suffix
    let suffix = (uuid::Uuid::new_v4().as_u128() & 0xff_ffff) as u32;
    // Use chrono-like formatting manually for zero deps
    let total_days = secs / 86400;
    let mut y = 1970u64;
    let mut d = total_days;
    loop {
        let days_in_year = if is_leap(y) { 366 } else { 365 };
        if d < days_in_year {
            break;
        }
        d -= days_in_year;
        y += 1;
    }
    let month_days = if is_leap(y) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut m = 0;
    while m < 12 && d >= month_days[m] {
        d -= month_days[m];
        m += 1;
    }
    let month = m + 1;
    let day = d + 1;
    let remaining = secs % 86400;
    let h = remaining / 3600;
    let min = (remaining % 3600) / 60;
    let s = remaining % 60;

    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z-{:06x}",
        y, month, day, h, min, s, suffix
    )
}

#[allow(clippy::manual_is_multiple_of)]
fn is_leap(y: u64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// A reserved destination for a run; reservation succeeds before any requests are sent.
pub(crate) struct RunDirectory {
    id: String,
    path: PathBuf,
    started: Instant,
}

pub(crate) struct WrittenRun {
    pub record: RunRecord,
    pub run_file: PathBuf,
    case_paths: Vec<PathBuf>,
}

impl WrittenRun {
    pub fn case_path(&self, number: u32) -> Option<&Path> {
        self.case_paths.get(number as usize).map(PathBuf::as_path)
    }
}

impl RunDirectory {
    pub fn reserve(experiment_directory: &Path) -> Result<Self, StorageError> {
        let root = experiment_directory.join(".nadir/runs");
        fs::create_dir_all(&root).map_err(|source| path_error("creating", &root, source))?;
        loop {
            let id = generate_run_id();
            let path = root.join(&id);
            match fs::create_dir(&path) {
                Ok(()) => {
                    let cases = path.join("cases");
                    fs::create_dir(&cases)
                        .map_err(|source| path_error("creating", &cases, source))?;
                    return Ok(Self {
                        id,
                        path,
                        started: Instant::now(),
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(source) => return Err(path_error("creating", &path, source)),
            }
        }
    }

    pub fn write(
        self,
        configuration: &ResolvedExperiment,
        result: &EnumerationResult,
        diagnostic_secrets: &[String],
        transport_errors: u64,
    ) -> Result<WrittenRun, StorageError> {
        let mut record = RunRecord::new(
            configuration.experiment.name.clone(),
            result.stop_reason.clone(),
            self.started.elapsed().as_millis() as u64,
        );
        record.provenance.started_at = self.id.split('-').next().unwrap_or_default().into();
        record.experiment_fingerprint = Some(configuration.sha256.clone());
        record.openapi_fingerprints = configuration
            .openapi_fingerprints
            .clone()
            .into_iter()
            .collect();
        record.status = Some(result.status());
        record.fuzz = result.fuzz.as_ref().map(|summary| FuzzRecord {
            seed: summary.seed,
            executed: summary.executed,
            kept: summary.kept,
            incomplete: summary.incomplete,
        });
        record.findings = finding_records(&result.findings);
        record.counters.findings_count = result.findings.len() as u32;
        record.counters.transport_errors = transport_errors;
        let mut case_paths = Vec::new();
        for (index, evaluated) in std::iter::once(&result.baseline_case)
            .chain(&result.cases)
            .enumerate()
        {
            let case = case_record(configuration, evaluated, index as u32, diagnostic_secrets);
            case_paths.push(write_case_record(&self.path, &case)?);
            record.counters.cases_executed += 1;
            record.counters.requests_sent += evaluated.execution.requests_sent;
            if index == 0 {
                record.baseline_case = Some(case);
            } else {
                record.cases.push(case);
            }
        }
        let run_file = self.path.join("run.json");
        write_json(&run_file, &record)?;
        Ok(WrittenRun {
            record,
            run_file,
            case_paths,
        })
    }
}

fn finding_records(findings: &DedupMap) -> Vec<FindingRecord> {
    let mut records: Vec<_> = findings
        .groups()
        .map(|group| {
            let occurrence = &group.first_occurrence;
            let (oracle_id, violation_category) = match &group.key {
                GroupingKey::OracleFailure {
                    oracle_id,
                    violation_category,
                    ..
                } => (oracle_id, violation_category),
                GroupingKey::ProcessExit {
                    exit_code, signal, ..
                } => {
                    return FindingRecord {
                        artifact_version: RunRecord::current_version(),
                        finding_id: String::new(),
                        kind: "process_exit".into(),
                        case_id: occurrence.case_id.clone(),
                        step_id: Some(occurrence.step_id.clone()),
                        oracle_id: None,
                        verdict: None,
                        violation_category: None,
                        exit_code: *exit_code,
                        signal: *signal,
                        occurrence_count: group.occurrence_count,
                        first_seen_case_id: occurrence.case_id.clone(),
                    }
                }
            };
            FindingRecord {
                artifact_version: RunRecord::current_version(),
                finding_id: String::new(),
                kind: "oracle_failure".into(),
                case_id: occurrence.case_id.clone(),
                step_id: Some(occurrence.step_id.clone()),
                oracle_id: Some(oracle_id.clone()),
                verdict: Some("fail".into()),
                violation_category: Some(violation_category.clone()),
                exit_code: None,
                signal: None,
                occurrence_count: group.occurrence_count,
                first_seen_case_id: occurrence.case_id.clone(),
            }
        })
        .collect();
    records.sort_by(|first, second| {
        (&first.step_id, &first.oracle_id, &first.violation_category).cmp(&(
            &second.step_id,
            &second.oracle_id,
            &second.violation_category,
        ))
    });
    for (index, record) in records.iter_mut().enumerate() {
        record.finding_id = format!("{index:04}");
    }
    records
}

/// The record of one executed case, with the plan `replay` needs.
pub(crate) fn case_record(
    configuration: &ResolvedExperiment,
    evaluated: &EvaluatedCase,
    number: u32,
    diagnostic_secrets: &[String],
) -> CaseRecord {
    let mut case = regression_record(&configuration.experiment, evaluated, number, diagnostic_secrets);
    let mut plan = RegressionPlan {
        experiment_sha256: configuration.sha256.clone(),
        openapi_fingerprints: configuration.openapi_fingerprints.clone(),
        variant: None,
        example_id: None,
        example_step_id: None,
        fuzz: None,
        corpus: None,
        json_example: None,
        malform: None,
        envar: None,
        envfile: None,
    };
    match &evaluated.plan {
        ExecutedPlan::Baseline => {}
        ExecutedPlan::Variant { replay, .. } => plan.variant = replay.clone(),
        ExecutedPlan::Example { example_id, step_id, json_example, .. } => {
            plan.example_id = Some(example_id.clone());
            plan.example_step_id = Some(step_id.clone());
            plan.json_example = json_example.clone();
        }
        ExecutedPlan::Fuzz { seed, iteration, generator, max_fields, .. } => {
            plan.fuzz = Some(FuzzCaseRef {
                seed: *seed,
                iteration: *iteration,
                generator: *generator,
                max_fields: *max_fields,
            });
        }
        ExecutedPlan::Corpus(reference) => plan.corpus = Some(reference.clone()),
        ExecutedPlan::Envar { step_id, variable, case, .. } => {
            plan.envar = Some(crate::artifacts::EnvarRef {
                step_id: step_id.clone(),
                variable: variable.clone(),
                case: case.clone(),
            });
        }
        ExecutedPlan::EnvFile { step_id, name } => {
            plan.envfile = Some(crate::artifacts::EnvFileRef { step_id: step_id.clone(), name: name.clone() });
        }
        ExecutedPlan::Malform { step_id, malformation } => {
            plan.malform = Some(crate::artifacts::MalformRef {
                step_id: step_id.clone(),
                name: malformation.name().into(),
            });
        }
    }
    if !matches!(&evaluated.plan, ExecutedPlan::Variant { replay: None, .. }) {
        case.regression_plan = Some(plan);
    }
    case
}

/// Folder for crash records, next to the experiment file.
pub const CRASHES_DIR: &str = "artifacts/crashes";
pub const TRANSPORT_ERRORS_DIR: &str = "artifacts/transport-errors";

#[derive(serde::Serialize)]
struct DiagnosticArtifact<'a> {
    artifact_version: u32,
    case_id: &'a str,
    case_record: &'a str,
    #[serde(flatten)]
    diagnostic: &'a ExecutionDiagnostic<'a>,
}

pub(crate) fn write_crash_record(
    experiment_directory: &Path,
    record: &CaseRecord,
) -> Result<PathBuf, StorageError> {
    write_case_artifact(&experiment_directory.join(CRASHES_DIR), record, None)
}

pub(crate) fn write_execution_diagnostic(
    experiment_directory: &Path,
    record: &CaseRecord,
    diagnostic: &ExecutionDiagnostic<'_>,
) -> Result<PathBuf, StorageError> {
    let mut evidence = record.clone();
    evidence.regression_plan = None;
    let folder = match diagnostic.outcome {
        ExecutionOutcome::ProcessExit { .. } => CRASHES_DIR,
        ExecutionOutcome::TransportError { .. } => TRANSPORT_ERRORS_DIR,
    };
    write_case_artifact(&experiment_directory.join(folder), &evidence, Some(diagnostic))
}

fn write_case_artifact(
    directory: &Path,
    record: &CaseRecord,
    diagnostic: Option<&ExecutionDiagnostic<'_>>,
) -> Result<PathBuf, StorageError> {
    fs::create_dir_all(directory).map_err(|source| path_error("creating", directory, source))?;
    let json = serde_json::to_vec_pretty(record)?;
    loop {
        let name = crash_file_name();
        let json_path = directory.join(&name);
        let yaml_path = json_path.with_extension("yaml");
        if yaml_path.try_exists().map_err(|source| path_error("checking", &yaml_path, source))? {
            continue;
        }
        match publish_file(&json_path, &json, |temporary| fs::hard_link(temporary, &json_path)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(source) => return Err(path_error("publishing", &json_path, source)),
        }
        let Some(diagnostic) = diagnostic else {
            return Ok(json_path);
        };
        let artifact = DiagnosticArtifact {
            artifact_version: 1,
            case_id: &record.case_id,
            case_record: &name,
            diagnostic,
        };
        let yaml = serde_yaml::to_string(&artifact)
            .map_err(|source| StorageError::SerializeYaml { path: yaml_path.clone(), source })?;
        publish_file(&yaml_path, yaml.as_bytes(), |temporary| fs::hard_link(temporary, &yaml_path))
            .map_err(|source| path_error("publishing", &yaml_path, source))?;
        return Ok(yaml_path);
    }
}

/// `27-11-2026-184512-a3f9c1.json`: local time, then six random hex digits.
fn crash_file_name() -> String {
    let suffix = (uuid::Uuid::new_v4().as_u128() & 0xff_ffff) as u32;
    let now = jiff::ZonedDateTime::now();
    format!("{}-{suffix:06x}.json", now.strftime("%d-%m-%Y-%H%M%S"))
}

fn path_error(action: &'static str, path: &Path, source: std::io::Error) -> StorageError {
    StorageError::Path {
        action,
        path: path.to_owned(),
        source,
    }
}

fn write_case_record(run_dir: &Path, record: &CaseRecord) -> Result<PathBuf, StorageError> {
    let path = run_dir
        .join("cases")
        .join(format!("{:04}.json", record.case_number));
    write_json(&path, record)?;
    Ok(path)
}

fn write_json(path: &Path, value: &impl serde::Serialize) -> Result<(), StorageError> {
    let json = serde_json::to_vec_pretty(value).map_err(|source| StorageError::SerializeAt {
        path: path.to_owned(),
        source,
    })?;
    publish_file(path, &json, |temporary| fs::rename(temporary, path))
        .map_err(|source| path_error("writing", path, source))
}

fn publish_file(
    path: &Path,
    bytes: &[u8],
    publish: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let mut file = fs::OpenOptions::new().write(true).create_new(true).open(&temporary)?;
    let written = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    let result = written.and_then(|()| publish(&temporary));
    let _ = fs::remove_file(&temporary);
    result
}

/// Write a finding record to `findings/<NNNN>.json`.
pub fn write_finding_record(
    run_dir: &Path,
    index: usize,
    record: &FindingRecord,
) -> Result<(), StorageError> {
    let findings_dir = run_dir.join("findings");
    fs::create_dir_all(&findings_dir)?;
    let filename = format!("{:04}.json", index);
    let path = findings_dir.join(&filename);
    let json = serde_json::to_string_pretty(record)?;
    fs::write(&path, json)?;
    Ok(())
}

/// Write a corpus entry to `corpus/<sha256>.json`.
pub fn write_corpus_entry(run_dir: &Path, entry: &CorpusEntryRecord) -> Result<(), StorageError> {
    let corpus_dir = run_dir.join("corpus");
    fs::create_dir_all(&corpus_dir)?;
    let filename = format!("{}.json", entry.sha256);
    let path = corpus_dir.join(&filename);
    let json = serde_json::to_string_pretty(entry)?;
    fs::write(&path, json)?;
    Ok(())
}

/// Load a case record from a saved JSON file.
pub fn load_case_record(path: &Path) -> Result<CaseRecord, StorageError> {
    let json = fs::read_to_string(path)?;
    let record: CaseRecord = serde_json::from_str(&json)?;
    Ok(record)
}

/// Load a finding record from a saved JSON file.
pub fn load_finding_record(path: &Path) -> Result<FindingRecord, StorageError> {
    let json = fs::read_to_string(path)?;
    let record: FindingRecord = serde_json::from_str(&json)?;
    Ok(record)
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("nadir_storage_{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn test_run_id_format() {
        let id = generate_run_id();
        // Should match YYYYMMDDTHHMMSSZ-XXXXXX
        assert!(id.contains('T'), "missing T separator: {}", id);
        assert!(id.contains('Z'), "missing Z: {}", id);
        assert!(id.contains('-'), "missing dash: {}", id);
        assert!(
            id.len() >= 20 && id.len() <= 24,
            "unexpected length: {} ({})",
            id.len(),
            id
        );
    }

    #[test]
    fn test_write_and_read_case() {
        let dir = temp_dir();
        fs::create_dir_all(dir.join("cases")).unwrap();

        let record = CaseRecord {
            artifact_version: 1,
            case_id: "test-case".into(),
            case_number: 0,
            step_results: vec![],
            coverage_snapshot_sha256: None,
            regression_plan: None,
            process_exit: None,
        };

        write_case_record(&dir, &record).unwrap();
        let path = dir.join("cases/0000.json");
        assert!(path.exists());

        let loaded = load_case_record(&path).unwrap();
        assert_eq!(loaded.case_id, "test-case");
        assert_eq!(loaded.case_number, 0);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_publish_reports_the_destination_and_removes_temporary_data() {
        let directory =
            std::env::temp_dir().join(format!("nadir-storage-{}", uuid::Uuid::new_v4()));
        let destination = directory.join("run.json");
        fs::create_dir_all(&destination).unwrap();
        let error = write_json(&destination, &serde_json::json!({"status": "passed"})).unwrap_err();
        assert!(matches!(error, StorageError::Path { path, .. } if path == destination));
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn publish_collision_preserves_the_old_file_and_removes_the_temporary() {
        let directory = temp_dir();
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("case.yaml");
        fs::write(&path, b"old").unwrap();
        let error = publish_file(&path, b"new", |temporary| {
            assert_eq!(fs::read(&path)?, b"old");
            assert_eq!(fs::read(temporary)?, b"new");
            fs::hard_link(temporary, &path)
        }).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&path).unwrap(), b"old");
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_failed_atomic_publication_leaves_no_visible_artifact() {
        let directory = temp_dir();
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("case.yaml");
        let result = publish_file(&path, b"complete", |temporary| {
            assert!(!path.exists());
            assert_eq!(fs::read(temporary)?, b"complete");
            Err(io::Error::other("publication interrupted"))
        });
        assert!(result.is_err());
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 0);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn crash_files_are_named_by_local_time_and_never_collide() {
        let name = crash_file_name();
        let bytes = name.as_bytes();
        assert_eq!(name.len(), "27-11-2026-184512-a3f9c1.json".len(), "{name}");
        for (index, byte) in bytes.iter().enumerate() {
            match index {
                2 | 5 | 10 | 17 => assert_eq!(*byte, b'-', "{name}"),
                0..=16 => assert!(byte.is_ascii_digit(), "{name}"),
                18..=23 => assert!(byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase(), "{name}"),
                _ => {}
            }
        }
        assert!(name.ends_with(".json"));

        let dir = temp_dir().join(uuid::Uuid::new_v4().to_string());
        let record: CaseRecord = serde_json::from_value(serde_json::json!({
            "artifact_version": 1, "case_id": "c", "case_number": 3,
            "step_results": [], "coverage_snapshot_sha256": null,
            "process_exit": null
        })).unwrap();
        let first = write_crash_record(&dir, &record).unwrap();
        let second = write_crash_record(&dir, &record).unwrap();
        assert_ne!(first, second);
        assert!(first.starts_with(dir.join(CRASHES_DIR)));
        assert_eq!(load_case_record(&first).unwrap().case_number, 3);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_case_numbering() {
        // Baseline is 0000, subsequent cases increment
        assert_eq!(format!("{:04}.json", 0u32), "0000.json");
        assert_eq!(format!("{:04}.json", 5u32), "0005.json");
        assert_eq!(format!("{:04}.json", 9999u32), "9999.json");
    }
}
