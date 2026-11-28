//! Input mutation: variant enumeration, token dictionary, value generation.
//!
//! # Design notes
//!
//! **Chosen:** `VariantGenerator::enumerate()` returns a `Vec<CasePlan>` in
//! canonical order (step declaration → mutator declaration → field byte order).
//! Each variant replaces exactly one field. Bound fields and duplicate values
//! are excluded.

pub mod fuzz;
pub mod text;

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::adapters::http::{contract::ResolvedContract, execution::baseline_value};
use crate::experiment::types::{
    Experiment, FieldSchema, JsonMutatorMode, MutatorDecl, ReplacementValue, Step,
};

// ── Part descriptor ────────────────────────────────────────────────

/// Stable descriptor for one mutable part, computed at resolution time.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PartDescriptor {
    pub step_id: String,
    /// Canonical selector string (e.g., "body.title", "header.Accept-Language").
    pub canonical_selector: String,
    pub input_kind: InputKind,
    pub representation: ValueRepresentation,
    pub mutation_mode: Option<JsonMutatorMode>,
    /// Index in the step's `mutators` this descriptor came from, so a part
    /// with several mutators uses each one's own catalog. `None` matches
    /// the first mutator that lists the part.
    pub mutator_index: Option<usize>,
}

impl PartDescriptor {
    /// Whether mutator `index` of the step is the one this descriptor uses.
    fn uses_mutator(&self, index: usize) -> bool {
        self.mutator_index.is_none_or(|own| own == index)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InputKind {
    JsonField,
    HeaderField,
    PathField,
    /// A `protocol: cli` step's environment variable.
    EnvVar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ValueRepresentation {
    JsonString,
    HeaderAscii,
}

// ── Case plan ──────────────────────────────────────────────────────

/// A set of replacements to apply for one case.
#[derive(Debug, Clone)]
pub struct CasePlan {
    /// Descriptors in canonical order (one per mutable part).
    pub descriptors: Vec<PartDescriptor>,
    /// Replacements index-aligned with descriptors.
    /// `None` means "no replacement for this descriptor in this plan."
    pub replacements: Vec<Option<ReplacementValue>>,
}

// ── Variant generator ──────────────────────────────────────────────

/// Identifies a configured regression variant without persisting the request value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegressionVariant {
    pub step_id: String,
    pub selector: String,
    #[serde(flatten)]
    identity: VariantIdentity,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum VariantIdentity {
    Stable { variant_id: VariantId },
    Legacy { choice: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "option")]
enum VariantId {
    Empty,
    LongLowercase,
    LongUppercase,
    Unicode,
    Reserved,
    NulByte,
    HugeUppercase,
    ScriptTag,
    SqlQuote,
    TemplateSyntax,
    NullString,
    // `mode: schema`: values derived from the field's request schema.
    MissingField,
    NullValue,
    WrongTypeString,
    WrongTypeNumber,
    WrongTypeBoolean,
    WrongTypeArray,
    WrongTypeObject,
    TooShort,
    AtMaxLength,
    OverMaxLength,
    BadFormat,
    NotInEnum,
    BelowMinimum,
    AboveMaximum,
    Fraction,
    IntegerOverflow,
    HugeNumber,
    NegativeOne,
    EmptyArray,
    OverMaxItems,
    EmptyObject,
    /// A value from the mutator's `values` list, by position.
    Custom(usize),
    /// Saved by `use: headers` before `use: http`; resolves as `Custom`.
    Header(usize),
    /// `use: http`, `mode: absent`: the header is left out.
    Absent,
    /// `path`: a line (1-based) of the file, with the file's fingerprint
    /// (`ValueLines::fingerprint`): a changed file no longer resolves.
    Line { line: usize, file: u64 },
}

const EDGE_CASES_V1: [VariantId; 5] = [
    VariantId::Empty,
    VariantId::LongLowercase,
    VariantId::LongUppercase,
    VariantId::Unicode,
    VariantId::Reserved,
];

// The historical ordering is reserved for v1 records. V2 resolves names.
const EDGE_CASES: [VariantId; 5] = [
    VariantId::Empty,
    VariantId::LongLowercase,
    VariantId::LongUppercase,
    VariantId::Unicode,
    VariantId::Reserved,
];

/// The `unconstrained` catalog, in enumeration order. Names (not positions)
/// identify entries in saved cases.
const UNCONSTRAINED: [VariantId; 8] = [
    VariantId::Empty,
    VariantId::LongLowercase,
    VariantId::NulByte,
    VariantId::HugeUppercase,
    VariantId::ScriptTag,
    VariantId::SqlQuote,
    VariantId::TemplateSyntax,
    VariantId::NullString,
];

impl VariantId {
    fn json_value(self) -> Option<ReplacementValue> {
        let value = match self {
            Self::Empty => String::new(),
            Self::LongLowercase => "a".repeat(200),
            Self::LongUppercase => "A".repeat(200),
            Self::Unicode => "测试".into(),
            Self::Reserved => "RESERVED".into(),
            Self::NulByte => "\u{0000}".into(),
            Self::HugeUppercase => "A".repeat(65536),
            Self::ScriptTag => "</script>".into(),
            Self::SqlQuote => "' OR '1'='1".into(),
            Self::TemplateSyntax => "${}".into(),
            Self::NullString => "null".into(),
            // Schema-derived and custom values depend on the step.
            _ => return None,
        };
        Some(ReplacementValue::JsonString(value))
    }
}

/// The `edge_cases` or `unconstrained` catalog for an environment variable,
/// as (stable name, value) in catalog order: every entry but `nul_byte`,
/// which an environment value cannot carry.
pub(crate) fn envar_catalog(unconstrained: bool) -> Vec<(String, String)> {
    let catalog: &[VariantId] = if unconstrained { &UNCONSTRAINED } else { &EDGE_CASES };
    let mut entries: Vec<(String, String)> = catalog
        .iter()
        .filter(|id| **id != VariantId::NulByte)
        .filter_map(|id| {
            let Some(ReplacementValue::JsonString(text)) = id.json_value() else {
                return None;
            };
            let name = serde_json::to_value(id).ok()?.get("kind")?.as_str()?.to_owned();
            Some((name, text))
        })
        .collect();
    // An environment value can carry bytes JSON and headers cannot.
    if unconstrained {
        entries.push(("non_unicode".into(), crate::adapters::cli::osvalue::non_unicode()));
    }
    entries
}

/// Longest string `mode: schema` generates for `maxLength` cases.
const MAX_GENERATED_LENGTH: u64 = 1 << 20;
/// Largest `maxItems` for which `mode: schema` builds an oversized array.
const MAX_GENERATED_ITEMS: u64 = 1000;

/// The named values a descriptor enumerates, in order: the mode's catalog
/// followed by the mutator's custom `values`.
fn field_variants(step: &Step, descriptor: &PartDescriptor) -> Vec<(VariantId, ReplacementValue)> {
    if descriptor.input_kind != InputKind::JsonField {
        return http_variants(step, descriptor);
    }
    json_variants(step, descriptor)
}

/// Header and path values for `use: http`. Headers skip what the client
/// cannot send (a NUL byte); schema values that are not text go as text.
fn http_variants(step: &Step, descriptor: &PartDescriptor) -> Vec<(VariantId, ReplacementValue)> {
    let header = descriptor.input_kind == InputKind::HeaderField;
    let as_text = |value: ReplacementValue| -> Option<ReplacementValue> {
        let text = match value {
            ReplacementValue::JsonString(text) | ReplacementValue::HeaderAscii(text) => text,
            ReplacementValue::JsonValue(Value::String(text)) => text,
            ReplacementValue::JsonValue(other) => other.to_string(),
            ReplacementValue::Absent => return Some(ReplacementValue::Absent),
        };
        if header {
            text.chars()
                .all(crate::mutation::text::header_safe)
                .then_some(ReplacementValue::HeaderAscii(text))
        } else {
            Some(ReplacementValue::JsonString(text))
        }
    };
    for (index, mutator) in step.mutators.iter().enumerate() {
        let MutatorDecl::Http { fields, mode, values, field_schemas, lines } = mutator else {
            continue;
        };
        if !descriptor.uses_mutator(index) {
            continue;
        }
        if !fields.iter().any(|field| field.selector() == descriptor.canonical_selector) {
            continue;
        }
        let catalog: Vec<(VariantId, ReplacementValue)> = match mode {
            JsonMutatorMode::EdgeCases => EDGE_CASES.iter().filter_map(|id| Some((*id, id.json_value()?))).collect(),
            JsonMutatorMode::Unconstrained => {
                UNCONSTRAINED.iter().filter_map(|id| Some((*id, id.json_value()?))).collect()
            }
            JsonMutatorMode::Schema => field_schemas
                .get(&descriptor.canonical_selector)
                .map(schema_variants)
                .unwrap_or_default(),
            JsonMutatorMode::Absent => vec![(VariantId::Absent, ReplacementValue::Absent)],
            JsonMutatorMode::Values
            | JsonMutatorMode::Lines
            | JsonMutatorMode::Alphanumeric
            | JsonMutatorMode::Text(_)
            | JsonMutatorMode::Env(_) => Vec::new(),
        };
        let custom = values
            .iter()
            .enumerate()
            .map(|(index, value)| (VariantId::Custom(index), ReplacementValue::JsonString(value.clone())));
        // Header lines a header cannot carry are left out by `as_text`.
        return catalog
            .into_iter()
            .chain(line_variants(lines.as_ref()))
            .chain(custom)
            .filter_map(|(id, value)| Some((id, as_text(value)?)))
            .collect();
    }
    Vec::new()
}

/// One variant per line of a `path` file, in file order.
fn line_variants(lines: Option<&crate::experiment::types::ValueLines>) -> Vec<(VariantId, ReplacementValue)> {
    let Some(lines) = lines else {
        return Vec::new();
    };
    let file = lines.fingerprint();
    lines
        .lines
        .iter()
        .enumerate()
        .map(|(index, line)| (VariantId::Line { line: index + 1, file }, ReplacementValue::JsonString(line.clone())))
        .collect()
}

fn json_variants(step: &Step, descriptor: &PartDescriptor) -> Vec<(VariantId, ReplacementValue)> {
    let Some(field) = descriptor.canonical_selector.strip_prefix("body.") else {
        return Vec::new();
    };
    for (index, mutator) in step.mutators.iter().enumerate() {
        let MutatorDecl::Json {
            fields,
            mode,
            values,
            field_schemas,
            lines,
        } = mutator
        else {
            continue;
        };
        if !descriptor.uses_mutator(index) {
            continue;
        }
        if !fields.iter().flatten().any(|candidate| candidate == field) {
            continue;
        }
        let static_catalog = |catalog: &[VariantId]| -> Vec<(VariantId, ReplacementValue)> {
            catalog
                .iter()
                .filter_map(|id| Some((*id, id.json_value()?)))
                .collect()
        };
        let mut variants = match mode {
            JsonMutatorMode::EdgeCases => static_catalog(&EDGE_CASES),
            JsonMutatorMode::Unconstrained => static_catalog(&UNCONSTRAINED),
            JsonMutatorMode::Schema => field_schemas
                .get(field)
                .map(schema_variants)
                .unwrap_or_default(),
            JsonMutatorMode::Alphanumeric
            | JsonMutatorMode::Text(_)
            | JsonMutatorMode::Absent
            | JsonMutatorMode::Values
            | JsonMutatorMode::Lines
            | JsonMutatorMode::Env(_) => Vec::new(),
        };
        variants.extend(line_variants(lines.as_ref()));
        variants.extend(values.iter().enumerate().map(|(index, value)| {
            let value = match value {
                Value::String(text) => ReplacementValue::JsonString(text.clone()),
                other => ReplacementValue::JsonValue(other.clone()),
            };
            (VariantId::Custom(index), value)
        }));
        return variants;
    }
    Vec::new()
}

/// Values that probe the edges of a field's JSON Schema: a missing required
/// field, `null`, wrong types, and each declared bound just outside (and,
/// for `maxLength`, exactly at) its limit.
fn schema_variants(field: &FieldSchema) -> Vec<(VariantId, ReplacementValue)> {
    use VariantId::*;
    let schema = &field.schema;
    let types: Vec<&str> = match schema.get("type") {
        Some(Value::String(kind)) => vec![kind.as_str()],
        Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    let has = |kind: &str| types.contains(&kind);
    let numeric = has("integer") || has("number");
    let integer_only = has("integer") && !has("number");
    let json = ReplacementValue::JsonValue;
    let text = |value: String| ReplacementValue::JsonString(value);
    let unsigned = |key: &str| schema.get(key).and_then(Value::as_u64);

    let mut out = Vec::new();
    if field.required {
        out.push((MissingField, ReplacementValue::Absent));
    }
    // Wrong types only make sense when the schema restricts the type.
    if !types.is_empty() {
        if !has("null") {
            out.push((NullValue, json(Value::Null)));
        }
        if !has("string") {
            out.push((WrongTypeString, json(Value::from("nadir"))));
        }
        if !numeric {
            out.push((WrongTypeNumber, json(Value::from(12345))));
        }
        if !has("boolean") {
            out.push((WrongTypeBoolean, json(Value::Bool(true))));
        }
        if !has("array") {
            out.push((WrongTypeArray, json(Value::Array(Vec::new()))));
        }
        if !has("object") {
            out.push((WrongTypeObject, json(Value::Object(Default::default()))));
        }
    }
    if has("string") {
        if let Some(min) = unsigned("minLength").filter(|min| *min >= 1) {
            out.push((TooShort, text("a".repeat(min as usize - 1))));
        }
        if let Some(max) = unsigned("maxLength").filter(|max| *max < MAX_GENERATED_LENGTH) {
            out.push((AtMaxLength, text("a".repeat(max as usize))));
            out.push((OverMaxLength, text("a".repeat(max as usize + 1))));
        }
        if let Some(format) = schema.get("format").and_then(Value::as_str) {
            out.push((BadFormat, text(format!("not-a-valid-{format}"))));
        }
    }
    if numeric {
        let exclusive_min = schema.get("exclusiveMinimum");
        let exclusive_max = schema.get("exclusiveMaximum");
        // OpenAPI 3.1 states exclusive bounds as numbers; 3.0 as booleans.
        let below = match (exclusive_min.filter(|bound| bound.is_number()), schema.get("minimum")) {
            (Some(bound), _) => Some(bound.clone()),
            (None, Some(min)) if exclusive_min == Some(&Value::Bool(true)) => Some(min.clone()),
            (None, Some(min)) => offset(min, -1),
            (None, None) => None,
        };
        let above = match (exclusive_max.filter(|bound| bound.is_number()), schema.get("maximum")) {
            (Some(bound), _) => Some(bound.clone()),
            (None, Some(max)) if exclusive_max == Some(&Value::Bool(true)) => Some(max.clone()),
            (None, Some(max)) => offset(max, 1),
            (None, None) => None,
        };
        let has_lower_bound = below.is_some();
        if let Some(value) = below {
            out.push((BelowMinimum, json(value)));
        }
        if let Some(value) = above {
            out.push((AboveMaximum, json(value)));
        }
        if !has_lower_bound {
            out.push((NegativeOne, json(Value::from(-1))));
        }
        if integer_only {
            out.push((Fraction, json(Value::from(1.5))));
            out.push((IntegerOverflow, json(Value::from(u64::MAX))));
        } else {
            out.push((HugeNumber, json(Value::from(f64::MAX))));
        }
    }
    if has("array") {
        if unsigned("minItems").is_some_and(|min| min >= 1) {
            out.push((EmptyArray, json(Value::Array(Vec::new()))));
        }
        if let Some(max) = unsigned("maxItems").filter(|max| *max < MAX_GENERATED_ITEMS) {
            out.push((OverMaxItems, json(Value::Array(vec![Value::Null; max as usize + 1]))));
        }
    }
    if has("object")
        && schema
            .get("required")
            .and_then(Value::as_array)
            .is_some_and(|required| !required.is_empty())
    {
        out.push((EmptyObject, json(Value::Object(Default::default()))));
    }
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array) {
        let outsider = if allowed.iter().all(Value::is_number) {
            Value::from(987_654_321)
        } else {
            Value::from("nadir-not-in-enum")
        };
        if !allowed.contains(&outsider) {
            let value = match outsider {
                Value::String(outsider) => text(outsider),
                other => json(other),
            };
            out.push((NotInEnum, value));
        }
    }
    out
}

/// `bound + delta`, keeping integers as integers.
fn offset(bound: &Value, delta: i64) -> Option<Value> {
    if let Some(integer) = bound.as_i64() {
        return Some(Value::from(integer.saturating_add(delta)));
    }
    bound.as_f64().map(|float| Value::from(float + delta as f64))
}

impl RegressionVariant {
    /// The line of a `path` file this variant was saved from, if any.
    pub(crate) fn saved_line(&self) -> Option<usize> {
        match self.identity {
            VariantIdentity::Stable { variant_id: VariantId::Line { line, .. } } => Some(line),
            _ => None,
        }
    }

    pub(crate) fn from_replacement(
        experiment: &Experiment,
        descriptor: &PartDescriptor,
        value: &ReplacementValue,
    ) -> Option<Self> {
        let step = experiment
            .steps
            .iter()
            .find(|step| step.id == descriptor.step_id)?;
        let variant_id = field_variants(step, descriptor)
            .into_iter()
            .find(|(_, candidate)| candidate == value)?
            .0;
        Some(Self {
            step_id: descriptor.step_id.clone(),
            selector: descriptor.canonical_selector.clone(),
            identity: VariantIdentity::Stable { variant_id },
        })
    }

    /// Whether this variant sends a value its field's schema forbids, so the
    /// service is expected to reject the request.
    pub(crate) fn expects_rejection(&self) -> bool {
        use VariantId::*;
        matches!(
            self.identity,
            VariantIdentity::Stable {
                variant_id: MissingField
                    | NullValue
                    | WrongTypeString
                    | WrongTypeNumber
                    | WrongTypeBoolean
                    | WrongTypeArray
                    | WrongTypeObject
                    | TooShort
                    | OverMaxLength
                    | BadFormat
                    | NotInEnum
                    | BelowMinimum
                    | AboveMaximum
                    | Fraction
                    | IntegerOverflow
                    | HugeNumber
                    | EmptyArray
                    | OverMaxItems
                    | EmptyObject
            }
        )
    }

    pub(crate) fn resolve(
        &self,
        experiment: &Experiment,
        artifact_version: u32,
    ) -> Option<ReplacementValue> {
        let step = experiment
            .steps
            .iter()
            .find(|step| step.id == self.step_id)?;
        build_descriptors(experiment)
            .into_iter()
            .filter(|descriptor| {
                descriptor.step_id == self.step_id && descriptor.canonical_selector == self.selector
            })
            .find_map(|descriptor| {
                let id = match (&self.identity, artifact_version, descriptor.input_kind) {
                    // `use: headers` saved its values by position.
                    (VariantIdentity::Stable { variant_id: VariantId::Header(option) }, 2, _)
                    | (VariantIdentity::Legacy { choice: option }, 1, InputKind::HeaderField) => {
                        VariantId::Custom(*option)
                    }
                    (VariantIdentity::Stable { variant_id }, 2, _) => *variant_id,
                    (VariantIdentity::Legacy { choice }, 1, InputKind::JsonField) => {
                        *EDGE_CASES_V1.get(*choice)?
                    }
                    _ => return None,
                };
                // Only names the step currently enumerates resolve.
                field_variants(step, &descriptor)
                    .into_iter()
                    .find(|(candidate, _)| *candidate == id)
                    .map(|(_, value)| value)
            })
    }
}

/// Generates single-field enumeration variants.
pub struct VariantGenerator;

impl VariantGenerator {
    /// Generate all single-field variants for an experiment.
    ///
    /// The contract provides the values the baseline request sends (the
    /// operation example plus bindings), used to skip "unchanged" variants.
    pub fn enumerate(experiment: &Experiment, contract: Option<&ResolvedContract>) -> Vec<CasePlan> {
        Self::enumerate_with(experiment, |step, selector| baseline_value(contract?, step, selector))
    }

    pub(crate) fn enumerate_with(
        experiment: &Experiment,
        baseline: impl Fn(&Step, &str) -> Option<ReplacementValue>,
    ) -> Vec<CasePlan> {
        // 1. Build the canonical descriptor list
        let descriptors = build_descriptors(experiment);
        if descriptors.is_empty() {
            return Vec::new();
        }

        // 2. Collect baseline values for each descriptor
        let baseline_values = collect_baseline_values(experiment, baseline, &descriptors);

        // 3. Generate all variant values
        let mut all_variants: Vec<(usize, ReplacementValue)> = Vec::new();
        let mut seen: HashSet<(usize, ReplacementValue)> = HashSet::new();

        for (desc_idx, descriptor) in descriptors.iter().enumerate() {
            let step = experiment
                .steps
                .iter()
                .find(|s| s.id == descriptor.step_id)
                .expect("descriptor references unknown step");

            let values = enumerate_values_for_descriptor(step, descriptor);

            for value in values {
                // Skip duplicates
                if !seen.insert((desc_idx, value.clone())) {
                    continue;
                }
                // Skip unchanged (equals baseline)
                if let Some(baseline) = baseline_values.get(&desc_idx) {
                    if *baseline == value {
                        continue;
                    }
                }
                all_variants.push((desc_idx, value));
            }
        }

        // 4. Build case plans: one per variant (single-field replacement)
        let mut plans = Vec::new();
        for (desc_idx, value) in all_variants {
            let mut replacements: Vec<Option<ReplacementValue>> = vec![None; descriptors.len()];
            replacements[desc_idx] = Some(value);

            plans.push(CasePlan {
                descriptors: descriptors.clone(),
                replacements,
            });
        }

        plans
    }
}

// ── Descriptor construction ────────────────────────────────────────

pub(crate) fn build_descriptors(experiment: &Experiment) -> Vec<PartDescriptor> {
    let mut descriptors = Vec::new();

    for step in &experiment.steps {
        // Collect bound field names for this step
        let bound_fields: HashSet<String> = step
            .bindings
            .iter()
            .filter_map(|b| {
                if let crate::experiment::selectors::InputSelector::BodyField(path) = &b.selector {
                    Some(path.join("."))
                } else {
                    None
                }
            })
            .collect();

        for (mutator_index, mutator) in step.mutators.iter().enumerate() {
            match mutator {
                // Malformations are enumerated cases, not parts to mutate.
                MutatorDecl::Malform { .. } | MutatorDecl::EnvFile { .. } => {}
                // Variables with `mode` are random parts; fixed `value`/`values`
                // are enumerated cases.
                MutatorDecl::Envar { variables, .. } => {
                    for variable in variables {
                        let mode = match variable.rule {
                            crate::experiment::types::EnvarVariableRule::Mode(mode) => JsonMutatorMode::Env(mode),
                            crate::experiment::types::EnvarVariableRule::Text(generator) => JsonMutatorMode::Text(generator),
                            _ => continue,
                        };
                        descriptors.push(PartDescriptor {
                            step_id: step.id.clone(),
                            canonical_selector: variable.name.clone(),
                            input_kind: InputKind::EnvVar,
                            representation: ValueRepresentation::JsonString,
                            mutation_mode: Some(mode),
                            mutator_index: Some(mutator_index),
                        });
                    }
                }
                MutatorDecl::Json { fields, mode, .. } => {
                    let field_list = fields.clone().unwrap_or_default();
                    for field in &field_list {
                        let canonical = format!("body.{}", field);
                        // Skip bound fields
                        if bound_fields.contains(field) {
                            continue;
                        }
                        descriptors.push(PartDescriptor {
                            step_id: step.id.clone(),
                            canonical_selector: canonical,
                            input_kind: InputKind::JsonField,
                            representation: ValueRepresentation::JsonString,
                            mutation_mode: Some(*mode),
                            mutator_index: Some(mutator_index),
                        });
                    }
                }
                // Bound header and path values are mutated too: each case
                // replaces the bound value.
                MutatorDecl::Http { fields, mode, .. } => {
                    for field in fields {
                        let (input_kind, representation) = if field.is_header() {
                            (InputKind::HeaderField, ValueRepresentation::HeaderAscii)
                        } else {
                            (InputKind::PathField, ValueRepresentation::JsonString)
                        };
                        descriptors.push(PartDescriptor {
                            step_id: step.id.clone(),
                            canonical_selector: field.selector(),
                            input_kind,
                            representation,
                            mutation_mode: Some(*mode),
                            mutator_index: Some(mutator_index),
                        });
                    }
                }
            }
        }
    }

    descriptors
}

// ── Value enumeration ──────────────────────────────────────────────

fn enumerate_values_for_descriptor(
    step: &Step,
    descriptor: &PartDescriptor,
) -> Vec<ReplacementValue> {
    field_variants(step, descriptor).into_iter().map(|(_, value)| value).collect()
}

// ── Baseline value collection ──────────────────────────────────────

/// The value each JSON descriptor has in the baseline request body.
///
/// Mutated body fields cannot be bound (static validation), so their
/// baseline value is static: the operation's request example with literal
/// bindings applied. Header and path values come from literal bindings; an
/// unbound or runtime-bound one has no static baseline value.
fn collect_baseline_values(
    experiment: &Experiment,
    baseline: impl Fn(&Step, &str) -> Option<ReplacementValue>,
    descriptors: &[PartDescriptor],
) -> HashMap<usize, ReplacementValue> {
    descriptors
        .iter()
        .enumerate()
        .filter_map(|(index, descriptor)| {
            let step = experiment.steps.iter().find(|step| step.id == descriptor.step_id)?;
            let value = baseline(step, &descriptor.canonical_selector)?;
            Some((index, value))
        })
        .collect()
}

// ── Token dictionary ───────────────────────────────────────────────

/// Build a token dictionary from the experiment's static configuration.
pub fn build_token_dictionary(experiment: &Experiment) -> Vec<Vec<u8>> {
    let mut tokens: Vec<Vec<u8>> = Vec::new();
    let max_tokens = 1024;
    let max_token_size = 256;

    for step in &experiment.steps {
        for mutator in &step.mutators {
            match mutator {
                MutatorDecl::Malform { .. } | MutatorDecl::EnvFile { .. } => {}
                MutatorDecl::Envar { .. } => {}
                MutatorDecl::Http { values, .. } => {
                    for v in values {
                        let bytes = v.as_bytes().to_vec();
                        if bytes.len() <= max_token_size && tokens.len() < max_tokens {
                            tokens.push(bytes);
                        }
                    }
                }
                MutatorDecl::Json { .. } => {
                    // Add common JSON tokens
                    let common = [
                        "null",
                        "true",
                        "false",
                        "0",
                        "\"\"",
                        "{}",
                        "[]",
                        "\"admin\"",
                        "\"root\"",
                        "\"test\"",
                    ];
                    for t in &common {
                        let bytes = t.as_bytes().to_vec();
                        if tokens.len() < max_tokens {
                            tokens.push(bytes);
                        }
                    }
                }
            }
        }
    }

    tokens.truncate(max_tokens);
    tokens
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envar_catalogs_are_the_shared_ones_without_nul() {
        let names = |unconstrained| envar_catalog(unconstrained).into_iter().map(|(name, _)| name).collect::<Vec<_>>();
        assert_eq!(names(false), ["empty", "long_lowercase", "long_uppercase", "unicode", "reserved"]);
        assert_eq!(
            names(true),
            ["empty", "long_lowercase", "huge_uppercase", "script_tag", "sql_quote", "template_syntax", "null_string", "non_unicode"]
        );
        // No value reaches the process with a NUL (`non_unicode` is encoded).
        use std::os::unix::ffi::OsStrExt;
        assert!(envar_catalog(true).iter().all(|(_, value)| !crate::adapters::cli::osvalue::decode(value).as_bytes().contains(&0)));
        assert_eq!(envar_catalog(true)[2].1.len(), 65536);
    }
    use crate::adapters::http::contract;
    use crate::config;
    use crate::experiment;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn examples_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples")
    }

    /// An experiment over `/items/{id}` with a declared `X-Level` header.
    fn http_experiment(mutators: &str) -> Experiment {
        let directory = std::env::temp_dir().join(format!("nadir-http-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("openapi.yaml"), r#"
openapi: 3.1.0
info: {title: t, version: "1"}
paths:
  /items/{id}:
    get:
      operationId: getItem
      parameters:
        - {name: id, in: path, required: true, schema: {type: string, maxLength: 4}}
        - {name: X-Level, in: header, required: true, schema: {type: integer, enum: [1, 2]}}
      responses: {"200": {description: OK}}
"#).unwrap();
        std::fs::write(directory.join("experiment.yaml"), format!(r#"
version: 1
experiment:
  name: http
  max_cases: 100
  adapters:
    http: {{base_url: "http://127.0.0.1:9", openapi: ./openapi.yaml}}
  steps:
    - id: item
      adapter: http
      operation_id: getItem
      bind: {{path.id: abc, header.X-Level: "1"}}
      cases:
        mutators:
{mutators}
"#)).unwrap();
        let resolved = config::load_resolved(&directory.join("experiment.yaml")).unwrap();
        let _ = std::fs::remove_dir_all(&directory);
        Arc::try_unwrap(resolved.experiment).unwrap_or_else(|shared| (*shared).clone())
    }

    fn variants_for(experiment: &Experiment, selector: &str) -> Vec<(VariantId, ReplacementValue)> {
        let descriptor = build_descriptors(experiment)
            .into_iter()
            .find(|descriptor| descriptor.canonical_selector == selector)
            .unwrap();
        field_variants(&experiment.steps[0], &descriptor)
    }

    #[test]
    fn http_catalogs_follow_what_each_location_can_carry() {
        let experiment = http_experiment(r#"
          - use: http
            fields: [header.X-Trace, path.id]
            mode: unconstrained
            values: ["../x"]"#);
        let header = variants_for(&experiment, "header.X-Trace");
        assert!(header.iter().all(|(id, value)| *id != VariantId::NulByte
            && matches!(value, ReplacementValue::HeaderAscii(_))));
        assert!(header.contains(&(VariantId::Custom(0), ReplacementValue::HeaderAscii("../x".into()))));
        let path = variants_for(&experiment, "path.id");
        assert!(path.contains(&(VariantId::NulByte, ReplacementValue::JsonString("\u{0}".into()))));
        assert_eq!(path.len(), header.len() + 1);
    }

    #[test]
    fn http_schema_mode_uses_the_parameter_schema_and_bound_values_mutate() {
        let experiment = http_experiment(r#"
          - use: http
            fields: [header.X-Level, path.id]
            mode: schema
          - use: http
            fields: [header.X-Gone]
            mode: absent"#);
        let level = variants_for(&experiment, "header.X-Level");
        let text = |value: &str| ReplacementValue::HeaderAscii(value.into());
        assert!(level.contains(&(VariantId::MissingField, ReplacementValue::Absent)));
        assert!(level.contains(&(VariantId::WrongTypeString, text("nadir"))));
        assert!(level.contains(&(VariantId::NullValue, text("null"))));
        assert!(level.contains(&(VariantId::NotInEnum, text("987654321"))));
        let path = variants_for(&experiment, "path.id");
        assert!(path.contains(&(VariantId::OverMaxLength, ReplacementValue::JsonString("aaaaa".into()))));
        assert_eq!(variants_for(&experiment, "header.X-Gone"), vec![(VariantId::Absent, ReplacementValue::Absent)]);
        // Both bound values are mutated: one plan per variant.
        let plans = VariantGenerator::enumerate_with(&experiment, |_, _| None);
        assert_eq!(plans.len(), level.len() + path.len() + 1);
    }

    #[test]
    fn each_mutator_of_a_part_uses_its_own_catalog() {
        let experiment = http_experiment(r#"
          - use: http
            fields: [header.X-Trace]
            mode: absent
          - use: http
            fields: [header.X-Trace]
            mode: values
            values: ["kept"]"#);
        let catalogs: Vec<Vec<ReplacementValue>> = build_descriptors(&experiment)
            .iter()
            .filter(|descriptor| descriptor.canonical_selector == "header.X-Trace")
            .map(|descriptor| enumerate_values_for_descriptor(&experiment.steps[0], descriptor))
            .collect();
        assert_eq!(catalogs, vec![
            vec![ReplacementValue::Absent],
            vec![ReplacementValue::HeaderAscii("kept".into())],
        ]);
        // Enumeration runs both, once each.
        let plans = VariantGenerator::enumerate_with(&experiment, |_, _| None);
        assert_eq!(plans.len(), 2);
    }

    #[test]
    fn records_saved_by_use_headers_still_replay() {
        let experiment = http_experiment(r#"
          - use: headers
            fields: [X-Old]
            values: ["a", "b"]"#);
        let saved: RegressionVariant = serde_json::from_value(serde_json::json!({
            "step_id": "item", "selector": "header.X-Old", "variant_id": {"kind": "header", "option": 1}
        })).unwrap();
        assert_eq!(saved.resolve(&experiment, 2), Some(ReplacementValue::HeaderAscii("b".into())));
        let legacy: RegressionVariant = serde_json::from_value(serde_json::json!({
            "step_id": "item", "selector": "header.X-Old", "choice": 0
        })).unwrap();
        assert_eq!(legacy.resolve(&experiment, 1), Some(ReplacementValue::HeaderAscii("a".into())));
    }

    fn load_experiment(name: &str) -> Experiment {
        let path = examples_dir().join(format!("{}/experiment.yaml", name));
        let raw = config::load(&path).unwrap();
        let openapi_path = examples_dir().join("create-record/openapi.yaml");
        let contract = contract::load_contract(&openapi_path).unwrap();
        let mut contracts = HashMap::new();
        contracts.insert("./openapi.yaml".into(), contract.clone());
        contracts.insert("../create-record/openapi.yaml".into(), contract);
        experiment::resolve_experiment(raw, &contracts, &examples_dir()).unwrap()
    }

    fn create_record_contract() -> ResolvedContract {
        contract::load_contract(&examples_dir().join("create-record/openapi.yaml")).unwrap()
    }

    #[test]
    fn resolves_named_catalog_entry_independently_of_legacy_position() {
        let experiment = load_experiment("create-record");
        let v2: RegressionVariant = serde_json::from_value(serde_json::json!({
            "step_id": "post_record", "selector": "body.title",
            "variant_id": { "kind": "long_uppercase" }
        }))
        .unwrap();
        let v1: RegressionVariant = serde_json::from_value(serde_json::json!({
            "step_id": "post_record", "selector": "body.title", "choice": 2
        }))
        .unwrap();
        let expected = Some(ReplacementValue::JsonString("A".repeat(200)));
        assert_eq!(v2.resolve(&experiment, 2), expected);
        assert_eq!(v1.resolve(&experiment, 1), expected);
        assert_eq!(v1.resolve(&experiment, 2), None);
    }

    #[test]
    fn test_enumerates_json_values() {
        let exp = load_experiment("create-record");
        let contract = create_record_contract();
        let plans = VariantGenerator::enumerate(&exp, Some(&contract));

        // post_record has a json mutator on "title" — should generate variants
        let has_title_variant = plans.iter().any(|p| {
            p.descriptors
                .iter()
                .any(|d| d.canonical_selector == "body.title")
        });
        assert!(
            has_title_variant || plans.is_empty(),
            "expected title variants (or empty if all skip)"
        );
    }

    #[test]
    fn test_enumerates_headers() {
        let exp = load_experiment("create-record");
        let contract = create_record_contract();
        let plans = VariantGenerator::enumerate(&exp, Some(&contract));

        // get_template has a header mutator on Accept-Language with values [en, es]
        let has_header_variant = plans.iter().any(|p| {
            p.descriptors
                .iter()
                .any(|d| d.canonical_selector == "header.Accept-Language")
        });
        assert!(has_header_variant);
    }

    #[test]
    fn test_skips_bound_fields() {
        let exp = load_experiment("create-record");
        let contract = create_record_contract();
        let plans = VariantGenerator::enumerate(&exp, Some(&contract));

        // template_id is bound (not in mutators), reference is bound too
        // Verify no descriptor targets bound fields
        for plan in &plans {
            for desc in &plan.descriptors {
                assert!(
                    !desc.canonical_selector.contains("template_id"),
                    "bound field template_id should not be mutable"
                );
                assert!(
                    !desc.canonical_selector.contains("reference"),
                    "bound field reference should not be mutable"
                );
            }
        }
    }

    #[test]
    fn test_token_dictionary_size_limit() {
        let exp = load_experiment("create-record");
        let tokens = build_token_dictionary(&exp);
        assert!(
            tokens.len() <= 1024,
            "dictionary too large: {}",
            tokens.len()
        );
    }

    #[test]
    fn test_token_dictionary_contains_expected() {
        let exp = load_experiment("create-record");
        let tokens = build_token_dictionary(&exp);
        // Should contain header values from Accept-Language mutator
        let has_en = tokens.iter().any(|t| t == b"en");
        let has_es = tokens.iter().any(|t| t == b"es");
        assert!(has_en || has_es, "dictionary should contain header values");
    }

    #[test]
    fn test_skips_values_equal_to_the_request_example() {
        let mut exp = load_experiment("create-record");
        let contract = create_record_contract();
        let title_variants = |exp: &Experiment| {
            VariantGenerator::enumerate(exp, Some(&contract))
                .iter()
                .filter(|plan| {
                    plan.descriptors
                        .iter()
                        .zip(&plan.replacements)
                        .any(|(d, r)| r.is_some() && d.canonical_selector == "body.title")
                })
                .count()
        };
        assert_eq!(title_variants(&exp), 5);

        // The baseline request now sends "RESERVED", so that variant is unchanged.
        let step = exp.steps.iter_mut().find(|step| step.id == "post_record")
            .unwrap();
        step.body_example = Some(serde_json::json!({"title": "RESERVED"}));
        assert_eq!(title_variants(&exp), 4);
    }

    #[test]
    fn unconstrained_values_round_trip_through_stable_names() {
        let experiment = load_experiment("fuzz-probe");
        let descriptor = build_descriptors(&experiment).remove(0);
        assert_eq!(descriptor.mutation_mode, Some(JsonMutatorMode::Unconstrained));
        let step = &experiment.steps[0];
        let values = enumerate_values_for_descriptor(step, &descriptor);
        assert_eq!(values.len(), 8);
        let mut names = Vec::new();
        for value in &values {
            let variant = RegressionVariant::from_replacement(&experiment, &descriptor, value)
                .unwrap_or_else(|| panic!("no stable name for {value:?}"));
            let saved = serde_json::to_value(&variant).unwrap();
            names.push(saved["variant_id"]["kind"].as_str().unwrap().to_owned());
            let loaded: RegressionVariant = serde_json::from_value(saved).unwrap();
            assert_eq!(loaded.resolve(&experiment, 2).as_ref(), Some(value));
        }
        assert_eq!(
            names,
            [
                "empty", "long_lowercase", "nul_byte", "huge_uppercase",
                "script_tag", "sql_quote", "template_syntax", "null_string"
            ]
        );
        // A name from the other catalog does not resolve for this step.
        let foreign: RegressionVariant = serde_json::from_value(serde_json::json!({
            "step_id": "probe", "selector": "body.value",
            "variant_id": { "kind": "unicode" }
        }))
        .unwrap();
        assert_eq!(foreign.resolve(&experiment, 2), None);
    }

    fn names(field: FieldSchema) -> Vec<String> {
        schema_variants(&field)
            .into_iter()
            .map(|(id, _)| {
                serde_json::to_value(id).unwrap()["kind"].as_str().unwrap().to_owned()
            })
            .collect()
    }

    fn value_of(field: FieldSchema, name: &str) -> ReplacementValue {
        schema_variants(&field)
            .into_iter()
            .find(|(id, _)| serde_json::to_value(id).unwrap()["kind"] == name)
            .unwrap_or_else(|| panic!("no {name}"))
            .1
    }

    fn field(schema: Value, required: bool) -> FieldSchema {
        FieldSchema { schema, required }
    }

    #[test]
    fn schema_variants_for_strings() {
        let title = field(
            serde_json::json!({"type": "string", "minLength": 2, "maxLength": 4, "format": "email"}),
            true,
        );
        assert_eq!(
            names(title.clone()),
            [
                "missing_field", "null_value", "wrong_type_number", "wrong_type_boolean",
                "wrong_type_array", "wrong_type_object", "too_short", "at_max_length",
                "over_max_length", "bad_format"
            ]
        );
        assert_eq!(value_of(title.clone(), "missing_field"), ReplacementValue::Absent);
        assert_eq!(value_of(title.clone(), "too_short"), ReplacementValue::JsonString("a".into()));
        assert_eq!(value_of(title.clone(), "at_max_length"), ReplacementValue::JsonString("aaaa".into()));
        assert_eq!(value_of(title.clone(), "over_max_length"), ReplacementValue::JsonString("aaaaa".into()));
        assert_eq!(
            value_of(title, "bad_format"),
            ReplacementValue::JsonString("not-a-valid-email".into())
        );

        // Optional, nullable, enum-restricted.
        let status = field(serde_json::json!({"type": ["string", "null"], "enum": ["a", "b"]}), false);
        let names = names(status.clone());
        assert!(!names.contains(&"missing_field".to_string()));
        assert!(!names.contains(&"null_value".to_string()));
        assert_eq!(
            value_of(status, "not_in_enum"),
            ReplacementValue::JsonString("nadir-not-in-enum".into())
        );
    }

    #[test]
    fn schema_variants_for_numbers_arrays_and_objects() {
        let json = ReplacementValue::JsonValue;
        let count = field(serde_json::json!({"type": "integer", "minimum": 1, "maximum": 10}), false);
        assert_eq!(value_of(count.clone(), "below_minimum"), json(Value::from(0)));
        assert_eq!(value_of(count.clone(), "above_maximum"), json(Value::from(11)));
        assert_eq!(value_of(count.clone(), "fraction"), json(Value::from(1.5)));
        assert_eq!(value_of(count.clone(), "integer_overflow"), json(Value::from(u64::MAX)));
        assert!(!names(count).contains(&"negative_one".to_string()));

        // OpenAPI 3.1 numeric exclusive bounds are themselves out of range.
        let ratio = field(serde_json::json!({"type": "number", "exclusiveMinimum": 0, "exclusiveMaximum": 1}), false);
        assert_eq!(value_of(ratio.clone(), "below_minimum"), json(Value::from(0)));
        assert_eq!(value_of(ratio.clone(), "above_maximum"), json(Value::from(1)));
        assert_eq!(value_of(ratio, "huge_number"), json(Value::from(f64::MAX)));

        let unbounded = field(serde_json::json!({"type": "number"}), false);
        assert_eq!(value_of(unbounded, "negative_one"), json(Value::from(-1)));

        let tags = field(serde_json::json!({"type": "array", "minItems": 1, "maxItems": 2}), false);
        assert_eq!(value_of(tags.clone(), "empty_array"), json(serde_json::json!([])));
        assert_eq!(value_of(tags, "over_max_items"), json(serde_json::json!([null, null, null])));

        let metadata = field(serde_json::json!({"type": "object", "required": ["source"]}), false);
        assert_eq!(value_of(metadata, "empty_object"), json(serde_json::json!({})));

        // Without a declared type, only the requirement can be probed.
        assert_eq!(names(field(serde_json::json!({}), true)), ["missing_field"]);
    }

    #[test]
    fn schema_mode_reads_nested_field_schemas_and_custom_values() {
        let mut experiment = load_experiment("create-record");
        let step = experiment.steps.iter_mut().find(|step| step.id == "post_record").unwrap();
        // Replace the edge_cases mutator with a schema one on a nested field.
        step.bindings.retain(|binding| binding.selector.to_string() != "body.metadata.source");
        step.mutators = vec![MutatorDecl::Json {
            fields: Some(vec!["metadata.source".into()]),
            mode: JsonMutatorMode::Schema,
            values: vec![Value::from("custom"), serde_json::json!({"k": 1})],
            lines: None,
            field_schemas: [(
                "metadata.source".to_string(),
                field(serde_json::json!({"type": "string", "minLength": 1}), true),
            )]
            .into_iter()
            .collect(),
        }];
        let descriptor = build_descriptors(&experiment)
            .into_iter()
            .find(|d| d.canonical_selector == "body.metadata.source")
            .unwrap();
        let step = experiment.steps.iter().find(|step| step.id == "post_record").unwrap();
        let values = enumerate_values_for_descriptor(step, &descriptor);
        assert_eq!(values.first(), Some(&ReplacementValue::Absent));
        assert_eq!(values[values.len() - 2], ReplacementValue::JsonString("custom".into()));
        assert_eq!(values.last(), Some(&ReplacementValue::JsonValue(serde_json::json!({"k": 1}))));
        for value in &values {
            let variant = RegressionVariant::from_replacement(&experiment, &descriptor, value).unwrap();
            assert_eq!(variant.resolve(&experiment, 2).as_ref(), Some(value));
            let saved = serde_json::to_value(&variant).unwrap();
            let rejects = !matches!(saved["variant_id"]["kind"].as_str(), Some("custom"));
            assert_eq!(variant.expects_rejection(), rejects, "{saved}");
        }
    }
    #[test]
    fn test_detects_unchanged() {
        let exp = load_experiment("create-record");
        let contract = create_record_contract();
        let plans = VariantGenerator::enumerate(&exp, Some(&contract));

        // The baseline title is "Default Record" — none of the generated
        // values should match this exactly
        for plan in &plans {
            for (idx, replacement) in plan.replacements.iter().enumerate() {
                if let Some(ReplacementValue::JsonString(ref s)) = replacement {
                    assert_ne!(
                        s, "Default Record",
                        "unchanged value not filtered for descriptor idx {}",
                        idx
                    );
                }
            }
        }
    }
}
