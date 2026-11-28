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
    pub protocols: ProtocolConfigs,
    pub steps: Vec<Step>,
    /// Environment variables whose values are shown as `[hidden]`.
    pub hidden: Vec<String>,
}

impl Experiment {
    /// Every `${env.NAME}` the experiment reads (steps and target), with the
    /// first default found for each name, in order of appearance.
    pub fn env_references(&self) -> Vec<(String, Option<String>)> {
        use crate::experiment::selectors::{parse_binding_value, BindingParseResult, Reference};
        let mut found: Vec<(String, Option<String>)> = Vec::new();
        let mut add = |reference: &Reference| {
            if let Reference::Env { name, default } = reference {
                if !found.iter().any(|(known, _)| known == name) {
                    found.push((name.clone(), default.clone()));
                }
            }
        };
        for step in &self.steps {
            for binding in &step.bindings {
                match &binding.value {
                    BindingValue::Reference(reference) => add(reference),
                    BindingValue::Composite(parts) => parts.iter().for_each(|part| {
                        if let CompositePart::Reference(reference) = part {
                            add(reference);
                        }
                    }),
                    BindingValue::Literal(_) => {}
                }
            }
            if let Some(auth) = &step.auth {
                add(&auth.bearer);
            }
        }
        if let Some(target) = &self.target {
            for text in target.command.iter().chain(target.env.values()) {
                match parse_binding_value(&serde_json::Value::String(text.clone())) {
                    BindingParseResult::Reference(reference) => add(&reference),
                    BindingParseResult::Composite(parts) => parts.iter().for_each(|part| {
                        if let CompositePart::Reference(reference) = part {
                            add(reference);
                        }
                    }),
                    _ => {}
                }
            }
        }
        found
    }
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
        /// Most distinct parts one random case changes; `None` = all.
        max_fields: Option<u16>,
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
pub struct ProtocolConfigs {
    pub http: Option<HttpProtocolConfig>,
    pub cli: Option<CliProtocolConfig>,
}

#[derive(Debug, Clone)]
pub struct HttpProtocolConfig {
    pub base_url: String,
    pub openapi_path: String,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone)]
pub struct CliProtocolConfig {
    pub timeout_ms: u64,
}

// ── Steps ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Step {
    pub id: String,
    pub protocol: ProtocolKind,
    pub format: crate::formats::PayloadFormat,
    pub operation_key: String,
    pub body_example: Option<serde_json::Value>,
    pub bindings: Vec<Binding>,
    pub auth: Option<AuthConfig>,
    pub outputs: Vec<OutputDecl>,
    pub mutators: Vec<MutatorDecl>,
    pub corpora: Vec<crate::mutation::text::Corpus>,
    pub key_mutators: Vec<crate::mutation::fuzz::KeyMutator>,
    pub oracles: Vec<OracleDecl>,
    /// Explicit cases that check the exact HTTP status.
    pub examples: Vec<ExampleCase>,
    /// Status codes the OpenAPI operation documents, sorted; `0` is `default`.
    pub declared_statuses: Vec<u16>,
    /// Show the request template in the run display.
    pub print: bool,
    /// Show each request before sending it.
    pub debug: Option<DebugMode>,
    /// CLI steps: the command and its arguments.
    pub command: Option<Vec<String>>,
    /// CLI steps: base environment variables.
    pub env_config: Option<HashMap<String, String>>,
    /// CLI steps: where and how the command runs.
    pub cli: Option<CliSetup>,
}

/// How a `protocol: cli` step runs its command.
#[derive(Debug, Clone, Default)]
pub struct CliSetup {
    /// The experiment directory: the program and copied files resolve here,
    /// and it is the working directory without `workdir`.
    pub dir: std::path::PathBuf,
    /// Variables that came from `env_file` (not overridden by `env`).
    pub from_env_file: Vec<String>,
    /// The `env_file` as written, for display.
    pub env_file: Option<String>,
    /// Files copied into a fresh working directory for each case, as
    /// (source next to the experiment, destination in the directory).
    pub workdir_copy: Option<Vec<(String, String)>>,
    pub service: Option<ServiceConfig>,
}

/// `service.ready` of a cli step.
#[derive(Debug, Clone)]
pub struct ServiceConfig {
    pub ready_url: url::Url,
    pub ready_timeout: std::time::Duration,
}

/// How `debug` advances to the next request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugMode {
    /// Wait for the space key.
    Manual,
    /// Advance after this long, or earlier on the space key.
    Auto(std::time::Duration),
}

/// Longest `debug.auto` interval, in seconds.
pub const MAX_DEBUG_AUTO_S: u32 = 3600;

/// A single example case declared in `cases.examples`.
///
#[derive(Debug, Clone)]
pub struct ExampleCase {
    pub id: String,
    pub action: ExampleAction,
    pub expect_status: u16,
}

impl ExampleCase {
    pub(crate) fn json_example(&self) -> Option<&JsonExample> {
        match &self.action {
            ExampleAction::Json(example) => Some(example),
            ExampleAction::Field { .. } => None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum ExampleAction {
    Field { path: String, value: Value },
    Json(JsonExample),
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JsonExample {
    Rename { key: String, destination: String },
    Syntax { fixture: JsonSyntaxFixture },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JsonSyntaxFixture {
    IncompleteObject,
    TrailingComma,
    LeadingText,
    TrailingText,
}

impl JsonSyntaxFixture {
    pub(crate) fn bytes(self) -> &'static [u8] {
        match self {
            Self::IncompleteObject => b"{\"sample\":",
            Self::TrailingComma => b"{\"sample\":1,}",
            Self::LeadingText => b"x{}",
            Self::TrailingText => b"{}x",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProtocolKind {
    Http,
    /// HTTP/1.1 written byte for byte to a socket; the step may break it.
    HttpRaw,
    /// Execute a local command and observe its exit status and output.
    Cli,
    #[cfg(test)]
    InMemory,
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
        /// `path`: one case per line (with `mode` `Lines`).
        lines: Option<ValueLines>,
    },
    /// `use: http` (and its alias `use: headers`): header and path values.
    Http {
        fields: Vec<HttpField>,
        mode: JsonMutatorMode,
        /// Extra values sent after the mode's catalog, in declaration order.
        values: Vec<String>,
        /// The OpenAPI parameter schema per field selector, when declared.
        field_schemas: HashMap<String, FieldSchema>,
        /// `path`: one case per line (with `mode` `Lines`).
        lines: Option<ValueLines>,
    },
    /// `use: envfile` (`protocol: cli` only): one case per way to break the
    /// `.env`-style `file` the working directory gets.
    EnvFile {
        file: String,
        /// The file as copied, before any case breaks it.
        base: Vec<u8>,
        cases: Vec<crate::adapters::cli::envbreak::EnvFileCase>,
    },
    /// `use: malform` (`protocol: http_raw` only): one case per malformation.
    Malform {
        malformations: Vec<crate::adapters::http_raw::Malformation>,
    },
    /// `use: envar` (`protocol: cli` only): vary environment variables.
    Envar {
        variables: Vec<EnvarVariable>,
        /// `mode: spec` variables left out by `used_when`, with the reason.
        skipped: Vec<String>,
    },
}

/// A single variable rule resolved from the envar mutator.
#[derive(Debug, Clone)]
pub struct EnvarVariable {
    pub name: String,
    pub rule: EnvarVariableRule,
}

/// The kind of variation for one environment variable.
#[derive(Debug, Clone)]
pub enum EnvarVariableRule {
    /// A single fixed value (enumerated).
    Fixed(String),
    /// Several fixed values, one case each (enumerated).
    Values(Vec<String>),
    /// Generative mode for `strategy: fuzz`: 1 to 64 characters.
    Mode(EnvarMode),
    /// Generative mode with `length`: the shared length policy.
    Text(crate::mutation::text::TextGenerator),
    /// `path`: one enumerated case per line of the file.
    File {
        path: String,
        sha256: String,
        lines: Vec<String>,
    },
    /// `mode: spec`: one enumerated case per invalid value of the rule.
    Spec(crate::adapters::cli::spec::SpecRule),
    /// `mode: absent`: one enumerated case without the variable.
    Absent,
    /// `mode: edge_cases` / `unconstrained`: one enumerated case per catalog
    /// entry (the same catalogs as `json` and `http`, without NUL).
    Catalog { unconstrained: bool },
}

/// The lines of a `path` file of `use: json` / `use: http`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueLines {
    /// As written in the experiment, for display.
    pub path: String,
    pub sha256: String,
    pub lines: Vec<String>,
}

impl ValueLines {
    /// The first 8 bytes of the SHA-256: saved with each line so `replay`
    /// refuses a case once the file changes.
    pub fn fingerprint(&self) -> u64 {
        u64::from_str_radix(&self.sha256[..16], 16).unwrap_or_default()
    }
}

/// The replacement for an `envar` case value: `None` removes the variable.
pub fn envar_replacement(value: &Option<String>) -> ReplacementValue {
    match value {
        Some(text) => ReplacementValue::JsonString(text.clone()),
        None => ReplacementValue::Absent,
    }
}

/// Generative modes for environment variable values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EnvarMode {
    /// ASCII letters only.
    Alpha,
    /// ASCII letters and digits.
    Alphanum,
    /// Digits only.
    Num,
    /// Any character but NUL: controls, punctuation, non-ASCII.
    Raw,
}

/// A request part `use: http` mutates.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum HttpField {
    Header(String),
    Path(String),
}

impl HttpField {
    /// `header.X-Key` → `Header("X-Key")`, `path.kid` → `Path("kid")`.
    pub fn parse(selector: &str) -> Option<Self> {
        let (location, name) = selector.split_once('.')?;
        if name.is_empty() {
            return None;
        }
        match location {
            "header" => Some(Self::Header(name.to_owned())),
            "path" => Some(Self::Path(name.to_owned())),
            _ => None,
        }
    }

    /// The canonical selector, as in `bind`.
    pub fn selector(&self) -> String {
        match self {
            Self::Header(name) => format!("header.{name}"),
            Self::Path(name) => format!("path.{name}"),
        }
    }

    pub fn is_header(&self) -> bool {
        matches!(self, Self::Header(_))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JsonMutatorMode {
    EdgeCases,
    Unconstrained,
    /// Values derived from each field's request schema.
    Schema,
    /// Random ASCII alphanumeric strings for input validation.
    Alphanumeric,
    Text(crate::mutation::text::TextGenerator),
    /// `use: http` only: send the request without the header (or with an
    /// empty path segment).
    Absent,
    /// `use: http` only: no catalog, just the mutator's `values`.
    Values,
    /// `path`: no catalog; one case per line of the file (then `values`).
    Lines,
    /// `use: envar` with `mode` and no `length`: random values of 1 to 64
    /// characters.
    Env(EnvarMode),
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
    /// A malformed request was handled: answered without a 5xx, or the
    /// connection closed, before the timeout. Used by `wire:handled`; not
    /// available in YAML.
    Handled,
    /// A service started with an invalid variable refused: exited before
    /// ready with a non-zero code, naming the variable and not echoing the
    /// value. Used by `startup:refuses`; not available in YAML.
    Refused,
    /// A service started without an optional variable came up with its
    /// documented default: ready (or exit 0 without `service`). Used by
    /// `startup:defaults`; not available in YAML.
    StartsWithDefault,
}
