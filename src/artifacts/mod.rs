//! Versioned artifact schemas: run, case, finding, corpus records.
//!
//! # Design notes
//!
//! **Chosen:** each artifact type has an `artifact_version` field. Serialization
//! uses `serde_json`. Deserialization rejects unknown versions.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

mod regression;
mod replay_plan;

use crate::exploration::RunStopReason;
pub(crate) use regression::regression_record;
pub(crate) use replay_plan::{ReplayPlan, SavedCase, SavedExample};

// ── Run record ─────────────────────────────────────────────────────

/// Top-level run artifact (`run.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub artifact_version: u32,
    pub experiment_name: String,
    pub experiment_fingerprint: Option<String>,
    pub openapi_fingerprints: HashMap<String, String>,
    pub baseline_case: Option<CaseRecord>,
    pub cases: Vec<CaseRecord>,
    pub discarded_attempts: Vec<DiscardRecord>,
    pub findings: Vec<FindingRecord>,
    pub counters: RunCounters,
    pub target_state_policy: String,
    pub stop_reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<crate::evaluation::case::RunStatus>,
    /// The random phase of `strategy: fuzz`, when it ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fuzz: Option<FuzzRecord>,
    pub provenance: RunProvenance,
}

/// Summary of the random phase in `run.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FuzzRecord {
    pub seed: u64,
    pub executed: u64,
    pub kept: usize,
    pub incomplete: u64,
}

/// Identifies a random case; replay regenerates it from these numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FuzzCaseRef {
    pub seed: u64,
    pub iteration: u64,
    /// The rule that built the case; records without it predate rule 2.
    #[serde(default = "first_generator")]
    pub generator: u32,
    /// `exploration.max_fields` when the case ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_fields: Option<u16>,
}

fn first_generator() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunProvenance {
    pub tool: String,
    pub tool_version: String,
    pub started_at: String,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunCounters {
    pub cases_executed: u32,
    pub requests_sent: u32,
    pub findings_count: u32,
    pub discards_count: u32,
    #[serde(default)]
    pub transport_errors: u64,
}

// ── Case record ────────────────────────────────────────────────────

/// Diagnostic data recorded when the monitored target exited during a case.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessExitDiagnostic {
    /// The step that was executing when the target exited.
    pub step_id: String,
    /// Target exit code, when known.
    pub exit_code: Option<i32>,
    /// Termination signal, when known.
    pub signal: Option<i32>,
}

/// Stored representation of a single case execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaseRecord {
    pub artifact_version: u32,
    pub case_id: String,
    pub case_number: u32, // 0000 = baseline
    pub step_results: Vec<StepResultRecord>,
    pub coverage_snapshot_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regression_plan: Option<RegressionPlan>,
    /// Present when the target process exited during this case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_exit: Option<ProcessExitDiagnostic>,
}

/// Portable regression recipe for the experiment that produced this record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegressionPlan {
    pub experiment_sha256: String,
    #[serde(default)]
    pub openapi_fingerprints: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub variant: Option<crate::mutation::RegressionVariant>,
    /// When this is an example case, the example ID from the YAML.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub example_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub example_step_id: Option<String>,
    /// When this is a random case, its seed and iteration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fuzz: Option<FuzzCaseRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corpus: Option<crate::mutation::text::CorpusCaseRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json_example: Option<crate::experiment::types::JsonExample>,
    /// When this is a `malform` case, the step and the malformation name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub malform: Option<MalformRef>,
    /// When this is an `envar` case, the step, variable and value it set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub envar: Option<EnvarRef>,
    /// When this is an `envfile` case, the step and the case name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub envfile: Option<EnvFileRef>,
}

/// Identifies an `envfile` case; replay rebuilds the file from the copied
/// source and the name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvFileRef {
    pub step_id: String,
    pub name: String,
}

/// Which value an `envar` case set: the i-th of the variable's `value`/
/// `values`, or a named case of its `mode: spec` rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvarCase {
    Value(usize),
    Spec(String),
    /// A line (1-based) of a `path` file with the file's SHA-256: replay
    /// refuses the case once the file changes.
    Line { line: usize, sha256: String },
    /// The variable removed from the environment (`mode: absent`, or the
    /// `unset` case of `mode: spec`).
    Unset,
    /// A named entry of the `edge_cases` or `unconstrained` catalog.
    Catalog(String),
}

/// Identifies an `envar` case; replay rebuilds the value from the YAML and
/// the specification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvarRef {
    pub step_id: String,
    pub variable: String,
    pub case: EnvarCase,
}

/// Identifies a `malform` case; replay rebuilds its bytes from the name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MalformRef {
    pub step_id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepResultRecord {
    pub step_id: String,
    pub outcome: String,
    pub status: Option<u16>,
    pub body_sha256: Option<String>,
    pub extracted_outputs: HashMap<String, serde_json::Value>,
    /// Redacted: secrets are stripped before serialization.
    pub oracle_verdicts: Vec<VerdictRecord>,
    /// Transport failure classification: "timeout", "reset", etc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport_reason: Option<crate::adapters::http::transport::TransportReason>,
    /// Transport failure phase: "before_headers", "during_body_read".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport_phase: Option<crate::adapters::http::transport::FailurePhase>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_diagnostic: Option<crate::adapters::cli::ProcessDiagnostic>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerdictRecord {
    pub oracle_id: String,
    pub verdict: String,
    pub category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_location: Option<String>,
}

// ── Finding record ─────────────────────────────────────────────────

/// A deduplicated finding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FindingRecord {
    pub artifact_version: u32,
    pub finding_id: String,
    pub kind: String, // "oracle_failure" | "process_exit"
    pub case_id: String,
    pub step_id: Option<String>,
    pub oracle_id: Option<String>,
    pub verdict: Option<String>,
    pub violation_category: Option<String>,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub occurrence_count: u32,
    pub first_seen_case_id: String,
}

// ── Discard record ─────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscardRecord {
    pub reason: String,
    pub plan_index: usize,
    pub detail: Option<String>,
}

// ── Corpus entry ───────────────────────────────────────────────────

/// A corpus entry stored on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorpusEntryRecord {
    pub artifact_version: u32,
    pub sha256: String, // of the canonical plan
    pub case_id: String,
    pub descriptors: Vec<PartDescriptorRecord>,
    pub replacements: Vec<Option<ReplacementValueRecord>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartDescriptorRecord {
    pub step_id: String,
    pub canonical_selector: String,
    pub input_kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplacementValueRecord {
    pub kind: String, // "json_string" | "header_ascii"
    pub value: String,
}

// ── Builders ───────────────────────────────────────────────────────

impl RunRecord {
    pub fn current_version() -> u32 {
        1
    }

    pub fn new(experiment_name: String, stop_reason: RunStopReason, elapsed_ms: u64) -> Self {
        Self {
            artifact_version: Self::current_version(),
            experiment_name,
            experiment_fingerprint: None,
            openapi_fingerprints: HashMap::new(),
            baseline_case: None,
            cases: Vec::new(),
            discarded_attempts: Vec::new(),
            findings: Vec::new(),
            counters: RunCounters {
                cases_executed: 0,
                requests_sent: 0,
                findings_count: 0,
                discards_count: 0,
                transport_errors: 0,
            },
            target_state_policy: "best_effort".into(),
            stop_reason: format!("{:?}", stop_reason),
            status: None,
            fuzz: None,
            provenance: RunProvenance {
                tool: "nadir".into(),
                tool_version: env!("CARGO_PKG_VERSION").into(),
                started_at: String::new(),
                elapsed_ms,
            },
        }
    }
}

// ── Conversion from execution types ────────────────────────────────

impl CaseRecord {
    pub const CURRENT_VERSION: u32 = 2;
    pub(crate) const CORPUS_VERSION: u32 = 3;
    pub(crate) const JSON_EXAMPLE_VERSION: u32 = 4;
    pub(crate) const MALFORM_VERSION: u32 = 5;
    pub(crate) const ENVAR_VERSION: u32 = 6;
    pub(crate) const ENVFILE_VERSION: u32 = 7;
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_run_record_round_trip() {
        let mut record =
            RunRecord::new("test-exp".into(), RunStopReason::EnumerationComplete, 1234);
        record.counters.cases_executed = 5;

        let json = serde_json::to_string_pretty(&record).unwrap();
        let parsed: RunRecord = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.artifact_version, 1);
        assert_eq!(parsed.experiment_name, "test-exp");
        assert_eq!(parsed.counters.cases_executed, 5);
    }

    #[test]
    fn test_version_field_present() {
        // A full record round-trips with version field preserved
        let record = RunRecord::new("test".into(), RunStopReason::EnumerationComplete, 0);
        let json = serde_json::to_string(&record).unwrap();
        let parsed: RunRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.artifact_version, 1);
        assert_eq!(parsed.experiment_name, "test");
    }

    #[test]
    fn test_case_record_artifact_version() {
        let record = CaseRecord {
            artifact_version: 1,
            case_id: "abc".into(),
            case_number: 0,
            step_results: vec![],
            coverage_snapshot_sha256: None,
            regression_plan: None,
            process_exit: None,
        };
        let json = serde_json::to_string(&record).unwrap();
        let parsed: CaseRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.artifact_version, 1);
    }

    #[test]
    fn backward_compat_deserializes_without_process_exit() {
        // Records predating the `process_exit` field must still deserialize.
        let json = serde_json::json!({
            "artifact_version": 2,
            "case_id": "old-case",
            "case_number": 42,
            "step_results": [{
                "step_id": "app",
                "outcome": "completed",
                "status": null,
                "body_sha256": null,
                "extracted_outputs": {},
                "oracle_verdicts": []
            }],
            "coverage_snapshot_sha256": null
        });
        let record: CaseRecord = serde_json::from_value(json).unwrap();
        assert_eq!(record.case_id, "old-case");
        assert_eq!(record.case_number, 42);
        assert!(record.process_exit.is_none(), "missing field defaults to None");
        assert!(record.step_results[0].process_diagnostic.is_none());
        let serialized = serde_json::to_value(record).unwrap();
        assert!(serialized["step_results"][0].get("process_diagnostic").is_none());
    }

    #[test]
    fn process_exit_diagnostic_round_trips() {
        let diag = ProcessExitDiagnostic {
            step_id: "sign".into(),
            exit_code: Some(1),
            signal: None,
        };
        let record = CaseRecord {
            artifact_version: 2,
            case_id: "crashed".into(),
            case_number: 0,
            step_results: vec![],
            coverage_snapshot_sha256: None,
            regression_plan: None,
            process_exit: Some(diag),
        };
        let json = serde_json::to_string(&record).unwrap();
        let parsed: CaseRecord = serde_json::from_str(&json).unwrap();
        let diag = parsed.process_exit.unwrap();
        assert_eq!(diag.step_id, "sign");
        assert_eq!(diag.exit_code, Some(1));
        assert_eq!(diag.signal, None);
    }
}
