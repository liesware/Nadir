//! `use: body`: random character edits on the serialized JSON body.
//!
//! Each random case makes one to `max` edits at random positions of the
//! body the step would send: insert characters, delete a stretch, replace
//! a character or duplicate a stretch. Inserted characters come from the
//! mode's alphabet, mixed half and half with JSON's structural characters,
//! which are the ones that break a parser. Positions are drawn before the
//! body is known and reduced modulo its length when applied, so a case
//! regenerates from its seed and iteration.

use super::fuzz::Rng;
use super::text::Alphabet;

/// JSON's structural characters, and the escape.
const STRUCTURAL: &[char] = &['{', '}', '[', ']', ':', ',', '"', '\\'];

/// One edit; `at` is reduced modulo the body length when applied.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum BodyEdit {
    Insert { at: u32, text: String },
    Delete { at: u32, len: u8 },
    Replace { at: u32, with: char },
    Duplicate { at: u32, len: u8 },
}

fn character(rng: &mut Rng, alphabet: Alphabet) -> char {
    if rng.below(2) == 0 {
        STRUCTURAL[rng.below(STRUCTURAL.len())]
    } else {
        alphabet.pick(rng)
    }
}

/// One to `max_edits` random edits.
pub(crate) fn generate(rng: &mut Rng, alphabet: Alphabet, max_edits: usize) -> Vec<BodyEdit> {
    let count = 1 + rng.below(max_edits.max(1));
    (0..count)
        .map(|_| {
            let at = rng.below(1 << 30) as u32;
            match rng.below(4) {
                0 => BodyEdit::Insert { at, text: (0..1 + rng.below(4)).map(|_| character(rng, alphabet)).collect() },
                1 => BodyEdit::Delete { at, len: 1 + rng.below(8) as u8 },
                2 => BodyEdit::Replace { at, with: character(rng, alphabet) },
                _ => BodyEdit::Duplicate { at, len: 1 + rng.below(16) as u8 },
            }
        })
        .collect()
}

/// `text` with the edits applied in order (by characters, so the result
/// stays valid UTF-8).
pub(crate) fn apply(text: &str, edits: &[BodyEdit]) -> String {
    let mut chars: Vec<char> = text.chars().collect();
    for edit in edits {
        let len = chars.len();
        match edit {
            BodyEdit::Insert { at, text } => {
                let position = *at as usize % (len + 1);
                chars.splice(position..position, text.chars());
            }
            _ if len == 0 => {}
            BodyEdit::Delete { at, len: count } => {
                let start = *at as usize % len;
                let end = (start + usize::from(*count)).min(len);
                chars.drain(start..end);
            }
            BodyEdit::Replace { at, with } => {
                chars[*at as usize % len] = *with;
            }
            BodyEdit::Duplicate { at, len: count } => {
                let start = *at as usize % len;
                let end = (start + usize::from(*count)).min(len);
                let piece: Vec<char> = chars[start..end].to_vec();
                chars.splice(end..end, piece);
            }
        }
    }
    chars.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_apply_by_position_modulo_length_and_regenerate_from_the_seed() {
        let body = r#"{"a":1}"#;
        assert_eq!(apply(body, &[BodyEdit::Insert { at: 8, text: "X".into() }]), r#"X{"a":1}"#);
        assert_eq!(apply(body, &[BodyEdit::Delete { at: 1, len: 3 }]), "{:1}");
        assert_eq!(apply(body, &[BodyEdit::Replace { at: 0, with: '[' }]), r#"["a":1}"#);
        assert_eq!(apply(body, &[BodyEdit::Duplicate { at: 5, len: 9 }]), r#"{"a":1}1}"#);
        assert_eq!(apply("", &[BodyEdit::Delete { at: 3, len: 2 }, BodyEdit::Insert { at: 9, text: "}".into() }]), "}");

        let first = generate(&mut Rng::for_case(5, 2), Alphabet::Raw, 4);
        assert_eq!(first, generate(&mut Rng::for_case(5, 2), Alphabet::Raw, 4));
        assert!((1..=4).contains(&first.len()));
        // Over many cases, structural characters are inserted.
        let inserted: String = (0..200)
            .flat_map(|iteration| generate(&mut Rng::for_case(1, iteration), Alphabet::Alpha, 4))
            .filter_map(|edit| match edit {
                BodyEdit::Insert { text, .. } => Some(text),
                BodyEdit::Replace { with, .. } => Some(with.to_string()),
                _ => None,
            })
            .collect();
        assert!(inserted.contains('{') && inserted.contains('"') && inserted.chars().any(|c| c.is_ascii_alphabetic()));
    }
}
