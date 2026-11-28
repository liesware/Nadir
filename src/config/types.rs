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
    #[serde(alias = "adapters")]
    pub protocols: RawProtocols,
    pub steps: Vec<RawStep>,
    /// Environment variables whose values are shown as `[hidden]`.
    #[serde(default)]
    pub hidden: Option<Vec<String>>,
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
    /// Most distinct parts one random case changes (default: all).
    #[serde(default)]
    pub max_fields: Option<u16>,
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
pub struct RawProtocols {
    #[serde(default)]
    pub http: Option<RawHttpProtocol>,
    #[serde(default)]
    pub cli: Option<RawCliProtocol>,
}

/// HTTP protocol configuration.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawHttpProtocol {
    pub base_url: String,
    pub openapi: String,
    #[serde(default = "default_http_timeout_ms")]
    pub timeout_ms: u64,
    /// Longest random path parameter value, in characters (default 4096).
    #[serde(default)]
    pub max_path_chars: Option<usize>,
}

/// CLI protocol configuration.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawCliProtocol {
    #[serde(default = "default_cli_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_http_timeout_ms() -> u64 {
    2000
}

fn default_cli_timeout_ms() -> u64 {
    5000
}

/// A single workflow step.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawStep {
    pub id: String,
    #[serde(alias = "adapter")]
    pub protocol: String,
    pub format: Option<String>,
    pub operation_id: Option<String>,
    pub operation_path: Option<String>,
    pub body_example: Option<serde_json::Value>,
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
    /// Show the request template of this step in the run display.
    #[serde(default)]
    pub print: Option<bool>,
    /// Show each request of this step before sending it.
    #[serde(default)]
    pub debug: Option<RawDebug>,
    /// CLI steps: the command and its arguments.
    #[serde(default)]
    pub command: Option<Vec<String>>,
    /// CLI steps: base environment variables.
    #[serde(default)]
    pub env: Option<HashMap<String, String>>,
    /// CLI steps: a `.env` file with the base environment (relative to the
    /// experiment file). `env` entries override it.
    #[serde(default)]
    pub env_file: Option<String>,
    /// CLI steps: run each case in a fresh temporary directory with copies
    /// of these files.
    #[serde(default)]
    pub workdir: Option<RawWorkdir>,
    /// CLI steps: the command is a service; wait until it is ready, then stop it.
    #[serde(default)]
    pub service: Option<RawService>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawWorkdir {
    #[serde(default)]
    pub copy: Vec<RawCopy>,
}

/// One `workdir.copy` entry: a path kept as is, or `{from, to}` to copy a
/// file under another name (for example a prepared file as `.env`).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum RawCopy {
    Same(String),
    Renamed { from: String, to: String },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawService {
    pub ready: RawServiceReady,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawServiceReady {
    pub url: String,
    #[serde(default = "default_service_ready_ms")]
    pub timeout_ms: u64,
}

fn default_service_ready_ms() -> u64 {
    3000
}

/// `debug: true` waits for a key; `debug: {auto: N}` advances every N seconds.
#[derive(Debug)]
pub enum RawDebug {
    Enabled(bool),
    Auto(RawDebugAuto),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawDebugAuto {
    pub auto: u32,
}

impl<'de> Deserialize<'de> for RawDebug {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Form {
            Enabled(bool),
            Auto(RawDebugAuto),
        }
        match Form::deserialize(deserializer) {
            Ok(Form::Enabled(enabled)) => Ok(Self::Enabled(enabled)),
            Ok(Form::Auto(auto)) => Ok(Self::Auto(auto)),
            Err(_) => Err(serde::de::Error::custom(
                "debug must be true, false or {auto: N} with N in seconds \
                 (for auto write `debug: {auto: 10}`; `debug: true` cannot also hold auto)",
            )),
        }
    }
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
    pub field: Option<String>,
    /// The value to substitute for this field.
    #[serde(default, deserialize_with = "present_value")]
    pub value: Option<serde_json::Value>,
    pub syntax: Option<String>,
    /// Expected HTTP status code for this example case.
    pub expect_status: u16,
}

/// A single mutator declaration.
#[derive(Debug, Default, Deserialize)]
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
    pub path: Option<String>,
    pub repetitions: Option<u16>,
    pub seed: Option<u64>,
    pub length: Option<RawLength>,
    pub keys: Option<Vec<String>>,
    pub rename_to: Option<Vec<String>>,
    pub expect_status: Option<u16>,
    /// `use: malform`: the catalog entries to run (default: all).
    #[serde(default)]
    pub only: Option<Vec<String>>,
    /// `use: envar`: variables to vary. Accepts a list of names (all share
    /// the same `mode`) or a map from name to `{value, values}`.
    #[serde(default, deserialize_with = "deserialize_envar_variables")]
    pub variables: Option<EnvarVariables>,
    /// `use: envar`, `mode: spec`: the specification file (relative to the
    /// experiment file).
    #[serde(default)]
    pub spec: Option<String>,
    /// `use: envfile`: the file to break, as `workdir.copy` names it in the
    /// working directory (for example `.env`).
    #[serde(default)]
    pub file: Option<String>,
    /// `use: envfile`: documented size limits; each adds its case.
    #[serde(default)]
    pub limits: Option<RawEnvLimits>,
    /// `use: envfile`: variables the file must not set (one case each).
    #[serde(default)]
    pub prohibited: Option<Vec<String>>,
    /// `use: body`: how many edits a case makes at most.
    #[serde(default)]
    pub edits: Option<RawEdits>,
    /// `use: malform`: sizes of `huge_uri`, `huge_header`, `many_headers`.
    #[serde(default)]
    pub sizes: Option<RawMalformSizes>,
}

/// `use: malform` `sizes`.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawMalformSizes {
    pub uri: Option<usize>,
    pub header: Option<usize>,
    pub headers: Option<usize>,
}

/// `use: body` `edits`.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawEdits {
    pub max: usize,
}

/// `use: envfile` `limits`, in bytes.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawEnvLimits {
    pub file_bytes: Option<usize>,
    pub value_bytes: Option<usize>,
    pub name_bytes: Option<usize>,
}

/// The `variables` field of an `envar` mutator, supporting two YAML shapes.
#[derive(Debug, Clone)]
pub enum EnvarVariables {
    /// List form: `variables: [APP_LABEL, APP_PROFILE]`
    List(Vec<String>),
    /// Map form: `variables: { APP_OUTPUT_FORMAT: {values: ["json","text"]} }`
    Map(HashMap<String, EnvarRule>),
    /// `variables: all`: every variable of the base environment that no
    /// other `envar` mutator of the step varies.
    All,
    /// `variables: spec`: every variable of the `spec` file (that its
    /// `used_when` allows) that no other `envar` mutator of the step varies.
    Spec,
}

/// A single variable rule in the map form.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvarRule {
    #[serde(default, deserialize_with = "present_value")]
    pub value: Option<serde_json::Value>,
    #[serde(default)]
    pub values: Option<Vec<serde_json::Value>>,
    /// A file with one value per line (relative to the experiment file).
    #[serde(default)]
    pub path: Option<String>,
}

impl<'de> serde::Deserialize<'de> for EnvarVariables {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de;

        struct EnvarVariablesVisitor;

        impl<'de> de::Visitor<'de> for EnvarVariablesVisitor {
            type Value = EnvarVariables;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a list of variable names, a map from name to rule, 'all' or 'spec'")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "all" => Ok(EnvarVariables::All),
                    "spec" => Ok(EnvarVariables::Spec),
                    other => Err(E::custom(format!("variables: '{other}' (expected a list, a map, 'all' or 'spec')"))),
                }
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: de::SeqAccess<'de>,
            {
                let mut names = Vec::new();
                while let Some(name) = seq.next_element::<String>()? {
                    names.push(name);
                }
                if names.is_empty() {
                    return Err(de::Error::invalid_length(0, &"at least one variable"));
                }
                Ok(EnvarVariables::List(names))
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: de::MapAccess<'de>,
            {
                let mut rules = HashMap::new();
                while let Some((name, rule)) = map.next_entry::<String, EnvarRule>()? {
                    rules.insert(name, rule);
                }
                if rules.is_empty() {
                    return Err(de::Error::invalid_length(0, &"at least one variable"));
                }
                Ok(EnvarVariables::Map(rules))
            }
        }

        deserializer.deserialize_any(EnvarVariablesVisitor)
    }
}

fn deserialize_envar_variables<'de, D>(deserializer: D) -> Result<Option<EnvarVariables>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<EnvarVariables>::deserialize(deserializer)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawLength {
    pub reference: Option<usize>,
    pub max: Option<usize>,
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
    #[serde(default, deserialize_with = "present_value")]
    pub one_of: Option<Vec<serde_json::Value>>,
    #[serde(default, deserialize_with = "present_value")]
    pub exists: Option<bool>,
    #[serde(default, deserialize_with = "present_value")]
    pub absent: Option<bool>,
}

fn present_value<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}
