//! Raw YAML v1 types matching the experiment schema.
//!
//! Every struct uses `#[serde(deny_unknown_fields)]` so that typos and
//! misplaced fields are caught at parse time. These types represent the
//! surface syntax only — resolution into validated `Experiment` types
//! happens in `src/experiment/`.

use serde::Deserialize;
use std::collections::HashMap;

/// Top-level YAML document.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawExperiment {
    pub version: u8,
    pub experiment: RawExperimentBlock,
}

/// The `experiment` block.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawExperimentBlock {
    pub name: String,
    pub max_cases: u16,
    #[serde(default)]
    pub exploration: Option<RawExploration>,
    #[serde(default)]
    pub target: Option<RawTarget>,
    pub adapters: RawAdapters,
    pub steps: Vec<RawStep>,
}

/// Exploration strategy declaration.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawExploration {
    pub strategy: String,
    #[serde(default)]
    pub max_execs: Option<u32>,
    #[serde(default)]
    pub duration_s: Option<u32>,
    #[serde(default)]
    pub seed: Option<u64>,
}

/// Target process configuration.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawTarget {
    pub command: Vec<String>,
    #[serde(default)]
    pub ready: RawReady,
    #[serde(default)]
    pub coverage: Option<RawCoverage>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: Option<HashMap<String, String>>,
}

/// Readiness check configuration.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawReady {
    #[serde(default = "default_ready_path")]
    pub path: String,
    #[serde(default = "default_ready_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_ready_path() -> String {
    "/".into()
}

fn default_ready_timeout_ms() -> u64 {
    10000
}

/// Coverage transport configuration.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawCoverage {
    pub transport: String,
    #[serde(default = "default_map_size")]
    pub map_size: usize,
}

fn default_map_size() -> usize {
    65536
}

/// Adapter declarations keyed by name.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawAdapters {
    #[serde(default)]
    pub http: Option<RawHttpAdapter>,
}

/// HTTP adapter configuration.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawHttpAdapter {
    pub base_url: String,
    pub openapi: String,
    #[serde(default = "default_http_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_http_timeout_ms() -> u64 {
    2000
}

/// A single workflow step.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawStep {
    pub id: String,
    pub adapter: String,
    pub operation_id: String,
    #[serde(default)]
    pub bind: Option<HashMap<String, serde_json::Value>>,
    #[serde(default)]
    pub auth: Option<RawAuth>,
    #[serde(default)]
    pub outputs: Option<HashMap<String, RawOutput>>,
    #[serde(default)]
    pub cases: Option<RawStepCases>,
    #[serde(default)]
    pub oracles: Vec<RawOracle>,
}

/// Authentication declaration.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawAuth {
    pub bearer: String,
}

/// A step output declaration.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawOutput {
    pub from: String,
    #[serde(default)]
    pub when_status: Option<serde_json::Value>,
    #[serde(default)]
    pub secret: bool,
}

/// Case mutation configuration for a step.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawStepCases {
    #[serde(default)]
    pub mutators: Vec<RawMutator>,
    #[serde(default)]
    pub examples: Vec<RawExample>,
}

/// A single example declaration for structured case testing.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawExample {
    pub id: String,
    /// Dot-separated body field selector (e.g., "title").
    pub field: String,
    /// The value to substitute for this field.
    pub value: serde_json::Value,
    /// Expected HTTP status code for this example case.
    pub expect_status: u16,
}

/// A single mutator declaration.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawMutator {
    pub r#use: String,
    #[serde(default)]
    pub fields: Option<Vec<String>>,
    /// Header values (strings) or extra JSON values (any JSON).
    #[serde(default)]
    pub values: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    pub mode: Option<String>,
}

/// A single oracle declaration within a step.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawOracle {
    #[serde(default)]
    pub id: Option<String>,
    pub r#use: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub setup: Option<bool>,
    #[serde(default)]
    pub select: Option<String>,
    #[serde(rename = "when")]
    #[serde(default)]
    pub r#when: Option<RawWhenBlock>,
    #[serde(default)]
    pub expect: Option<RawExpectBlock>,
    #[serde(default)]
    pub record_reference: Option<String>,
}

/// A `when` condition on an oracle.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawWhenBlock {
    pub select: String,
    pub equals: serde_json::Value,
}

/// An `expect` block for an assert oracle.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawExpectBlock {
    #[serde(default, deserialize_with = "present_value")]
    pub equals: Option<serde_json::Value>,
    #[serde(default)]
    pub one_of: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    pub exists: Option<bool>,
    #[serde(default)]
    pub absent: Option<bool>,
}

fn present_value<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<serde_json::Value>, D::Error> {
    serde_json::Value::deserialize(deserializer).map(Some)
}
