//! Resolved experiment model: validated steps, bindings, oracles.
//!
//! These types represent a fully validated experiment ready for execution.
//! All references are parsed and checked. No raw YAML types appear here.
//!
//! # Design notes
//!
//! **Chosen:** `Experiment` as the single validated configuration type.
//! Resolution produces it once; the rest of the system references it via
//! `Arc<Experiment>`. Raw types live in `config/` and never leak out.

use std::collections::HashMap;

use crate::experiment::selectors::{InputSelector, ObservationSelector, Reference};

use serde_json::Value;

// ── Top-level experiment ──────────────────────────────────────────

/// A validated experiment ready for execution.
#[derive(Debug, Clone)]
pub struct Experiment {
    pub name: String,
    pub max_cases: MaxCases,
    pub exploration: ExplorationConfig,
    pub target: Option<TargetConfig>,
    pub adapters: AdapterConfigs,
    pub steps: Vec<Step>,
}

/// Largest accepted `max_cases`, including the baseline case.
pub const MAX_CASES_LIMIT: u16 = 1000;

/// Bounded case count: 1..=`MAX_CASES_LIMIT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaxCases(pub(crate) u16);

impl MaxCases {
    /// Create a `MaxCases`, clamping to 1..=`MAX_CASES_LIMIT`.
    pub fn new(v: u16) -> Self {
        Self(v.clamp(1, MAX_CASES_LIMIT))
    }

    pub fn get(&self) -> u16 {
        self.0
    }
}

// ── Exploration ────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ExplorationConfig {
    pub strategy: ExplorationStrategy,
}

#[derive(Debug, Clone)]
pub enum ExplorationStrategy {
    Enumerate,
    Fuzz {
        max_execs: u32,
        duration_s: u32,
        seed: Option<u64>,
    },
}

// ── Target ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct TargetConfig {
    pub command: Vec<String>,
    pub ready: ReadyConfig,
    pub coverage: Option<CoverageConfig>,
    pub cwd: Option<String>,
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct ReadyConfig {
    pub path: String,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone)]
pub struct CoverageConfig {
    pub transport: CoverageTransport,
    pub map_size: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverageTransport {
    MmapFile,
    AflShm,
}

// ── Adapters ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct AdapterConfigs {
    pub http: Option<HttpAdapterConfig>,
}

#[derive(Debug, Clone)]
pub struct HttpAdapterConfig {
    pub base_url: String,
    pub openapi_path: String,
    pub timeout_ms: u64,
}

// ── Steps ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Step {
    pub id: String,
    pub adapter: AdapterKind,
    pub operation_id: String,
    pub bindings: Vec<Binding>,
    pub auth: Option<AuthConfig>,
    pub outputs: Vec<OutputDecl>,
    pub mutators: Vec<MutatorDecl>,
    pub oracles: Vec<OracleDecl>,
    /// Structured example cases: each replaces one body field with a
    /// declared value and checks the exact HTTP status.
    pub examples: Vec<ExampleCase>,
    /// Status codes the OpenAPI operation documents, sorted; `0` is `default`.
    pub declared_statuses: Vec<u16>,
}

/// A single example case declared in `cases.examples`.
///
/// Each example replaces one JSON body field and asserts the exact
/// HTTP status code. The `id` is saved alongside the case descriptor
/// for replay.
#[derive(Debug, Clone)]
pub struct ExampleCase {
    pub id: String,
    /// Dot-separated body field selector (e.g. `"title"`).
    pub field: String,
    pub value: serde_json::Value,
    pub expect_status: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterKind {
    Http,
}

#[derive(Debug, Clone)]
pub struct Binding {
    pub selector: InputSelector,
    pub value: BindingValue,
}

#[derive(Debug, Clone)]
pub enum BindingValue {
    Literal(Value),
    Reference(Reference),
    Composite(Vec<CompositePart>),
}

#[derive(Debug, Clone)]
pub enum CompositePart {
    Literal(String),
    Reference(Reference),
}

#[derive(Debug, Clone)]
pub struct AuthConfig {
    pub bearer: Reference,
}

#[derive(Debug, Clone)]
pub struct OutputDecl {
    pub name: String,
    pub from: ObservationSelector,
    pub when_status: Option<Vec<u16>>,
    pub secret: bool,
}

// ── Replacement values ─────────────────────────────────────────────

/// A value to substitute for a mutable field.
#[derive(Debug, Clone)]
pub enum ReplacementValue {
    JsonString(String),
    HeaderAscii(String),
    /// An arbitrary JSON value (example cases, schema cases, custom values).
    JsonValue(serde_json::Value),
    /// Remove the field from the request body.
    Absent,
}

// Manual PartialEq + Eq + Hash for ReplacementValue since serde_json::Value
// does not implement Eq/Hash.
impl PartialEq for ReplacementValue {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::JsonString(a), Self::JsonString(b)) => a == b,
            (Self::HeaderAscii(a), Self::HeaderAscii(b)) => a == b,
            (Self::JsonValue(a), Self::JsonValue(b)) => a == b,
            (Self::Absent, Self::Absent) => true,
            _ => false,
        }
    }
}

impl Eq for ReplacementValue {}

impl std::hash::Hash for ReplacementValue {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        match self {
            Self::JsonString(s) => s.hash(state),
            Self::HeaderAscii(s) => s.hash(state),
            Self::JsonValue(v) => {
                // Hash the JSON representation
                if let Ok(s) = serde_json::to_string(v) {
                    s.hash(state);
                }
            }
            Self::Absent => {}
        }
    }
}

// ── Mutators ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum MutatorDecl {
    Json {
        fields: Option<Vec<String>>,
        mode: JsonMutatorMode,
        /// Extra values sent after the mode's catalog, in declaration order.
        values: Vec<Value>,
        /// Request-schema information per field, when the operation has one.
        field_schemas: HashMap<String, FieldSchema>,
    },
    Headers {
        fields: Vec<String>,
        values: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JsonMutatorMode {
    EdgeCases,
    Unconstrained,
    /// Values derived from each field's request schema.
    Schema,
}

/// The JSON Schema of one request body field and whether its parent object
/// requires it.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldSchema {
    pub schema: Value,
    pub required: bool,
}

// ── Oracles ────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct OracleDecl {
    pub id: String,
    /// Canonical oracle ID: if no explicit id, "openapi" for openapi oracles.
    pub oracle_type: OracleType,
    pub kind: Option<OracleKind>,
    pub setup: bool,
    pub select: Option<ObservationSelector>,
    pub when_condition: Option<WhenCondition>,
    pub expect: Option<ExpectCondition>,
    pub record_reference: Option<ObservationSelector>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OracleType {
    OpenApi,
    /// Validates the response body JSON against the OpenAPI schema declared
    /// for the exact HTTP status code. Undeclared status → contract violation.
    OpenApiSchema,
    Assert,
    RecordAbsent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OracleKind {
    Security,
    Check,
}

#[derive(Debug, Clone)]
pub struct WhenCondition {
    pub select: ObservationSelector,
    pub equals: Value,
}

#[derive(Debug, Clone)]
pub enum ExpectCondition {
    Equals(Value),
    OneOf(Vec<Value>),
    Exists,
    Absent,
    /// A number of at least 400: the request was rejected. Used by the
    /// implicit `schema:rejects-invalid` rule; not available in YAML.
    Rejected,
}
