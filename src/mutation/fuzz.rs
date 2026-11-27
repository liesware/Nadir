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
use crate::experiment::types::{Experiment, ReplacementValue};

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
}

/// Generate random case `iteration` for `seed`. Returns `None` when the
/// experiment declares no mutable fields.
pub fn generate_case(
    experiment: &Experiment,
    contract: Option<&ResolvedContract>,
    seed: u64,
    iteration: u64,
) -> Option<FuzzCase> {
    let descriptors = build_descriptors(experiment);
    if descriptors.is_empty() {
        return None;
    }
    let baseline = collect_baseline_values(experiment, contract, &descriptors);
    let tokens = tokens(experiment);
    let mut rng = Rng::for_case(seed, iteration);

    let first = rng.below(descriptors.len());
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

    let choice = rng.below(if header { 3 } else { 4 });
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
        // Header values must be visible ASCII; anything else fails before sending.
        ReplacementValue::HeaderAscii(
            text.chars()
                .filter(|character| (' '..='~').contains(character))
                .collect(),
        )
    } else {
        ReplacementValue::JsonString(text)
    }
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
struct Rng(u64);

impl Rng {
    fn for_case(seed: u64, iteration: u64) -> Self {
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
    fn below(&mut self, n: usize) -> usize {
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
    use crate::adapters::http::contract;
    use crate::{config, experiment};
    use std::collections::HashMap;
    use std::path::PathBuf;

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
                        crate::experiment::types::MutatorDecl::Headers { fields, .. } => fields
                            .iter()
                            .any(|field| selector == &format!("header.{field}")),
                    }),
                    "{selector} is not mutable in {}",
                    case.step_id
                );
                match value {
                    ReplacementValue::HeaderAscii(text) => {
                        assert!(text.chars().all(|c| (' '..='~').contains(&c)), "{text:?}")
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
}
