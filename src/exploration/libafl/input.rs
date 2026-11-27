//! LibAFL input wrapper: MultipartInput ↔ CasePlan conversion.
//!
//! Maps Nadir's typed case plans to LibAFL's `MultipartInput` format
//! and back. Each mutable part becomes a named `BytesInput`.

use crate::experiment::types::ReplacementValue;
use crate::mutation::{CasePlan, PartDescriptor};

/// Convert a `CasePlan` to a set of part values for LibAFL.
///
/// Returns a vector of `(part_name, value_bytes)` pairs in canonical order.
pub fn case_plan_to_parts(plan: &CasePlan) -> Vec<(String, Vec<u8>)> {
    plan.descriptors
        .iter()
        .zip(plan.replacements.iter())
        .map(|(desc, repl)| {
            let value = match repl {
                Some(ReplacementValue::JsonString(s)) => s.as_bytes().to_vec(),
                Some(ReplacementValue::HeaderAscii(s)) => s.as_bytes().to_vec(),
                Some(ReplacementValue::JsonValue(v)) => {
                    serde_json::to_vec(v).unwrap_or_default()
                }
                Some(ReplacementValue::Absent) => Vec::new(),
                None => Vec::new(), // baseline value
            };
            (desc.canonical_selector.clone(), value)
        })
        .collect()
}

/// Convert LibAFL part values back to a `CasePlan`.
///
/// Uses the canonical descriptors to map part indices back to replacements.
pub fn parts_to_case_plan(
    parts: &[(String, Vec<u8>)],
    descriptors: &[PartDescriptor],
) -> Result<CasePlan, InputConversionError> {
    if parts.len() != descriptors.len() {
        return Err(InputConversionError::PartCountMismatch {
            expected: descriptors.len(),
            got: parts.len(),
        });
    }

    let replacements: Vec<Option<ReplacementValue>> = parts
        .iter()
        .zip(descriptors.iter())
        .map(|((name, bytes), desc)| {
            if name != &desc.canonical_selector {
                return Err(InputConversionError::PartNameMismatch {
                    expected: desc.canonical_selector.clone(),
                    got: name.clone(),
                });
            }
            if bytes.is_empty() {
                Ok(None)
            } else {
                let value = match desc.representation {
                    crate::mutation::ValueRepresentation::JsonString => {
                        let s = String::from_utf8_lossy(bytes).to_string();
                        ReplacementValue::JsonString(s)
                    }
                    crate::mutation::ValueRepresentation::HeaderAscii => {
                        let s = String::from_utf8_lossy(bytes).to_string();
                        ReplacementValue::HeaderAscii(s)
                    }
                };
                Ok(Some(value))
            }
        })
        .collect::<Result<_, _>>()?;

    Ok(CasePlan {
        descriptors: descriptors.to_vec(),
        replacements,
    })
}

/// Errors during input conversion.
#[derive(Debug, thiserror::Error)]
pub enum InputConversionError {
    #[error("part count mismatch: expected {expected}, got {got}")]
    PartCountMismatch { expected: usize, got: usize },
    #[error("part name mismatch: expected '{expected}', got '{got}'")]
    PartNameMismatch { expected: String, got: String },
    #[error("invalid UTF-8 in JSON part: {0}")]
    InvalidUtf8(String),
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutation::{InputKind, ValueRepresentation};

    #[test]
    fn test_round_trip() {
        let descriptors = vec![
            PartDescriptor {
                step_id: "s1".into(),
                canonical_selector: "body.title".into(),
                input_kind: InputKind::JsonField,
                representation: ValueRepresentation::JsonString,
                mutation_mode: None,
                mutator_index: None,
            },
            PartDescriptor {
                step_id: "s2".into(),
                canonical_selector: "header.Accept".into(),
                input_kind: InputKind::HeaderField,
                representation: ValueRepresentation::HeaderAscii,
                mutation_mode: None,
                mutator_index: None,
            },
        ];

        let plan = CasePlan {
            descriptors: descriptors.clone(),
            replacements: vec![
                Some(ReplacementValue::JsonString("test".into())),
                Some(ReplacementValue::HeaderAscii("en".into())),
            ],
        };

        let parts = case_plan_to_parts(&plan);
        let recovered = parts_to_case_plan(&parts, &descriptors).unwrap();

        assert_eq!(recovered.replacements.len(), 2);
        assert_eq!(
            recovered.replacements[0],
            Some(ReplacementValue::JsonString("test".into()))
        );
        assert_eq!(
            recovered.replacements[1],
            Some(ReplacementValue::HeaderAscii("en".into()))
        );
    }

    #[test]
    fn test_empty_parts_mean_none() {
        let descriptors = vec![PartDescriptor {
            step_id: "s1".into(),
            canonical_selector: "body.x".into(),
            input_kind: InputKind::JsonField,
            representation: ValueRepresentation::JsonString,
            mutation_mode: None,
            mutator_index: None,
        }];

        let parts = vec![("body.x".into(), Vec::new())];
        let plan = parts_to_case_plan(&parts, &descriptors).unwrap();
        assert_eq!(plan.replacements[0], None);
    }
}
