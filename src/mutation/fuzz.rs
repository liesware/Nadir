//! Random case generation for `strategy: fuzz`.
//!
//! # Design notes
//!
//! **Chosen:** every random case is a pure function of
//! `(experiment, contract, seed, iteration)`. There is no corpus or feedback
//! between cases (no coverage), so a saved case is replayed by regenerating
//! it from its seed and iteration instead of storing request values.
//!
//! A case mutates one to three fields of a single step. Each value is drawn
//! from the field's catalog (including `mode: schema` values and custom
//! `values`), from text mutations of the baseline value, or from extreme JSON
//! values of any type.

use serde_json::Value;

use super::{
    build_descriptors, build_token_dictionary, collect_baseline_values,
    enumerate_values_for_descriptor, InputKind, PartDescriptor,
};
use crate::adapters::http::contract::ResolvedContract;
use crate::experiment::types::{Experiment, JsonExample, JsonMutatorMode, ReplacementValue};
use super::text::Alphabet;

#[derive(Debug, Clone)]
pub struct KeyMutator {
    pub(crate) key: String,
    pub(crate) alphabet: Alphabet,
    pub(crate) sibling_names: Vec<String>,
    pub(crate) expect_status: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyRenameCase {
    pub(crate) action: JsonExample,
    pub(crate) expect_status: Option<u16>,
}

impl KeyRenameCase {
    pub(crate) fn expectation(&self) -> Option<(String, u16)> {
        match &self.action {
            JsonExample::Rename { key, .. } => self.expect_status.map(|status| (key.clone(), status)),
            JsonExample::Syntax { .. } | JsonExample::Edits { .. } => None,
        }
    }
}

/// Most fields one random case changes.
const MAX_FIELDS_PER_CASE: usize = 3;
/// Longest string a text mutation produces.
const MAX_TEXT_LENGTH: usize = 100_000;

/// Tokens inserted by text mutations: injection syntax, encodings, and
/// boundary numbers that commonly break parsers and validators.
const TOKENS: &[&str] = &[
    "'", "\"", "\\", "`", "%s%s%s", "%n", "%00", "{{7*7}}", "${7*7}", "<script>",
    "</script>", "../../", "..\\..\\", "\0", "\r\n", "' OR '1'='1", "\"; --", "null",
    "undefined", "NaN", "-1", "0", "2147483648", "9223372036854775808", "1e309", "true",
    "[]", "{}", "😀", "\u{202e}", "\u{feff}", "测试",
];

/// Characters a text mutation substitutes into the value.
const CHARACTERS: &[char] = &[
    '\'', '"', '\\', '\0', '\n', '\r', '\t', '<', '>', '&', '%', '$', '{', '}', '/', ';',
    '\u{202e}', '\u{feff}', '\u{7f}', 'é', '测', '😀',
];

/// One random case: replacements for fields of a single step.
#[derive(Debug, Clone, PartialEq)]
pub struct FuzzCase {
    pub step_id: String,
    /// `(canonical selector, value)` pairs in descriptor order.
    pub replacements: Vec<(String, ReplacementValue)>,
    /// A change to the whole JSON body: a key rename or `use: body` edits.
    pub key_rename: Option<KeyRenameCase>,
}

impl FuzzCase {
    /// Whether this case breaks the body text (`use: body`).
    pub(crate) fn edits_body(&self) -> bool {
        self.key_rename.as_ref().is_some_and(|change| matches!(change.action, JsonExample::Edits { .. }))
    }
}

/// How a random case is built from `(seed, iteration)`. Saved cases record
/// it, so `replay` regenerates them exactly even after the rule changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeneratorVersion {
    /// One to three descriptors; a part with several mutators counts once
    /// per mutator.
    V1,
    /// How many parts (1 to all distinct parts, or `max_fields`), then which
    /// ones, then which of a part's mutators.
    V2,
}

impl GeneratorVersion {
    pub const CURRENT: Self = Self::V2;

    pub fn number(self) -> u32 {
        match self {
            Self::V1 => 1,
            Self::V2 => 2,
        }
    }

    pub fn from_number(number: u32) -> Option<Self> {
        match number {
            1 => Some(Self::V1),
            2 => Some(Self::V2),
            _ => None,
        }
    }
}

/// Generate random case `iteration` for `seed` with the current rule.
/// Returns `None` when the experiment declares no mutable fields.
pub fn generate_case(
    experiment: &Experiment,
    contract: Option<&ResolvedContract>,
    seed: u64,
    iteration: u64,
) -> Option<FuzzCase> {
    generate_case_with(experiment, |step, selector| {
        crate::adapters::http::execution::baseline_value(contract?, step, selector)
    }, seed, iteration, GeneratorVersion::CURRENT, None)
}

pub(crate) fn generate_case_with(
    experiment: &Experiment,
    baseline_value: impl Fn(&crate::experiment::types::Step, &str) -> Option<ReplacementValue>,
    seed: u64,
    iteration: u64,
    version: GeneratorVersion,
    max_fields: Option<u16>,
) -> Option<FuzzCase> {
    match version {
        GeneratorVersion::V1 => generate_v1(experiment, baseline_value, seed, iteration),
        GeneratorVersion::V2 => generate_v2(experiment, baseline_value, seed, iteration, max_fields),
    }
}

/// A key rename case: one property renamed, nothing else.
fn rename_case(rng: &mut Rng, step: &crate::experiment::types::Step, key: &KeyMutator) -> FuzzCase {
    let destination = loop {
        let length = 4 + rng.below(21);
        let name: String = (0..length).map(|_| key.alphabet.pick(rng)).collect();
        if !key.sibling_names.contains(&name) {
            break name;
        }
    };
    FuzzCase {
        step_id: step.id.clone(),
        replacements: Vec::new(),
        key_rename: Some(KeyRenameCase {
            action: JsonExample::Rename { key: key.key.clone(), destination },
            expect_status: key.expect_status,
        }),
    }
}

/// The current rule: choose how many distinct parts of one step to change
/// (1 to all, capped by `max_fields`), then which ones (uniformly), then
/// which mutator each part uses.
fn generate_v2(
    experiment: &Experiment,
    baseline_value: impl Fn(&crate::experiment::types::Step, &str) -> Option<ReplacementValue>,
    seed: u64,
    iteration: u64,
    max_fields: Option<u16>,
) -> Option<FuzzCase> {
    let descriptors = build_descriptors(experiment);
    let keys: Vec<_> = experiment.steps.iter()
        .flat_map(|step| step.key_mutators.iter().map(move |key| (step, key)))
        .collect();
    // `use: body`: a unit of its own, like a key rename. Without one the
    // draws below are unchanged, so cases saved before still replay.
    let bodies: Vec<_> = experiment.steps.iter()
        .flat_map(|step| step.mutators.iter().filter_map(move |mutator| match mutator {
            crate::experiment::types::MutatorDecl::Body { alphabet, max_edits } => Some((step, *alphabet, *max_edits)),
            _ => None,
        }))
        .collect();
    // A part is one request location; it lists one descriptor per mutator.
    let mut parts: Vec<Vec<usize>> = Vec::new();
    for (index, descriptor) in descriptors.iter().enumerate() {
        let same = parts.iter_mut().find(|part| {
            let known = &descriptors[part[0]];
            known.step_id == descriptor.step_id && known.canonical_selector == descriptor.canonical_selector
        });
        match same {
            Some(part) => part.push(index),
            None => parts.push(vec![index]),
        }
    }
    if parts.is_empty() && keys.is_empty() && bodies.is_empty() {
        return None;
    }
    let mut rng = Rng::for_case(seed, iteration);
    // The first unit only decides the step (or a rename or body case).
    let first = rng.below(parts.len() + keys.len() + bodies.len());
    if let Some(index) = first.checked_sub(parts.len() + keys.len()) {
        let (step, alphabet, max_edits) = bodies[index];
        return Some(FuzzCase {
            step_id: step.id.clone(),
            replacements: Vec::new(),
            key_rename: Some(KeyRenameCase {
                action: JsonExample::Edits { edits: super::body::generate(&mut rng, alphabet, max_edits) },
                expect_status: None,
            }),
        });
    }
    if let Some(index) = first.checked_sub(parts.len()) {
        let (step, key) = keys[index];
        return Some(rename_case(&mut rng, step, key));
    }
    let step_id = descriptors[parts[first][0]].step_id.clone();
    let mut candidates: Vec<usize> = (0..parts.len())
        .filter(|&part| descriptors[parts[part][0]].step_id == step_id)
        .collect();
    let limit = max_fields.map_or(candidates.len(), |max| candidates.len().min(usize::from(max).max(1)));
    let count = 1 + rng.below(limit);
    // Partial Fisher–Yates: `count` distinct parts, each equally likely.
    for slot in 0..count {
        let pick = slot + rng.below(candidates.len() - slot);
        candidates.swap(slot, pick);
    }
    let mut chosen = candidates[..count].to_vec();
    chosen.sort_unstable();

    let baseline = collect_baseline_values(experiment, baseline_value, &descriptors);
    let tokens = tokens(experiment);
    let max_path_chars = experiment.protocols.http.as_ref()
        .map_or(crate::experiment::types::DEFAULT_MAX_PATH_CHARS, |http| http.max_path_chars);
    let step = experiment.steps.iter().find(|step| step.id == step_id)?;
    let replacements = chosen
        .into_iter()
        .map(|part| {
            let index = parts[part][rng.below(parts[part].len())];
            let descriptor = &descriptors[index];
            let catalog = enumerate_values_for_descriptor(step, descriptor);
            let value = random_value(&mut rng, descriptor, &catalog, baseline.get(&index), &tokens);
            (descriptor.canonical_selector.clone(), sendable_path(descriptor, value, max_path_chars))
        })
        .collect();
    Some(FuzzCase { step_id, replacements, key_rename: None })
}

/// A random path value cut to `max` characters (`protocols.http.max_path_chars`,
/// 4096 by default: percent-encoded, a character takes at most 12 bytes, so
/// the URL stays under the 65 534 bytes the HTTP client accepts). Cutting it
/// uses no random draws: other cases are unchanged.
fn sendable_path(descriptor: &PartDescriptor, value: ReplacementValue, max: usize) -> ReplacementValue {
    match value {
        ReplacementValue::JsonString(text)
            if descriptor.input_kind == InputKind::PathField && text.chars().count() > max =>
        {
            ReplacementValue::JsonString(text.chars().take(max).collect())
        }
        other => other,
    }
}

/// The first rule, kept to regenerate cases saved before `V2`. Do not change.
fn generate_v1(
    experiment: &Experiment,
    baseline_value: impl Fn(&crate::experiment::types::Step, &str) -> Option<ReplacementValue>,
    seed: u64,
    iteration: u64,
) -> Option<FuzzCase> {
    let descriptors = build_descriptors(experiment);
    let keys: Vec<_> = experiment.steps.iter()
        .flat_map(|step| step.key_mutators.iter().map(move |key| (step, key)))
        .collect();
    if descriptors.is_empty() && keys.is_empty() {
        return None;
    }
    let mut rng = Rng::for_case(seed, iteration);
    let first = rng.below(descriptors.len() + keys.len());
    if let Some(index) = first.checked_sub(descriptors.len()) {
        let (step, key) = keys[index];
        let destination = loop {
            let length = 4 + rng.below(21);
            let name: String = (0..length).map(|_| key.alphabet.pick(&mut rng)).collect();
            if !key.sibling_names.contains(&name) {
                break name;
            }
        };
        return Some(FuzzCase {
            step_id: step.id.clone(),
            replacements: Vec::new(),
            key_rename: Some(KeyRenameCase {
                action: JsonExample::Rename { key: key.key.clone(), destination },
                expect_status: key.expect_status,
            }),
        });
    }
    let baseline = collect_baseline_values(experiment, baseline_value, &descriptors);
    let tokens = tokens(experiment);
    let step_id = descriptors[first].step_id.clone();
    let same_step: Vec<usize> = (0..descriptors.len())
        .filter(|&index| descriptors[index].step_id == step_id)
        .collect();
    let count = 1 + rng.below(same_step.len().min(MAX_FIELDS_PER_CASE));
    let mut chosen = vec![first];
    while chosen.len() < count {
        let candidate = same_step[rng.below(same_step.len())];
        if !chosen.contains(&candidate) {
            chosen.push(candidate);
        }
    }
    chosen.sort_unstable();

    let step = experiment.steps.iter().find(|step| step.id == step_id)?;
    let replacements = chosen
        .into_iter()
        .map(|index| {
            let descriptor = &descriptors[index];
            let catalog = enumerate_values_for_descriptor(step, descriptor);
            let value = random_value(&mut rng, descriptor, &catalog, baseline.get(&index), &tokens);
            (descriptor.canonical_selector.clone(), value)
        })
        .collect();
    Some(FuzzCase {
        step_id,
        replacements,
        key_rename: None,
    })
}

fn tokens(experiment: &Experiment) -> Vec<String> {
    TOKENS
        .iter()
        .map(|token| token.to_string())
        .chain(
            build_token_dictionary(experiment)
                .into_iter()
                .map(|token| String::from_utf8_lossy(&token).into_owned()),
        )
        .collect()
}

fn random_value(
    rng: &mut Rng,
    descriptor: &PartDescriptor,
    catalog: &[ReplacementValue],
    baseline: Option<&ReplacementValue>,
    tokens: &[String],
) -> ReplacementValue {
    let header = descriptor.input_kind == InputKind::HeaderField;
    // Header values travel as header text; body and path values as strings.
    let text_value = |text: String| {
        if header {
            ReplacementValue::HeaderAscii(text)
        } else {
            ReplacementValue::JsonString(text)
        }
    };
    if descriptor.mutation_mode == Some(JsonMutatorMode::Absent) {
        return ReplacementValue::Absent;
    }
    if let Some(JsonMutatorMode::Env(mode)) = descriptor.mutation_mode {
        return ReplacementValue::JsonString(envar_value(rng, mode));
    }
    if let Some(JsonMutatorMode::Text(generator)) = descriptor.mutation_mode {
        return text_value(generator.generate(rng, ""));
    }
    if descriptor.mutation_mode == Some(JsonMutatorMode::Alphanumeric) {
        const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
        const MAX_LENGTH: usize = 256;
        let baseline_length = match baseline {
            Some(ReplacementValue::JsonString(text) | ReplacementValue::HeaderAscii(text)) => {
                text.len().min(MAX_LENGTH)
            }
            _ => 0,
        };
        let length = match rng.below(8) {
            0 => 0,
            1 => baseline_length.saturating_sub(1),
            2 => baseline_length,
            3 => (baseline_length + 1).min(MAX_LENGTH),
            4 => MAX_LENGTH,
            _ => rng.below(MAX_LENGTH + 1),
        };
        let text = (0..length)
            .map(|_| char::from(ALPHABET[rng.below(ALPHABET.len())]))
            .collect();
        return text_value(text);
    }
    // Header and path values are text: no random JSON for them.
    let textual = descriptor.input_kind != InputKind::JsonField;
    let texts: Vec<&str> = catalog
        .iter()
        .chain(baseline)
        .filter_map(|value| match value {
            ReplacementValue::JsonString(text) | ReplacementValue::HeaderAscii(text) => {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect();
    let start = |rng: &mut Rng| -> String {
        match baseline {
            Some(ReplacementValue::JsonString(text)) if rng.chance(50) => text.clone(),
            _ => rng.pick(&texts).map_or_else(|| "nadir".to_string(), |text| text.to_string()),
        }
    };

    let choice = rng.below(if textual { 3 } else { 4 });
    let text = match choice {
        0 if !catalog.is_empty() => return catalog[rng.below(catalog.len())].clone(),
        2 => {
            let joined = format!("{}{}", start(rng), start(rng));
            mutate_text(rng, joined, tokens)
        }
        3 => return ReplacementValue::JsonValue(random_json(rng, 2)),
        _ => {
            let text = start(rng);
            mutate_text(rng, text, tokens)
        }
    };
    if header {
        // Header values cannot carry control characters; non-ASCII travels
        // as UTF-8 bytes.
        text_value(text.chars().filter(|character| crate::mutation::text::header_safe(*character)).collect())
    } else {
        text_value(text)
    }
}

/// A random environment value of 1 to 64 characters from the mode's alphabet
/// (`raw`: any character but NUL).
fn envar_value(rng: &mut Rng, mode: crate::experiment::types::EnvarMode) -> String {
    use crate::experiment::types::EnvarMode;
    const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    const ALPHANUM: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    const DIGITS: &[u8] = b"0123456789";
    let alphabet = match mode {
        EnvarMode::Alpha => ALPHA,
        EnvarMode::Alphanum => ALPHANUM,
        EnvarMode::Num => DIGITS,
        EnvarMode::Raw => {
            let length = 1 + rng.below(64);
            return (0..length).map(|_| crate::mutation::text::Alphabet::EnvRaw.pick(rng)).collect();
        }
    };
    let length = 1 + rng.below(64);
    (0..length).map(|_| char::from(alphabet[rng.below(alphabet.len())])).collect()
}

/// Apply one to four random edits to `text`.
fn mutate_text(rng: &mut Rng, text: String, tokens: &[String]) -> String {
    let mut chars: Vec<char> = text.chars().collect();
    for _ in 0..1 + rng.below(4) {
        let position = rng.below(chars.len() + 1);
        match rng.below(6) {
            0 => {
                let token = &tokens[rng.below(tokens.len())];
                chars.splice(position..position, token.chars());
            }
            1 if !chars.is_empty() => {
                let end = (position + 1 + rng.below(8)).min(chars.len());
                chars.drain(position.min(end)..end);
            }
            2 if !chars.is_empty() => {
                let end = (position + 1 + rng.below(16)).min(chars.len());
                let piece: Vec<char> = chars[position.min(end)..end].to_vec();
                for _ in 0..1 + rng.below(8) {
                    chars.splice(end..end, piece.iter().copied());
                }
            }
            3 if !chars.is_empty() => {
                let index = rng.below(chars.len());
                chars[index] = CHARACTERS[rng.below(CHARACTERS.len())];
            }
            4 if !chars.is_empty() => {
                let copy = chars.clone();
                for _ in 0..1 + rng.below(50) {
                    chars.extend_from_slice(&copy);
                }
            }
            5 => chars.truncate(position),
            _ => chars.insert(position, CHARACTERS[rng.below(CHARACTERS.len())]),
        }
        chars.truncate(MAX_TEXT_LENGTH);
    }
    chars.into_iter().collect()
}

/// A JSON value of a random type, nesting at most `depth` levels.
fn random_json(rng: &mut Rng, depth: u32) -> Value {
    let kinds = if depth == 0 { 5 } else { 7 };
    match rng.below(kinds) {
        0 => Value::Null,
        1 => Value::Bool(rng.chance(50)),
        2 => {
            const INTEGERS: [i64; 8] =
                [0, -1, 1, 2_147_483_648, -2_147_483_649, 9_007_199_254_740_993, i64::MAX, i64::MIN];
            Value::from(INTEGERS[rng.below(INTEGERS.len())])
        }
        3 => {
            const FLOATS: [f64; 5] = [0.1, -0.0, 1e308, -1e308, 5e-324];
            Value::from(FLOATS[rng.below(FLOATS.len())])
        }
        4 => Value::from("A".repeat(1 + rng.below(10_000))),
        5 => Value::Array(
            (0..rng.below(4))
                .map(|_| random_json(rng, depth - 1))
                .collect(),
        ),
        _ => Value::Object(
            (0..rng.below(4))
                .map(|index| (format!("nadir{index}"), random_json(rng, depth - 1)))
                .collect(),
        ),
    }
}

/// SplitMix64: small, fast, and identical on every platform.
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn for_case(seed: u64, iteration: u64) -> Self {
        let mut rng = Self(seed ^ iteration.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        rng.next();
        rng
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number in `0..n`; `n` must be positive.
    pub(crate) fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        (!items.is_empty()).then(|| &items[self.below(items.len())])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_path_values_are_cut_to_a_sendable_length() {
        let descriptor = |input_kind| PartDescriptor {
            step_id: "s".into(),
            canonical_selector: "path.id".into(),
            input_kind,
            representation: crate::mutation::ValueRepresentation::JsonString,
            mutation_mode: None,
            mutator_index: None,
        };
        let long = ReplacementValue::JsonString("é".repeat(5000));
        match sendable_path(&descriptor(InputKind::PathField), long.clone(), 4096) {
            ReplacementValue::JsonString(text) => assert_eq!(text.chars().count(), 4096),
            other => panic!("{other:?}"),
        }
        match sendable_path(&descriptor(InputKind::PathField), long.clone(), 10) {
            ReplacementValue::JsonString(text) => assert_eq!(text, "é".repeat(10)),
            other => panic!("{other:?}"),
        }
        assert_eq!(sendable_path(&descriptor(InputKind::HeaderField), long.clone(), 10), long);
    }
    use crate::adapters::http::contract;
    use crate::experiment::types::ExplorationStrategy;
    use crate::{config, experiment};
    use std::collections::HashMap;
    use std::path::PathBuf;

    /// One step with three parts: `body.a` (two mutators), `body.b`, and
    /// `header.X-K` (`absent` or a value, so its mutator shows in the case).
    fn three_parts(max_fields: &str) -> Experiment {
        let directory = std::env::temp_dir().join(format!("nadir-parts-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("openapi.yaml"), r#"
openapi: 3.1.0
info: {title: t, version: "1"}
paths:
  /items:
    post:
      operationId: postItem
      requestBody:
        content:
          application/json:
            schema: {type: object, properties: {a: {type: string}, b: {type: string}}}
            example: {a: x, b: y}
      responses: {"200": {description: OK}}
"#).unwrap();
        std::fs::write(directory.join("experiment.yaml"), format!(r#"
version: 1
experiment:
  name: parts
  max_cases: 10
  exploration: {{strategy: fuzz, duration_s: 1, seed: 1{max_fields}}}
  adapters:
    http: {{base_url: "http://127.0.0.1:9", openapi: ./openapi.yaml}}
  steps:
    - id: item
      adapter: http
      operation_id: postItem
      cases:
        mutators:
          - {{use: json, fields: [a], mode: edge_cases}}
          - {{use: json, fields: [a, b], mode: unconstrained}}
          - {{use: http, fields: [header.X-K], mode: absent}}
          - {{use: http, fields: [header.X-K], mode: values, values: [kept]}}
"#)).unwrap();
        let resolved = config::load_resolved(&directory.join("experiment.yaml")).unwrap();
        let _ = std::fs::remove_dir_all(&directory);
        (*resolved.experiment).clone()
    }

    #[test]
    fn v2_chooses_how_many_parts_then_which_ones_then_the_mutator() {
        let experiment = three_parts("");
        let runs = 3000;
        let (mut by_count, mut by_part, mut absent) = ([0usize; 4], [0usize; 3], 0usize);
        for iteration in 0..runs {
            let case = generate_case(&experiment, None, 11, iteration).unwrap();
            assert_eq!(case, generate_case(&experiment, None, 11, iteration).unwrap());
            let selectors: Vec<&str> = case.replacements.iter().map(|(selector, _)| selector.as_str()).collect();
            let mut unique = selectors.clone();
            unique.dedup();
            assert_eq!(unique, selectors, "a part changed twice: {selectors:?}");
            by_count[selectors.len()] += 1;
            for (index, part) in ["body.a", "body.b", "header.X-K"].iter().enumerate() {
                if selectors.contains(part) {
                    by_part[index] += 1;
                }
            }
            absent += case.replacements.iter()
                .filter(|(selector, value)| selector == "header.X-K" && *value == ReplacementValue::Absent)
                .count();
        }
        let share = |count: usize, total: usize| count as f64 / total as f64;
        for count in 1..=3 {
            assert!((share(by_count[count], runs as usize) - 1.0 / 3.0).abs() < 0.05, "{by_count:?}");
        }
        // Each part enters (N+1)/(2N) = 2/3 of the cases.
        for part in by_part {
            assert!((share(part, runs as usize) - 2.0 / 3.0).abs() < 0.05, "{by_part:?}");
        }
        // `header.X-K` picks either of its two mutators.
        assert!((share(absent, by_part[2]) - 0.5).abs() < 0.06, "{absent} of {}", by_part[2]);
    }

    #[test]
    fn max_fields_caps_the_parts_of_a_case() {
        let experiment = three_parts(", max_fields: 1");
        let ExplorationStrategy::Fuzz { max_fields, .. } = experiment.exploration.strategy else {
            panic!("fuzz strategy expected");
        };
        assert_eq!(max_fields, Some(1));
        for iteration in 0..200 {
            let case = generate_case_with(&experiment, |_, _| None, 3, iteration, GeneratorVersion::V2, max_fields)
                .unwrap();
            assert_eq!(case.replacements.len(), 1);
        }
    }

    #[test]
    fn saved_cases_without_a_generator_use_the_first_rule() {
        let old: crate::artifacts::FuzzCaseRef =
            serde_json::from_value(serde_json::json!({"seed": 7, "iteration": 3})).unwrap();
        assert_eq!((old.generator, old.max_fields), (1, None));
        assert_eq!(GeneratorVersion::from_number(old.generator), Some(GeneratorVersion::V1));
        let new = crate::artifacts::FuzzCaseRef { seed: 7, iteration: 3, generator: 2, max_fields: None };
        assert_eq!(serde_json::to_value(new).unwrap(), serde_json::json!({"seed": 7, "iteration": 3, "generator": 2}));
        assert_eq!(GeneratorVersion::from_number(3), None);
    }

    /// Cases saved before V2 regenerate exactly: fingerprints of 40 V1
    /// cases per example, recorded before V2 existed.
    #[test]
    fn v1_cases_are_unchanged() {
        use sha2::{Digest, Sha256};
        for (name, expected) in [
            ("create-record", "67ea2063aab1e8e5b48f11ad8d0c27de42530b5c7aa341a245da26f878542374"),
            ("fuzz-probe", "41de68eaafa8bac30a3124b5d8407d45fa63825fea71e1ae94e2d41f551543fd"),
        ] {
            let (experiment, contract) = load(name);
            let text: String = (0..40)
                .map(|iteration| {
                    let case = generate_case_with(&experiment, |step, selector| {
                        crate::adapters::http::execution::baseline_value(&contract, step, selector)
                    }, 7, iteration, GeneratorVersion::V1, None);
                    format!("{case:?}\n")
                })
                .collect();
            assert_eq!(format!("{:x}", Sha256::digest(text.as_bytes())), expected, "{name}");
        }
    }

    fn load(name: &str) -> (Experiment, ResolvedContract) {
        let examples = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples");
        let raw = config::load(&examples.join(format!("{name}/experiment.yaml"))).unwrap();
        let contract =
            contract::load_contract(&examples.join("create-record/openapi.yaml")).unwrap();
        let contracts: HashMap<_, _> = [
            ("./openapi.yaml".to_string(), contract.clone()),
            ("../create-record/openapi.yaml".to_string(), contract.clone()),
        ]
        .into_iter()
        .collect();
        (experiment::resolve_experiment(raw, &contracts, &examples).unwrap(), contract)
    }

    #[test]
    fn cases_are_reproducible_from_seed_and_iteration() {
        let (experiment, contract) = load("create-record");
        for iteration in 0..200 {
            let first = generate_case(&experiment, Some(&contract), 42, iteration).unwrap();
            let again = generate_case(&experiment, Some(&contract), 42, iteration).unwrap();
            assert_eq!(first, again, "iteration {iteration}");
            assert!(!first.replacements.is_empty());
            assert!(first.replacements.len() <= MAX_FIELDS_PER_CASE);
        }
        let differs = (0..50).any(|iteration| {
            generate_case(&experiment, Some(&contract), 42, iteration)
                != generate_case(&experiment, Some(&contract), 43, iteration)
        });
        assert!(differs, "different seeds should give different cases");
    }

    #[test]
    fn cases_stay_within_one_step_and_send_valid_headers() {
        let (experiment, contract) = load("create-record");
        let mut steps = std::collections::BTreeSet::new();
        let mut kinds = std::collections::BTreeSet::new();
        for iteration in 0..500 {
            let case = generate_case(&experiment, Some(&contract), 7, iteration).unwrap();
            steps.insert(case.step_id.clone());
            for (selector, value) in &case.replacements {
                let step = experiment.steps.iter().find(|step| step.id == case.step_id).unwrap();
                assert!(
                    step.mutators.iter().any(|mutator| match mutator {
                        crate::experiment::types::MutatorDecl::Json { fields, .. } => fields
                            .iter()
                            .flatten()
                            .any(|field| selector == &format!("body.{field}")),
                        crate::experiment::types::MutatorDecl::Http { fields, .. } => fields
                            .iter()
                            .any(|field| selector == &field.selector()),
                        crate::experiment::types::MutatorDecl::Envar { .. } => false,
                        crate::experiment::types::MutatorDecl::Malform { .. } | crate::experiment::types::MutatorDecl::EnvFile { .. } | crate::experiment::types::MutatorDecl::Body { .. } => false,
                    }),
                    "{selector} is not mutable in {}",
                    case.step_id
                );
                match value {
                    ReplacementValue::HeaderAscii(text) => {
                        assert!(http::HeaderValue::from_str(text).is_ok(), "{text:?}")
                    }
                    ReplacementValue::JsonString(text) => {
                        assert!(text.chars().count() <= MAX_TEXT_LENGTH);
                        kinds.insert("string");
                    }
                    ReplacementValue::JsonValue(_) => {
                        kinds.insert("json");
                    }
                    ReplacementValue::Absent => {
                        kinds.insert("absent");
                    }
                }
            }
        }
        assert_eq!(steps.len(), 2, "both mutable steps should be fuzzed: {steps:?}");
        assert!(kinds.contains("string") && kinds.contains("json"), "{kinds:?}");
    }

    #[test]
    fn no_mutable_fields_means_no_cases() {
        let (mut experiment, contract) = load("create-record");
        for step in &mut experiment.steps {
            step.mutators.clear();
        }
        assert_eq!(generate_case(&experiment, Some(&contract), 1, 0), None);
    }

    #[test]
    fn alphanumeric_mutations_generate_only_strings_of_varied_lengths() {
        let (mut experiment, contract) = load("create-record");
        for step in &mut experiment.steps {
            if step.id != "post_record" {
                step.mutators.clear();
                continue;
            }
            let [crate::experiment::types::MutatorDecl::Json { mode, .. }] =
                step.mutators.as_mut_slice()
            else {
                panic!("expected one JSON mutator");
            };
            *mode = JsonMutatorMode::Alphanumeric;
        }

        let mut lengths = std::collections::BTreeSet::new();
        for iteration in 0..100 {
            let case = generate_case(&experiment, Some(&contract), 1, iteration).unwrap();
            assert_eq!(case.step_id, "post_record");
            let [(selector, ReplacementValue::JsonString(text))] = case.replacements.as_slice()
            else {
                panic!("expected one string replacement");
            };
            assert_eq!(selector, "body.title");
            assert!(text.bytes().all(|byte| byte.is_ascii_alphanumeric()));
            assert!(text.len() <= 256);
            lengths.insert(text.len());
        }
        assert!(lengths.contains(&0));
        assert!(lengths.contains(&256));
        assert!(lengths.len() > 2);
    }
}
