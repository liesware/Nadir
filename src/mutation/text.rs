//! Bounded text generators and line-oriented validation corpora.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use sha2::{Digest, Sha256};

use super::fuzz::Rng;

pub(crate) const MAX_TEXT_CHARS: usize = 4096;
const MAX_CORPUS_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Alphabet {
    Alpha,
    Alphanum,
    /// Any Unicode scalar value worth probing: printable ASCII with
    /// punctuation, C0 controls and DEL, and a fixed set of non-ASCII
    /// characters (accents, other scripts, emoji, invisible and
    /// bidirectional marks, noncharacters).
    Raw,
    /// `Raw` without the characters a header value cannot carry: printable
    /// ASCII, tab, and the same non-ASCII set (sent as UTF-8 bytes).
    HeaderRaw,
    /// ASCII digits (`envar` `mode: num`).
    Num,
    /// `Raw` without NUL, the one character an environment value cannot
    /// carry (`envar` `mode: raw`).
    EnvRaw,
}

// Order matters: replay regenerates values from the seed and these tables.
const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
const ALPHANUM: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
const DIGITS: &[u8] = b"0123456789";

/// Non-ASCII characters that commonly break validation, normalization,
/// storage or rendering.
const RAW_UNICODE: &[char] = &[
    '\u{00A0}', // no-break space
    '\u{00E9}', // é
    '\u{00F1}', // ñ
    '\u{00DF}', // ß
    '\u{0130}', // İ (case mapping changes length)
    '\u{03A9}', // Ω
    '\u{0416}', // Ж
    '\u{05D0}', // א (right-to-left)
    '\u{0627}', // ا (right-to-left)
    '\u{0301}', // combining acute accent
    '\u{200B}', // zero-width space
    '\u{200D}', // zero-width joiner
    '\u{202E}', // right-to-left override
    '\u{2028}', // line separator
    '\u{2029}', // paragraph separator
    '\u{FEFF}', // byte order mark
    '\u{FFFD}', // replacement character
    '\u{FFFF}', // noncharacter
    '\u{4E2D}', // 中
    '\u{65E5}', // 日
    '\u{D55C}', // 한
    '\u{FF21}', // fullwidth A
    '\u{1F600}', // 😀 (outside the BMP)
    '\u{1F525}', // 🔥
    '\u{1D400}', // mathematical bold A
    '\u{10FFFF}', // last code point
];

/// Whether a header value can carry `character`: no controls (tab aside)
/// and no DEL. Non-ASCII characters travel as UTF-8 bytes.
pub(crate) fn header_safe(character: char) -> bool {
    character == '\t' || !character.is_control()
}

impl Alphabet {
    /// The mode name used in experiments.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Alpha => "alpha",
            Self::Alphanum => "alphanum",
            Self::Num => "num",
            Self::Raw | Self::HeaderRaw | Self::EnvRaw => "raw",
        }
    }

    /// One character drawn from the alphabet.
    pub(crate) fn pick(self, rng: &mut Rng) -> char {
        match self {
            Self::Alpha => char::from(ALPHA[rng.below(ALPHA.len())]),
            Self::Alphanum => char::from(ALPHANUM[rng.below(ALPHANUM.len())]),
            // Half printable ASCII, a fifth controls, the rest non-ASCII.
            Self::Raw => match rng.below(10) {
                0..=4 => char::from(0x20 + rng.below(0x5F) as u8),
                5 | 6 => match rng.below(33) {
                    32 => '\u{7F}',
                    control => char::from(control as u8),
                },
                _ => RAW_UNICODE[rng.below(RAW_UNICODE.len())],
            },
            // Mostly printable ASCII; a tab now and then; the rest non-ASCII.
            Self::HeaderRaw => match rng.below(10) {
                0..=5 => char::from(0x20 + rng.below(0x5F) as u8),
                6 => '\t',
                _ => RAW_UNICODE[rng.below(RAW_UNICODE.len())],
            },
            Self::Num => char::from(DIGITS[rng.below(DIGITS.len())]),
            // As `Raw`, with controls 0x01-0x1F and DEL (never NUL).
            Self::EnvRaw => match rng.below(10) {
                0..=4 => char::from(0x20 + rng.below(0x5F) as u8),
                5 | 6 => match rng.below(32) {
                    31 => '\u{7F}',
                    control => char::from(control as u8 + 1),
                },
                _ => RAW_UNICODE[rng.below(RAW_UNICODE.len())],
            },
        }
    }

    pub(crate) fn contains(self, character: char) -> bool {
        match self {
            Self::Alpha => character.is_ascii_alphabetic(),
            Self::Alphanum => character.is_ascii_alphanumeric(),
            Self::Raw => true,
            Self::HeaderRaw => header_safe(character),
            Self::Num => character.is_ascii_digit(),
            Self::EnvRaw => character != '\0',
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TextGenerator {
    pub(crate) alphabet: Alphabet,
    pub(crate) below: usize,
    pub(crate) above: usize,
    pub(crate) max: usize,
}

impl TextGenerator {
    pub(crate) fn generate(self, rng: &mut Rng, seed_text: &str) -> String {
        // Each side has equal probability irrespective of its size.
        let length = if rng.below(2) == 0 {
            rng.below(self.below)
        } else {
            self.above + 1 + rng.below(self.max - self.above)
        };
        // Lengths count characters (Unicode scalar values), not bytes.
        let mut output: String = seed_text
            .chars()
            .filter(|character| self.alphabet.contains(*character))
            .take(length)
            .collect();
        for _ in output.chars().count()..length {
            output.push(self.alphabet.pick(rng));
        }
        output
    }
}

#[derive(Debug, Clone)]
pub struct Corpus {
    pub(crate) field: String,
    pub(crate) repetitions: u16,
    pub(crate) sha256: String,
    pub(crate) generator: Option<(TextGenerator, u64)>,
    lines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CorpusCaseRef {
    pub step_id: String,
    pub field: String,
    pub corpus_index: usize,
    pub iteration: u16,
    pub line: usize,
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
}

impl Corpus {
    pub(crate) fn load(
        path: &Path,
        field: String,
        repetitions: u16,
        generator: Option<(TextGenerator, u64)>,
    ) -> io::Result<Self> {
        if !path.metadata()?.is_file() {
            return Err(invalid("corpus must be a regular file"));
        }
        let file = File::open(path)?;
        let mut bytes = Vec::new();
        file.take((MAX_CORPUS_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_CORPUS_BYTES {
            return Err(invalid("corpus exceeds 1 MiB"));
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| invalid("corpus must be UTF-8"))?;
        let lines: Vec<_> = text.lines().map(str::to_owned).collect();
        if lines.is_empty() {
            return Err(invalid("corpus has no lines"));
        }
        for (index, line) in lines.iter().enumerate() {
            if line.chars().count() > MAX_TEXT_CHARS {
                return Err(invalid(format!(
                    "corpus line {} exceeds {MAX_TEXT_CHARS} characters",
                    index + 1
                )));
            }
            if !line
                .chars()
                .all(|character| character.is_alphanumeric() || matches!(character, ' ' | '\t'))
            {
                return Err(invalid(format!(
                    "corpus line {} requires letters, digits, or horizontal whitespace",
                    index + 1
                )));
            }
        }
        Ok(Self {
            field,
            repetitions,
            sha256: format!("{:x}", Sha256::digest(&bytes)),
            generator,
            lines,
        })
    }

    pub(crate) fn entry(&self, iteration: u16) -> (usize, String) {
        let index = usize::from(iteration) % self.lines.len();
        let line = &self.lines[index];
        let value = match self.generator {
            Some((generator, seed)) => {
                generator.generate(&mut Rng::for_case(seed, u64::from(iteration)), line)
            }
            None => line.clone(),
        };
        (index + 1, value)
    }

    pub(crate) fn case(
        &self,
        step_id: &str,
        corpus_index: usize,
        iteration: u16,
    ) -> (CorpusCaseRef, crate::experiment::types::ReplacementValue) {
        let (line, value) = self.entry(iteration);
        let reference = CorpusCaseRef {
            step_id: step_id.into(),
            field: self.field.clone(),
            corpus_index,
            iteration,
            line,
            sha256: self.sha256.clone(),
            seed: self.generator.map(|(_, seed)| seed),
        };
        (
            reference,
            crate::experiment::types::ReplacementValue::JsonString(value),
        )
    }
}

/// The lines of a value file (`path`) and its SHA-256: UTF-8, up to 1 MiB,
/// at least one line, each up to 4096 characters. Blank lines are kept as
/// empty values; `\r\n` endings are accepted. `allow_nul: false` (an
/// environment value) rejects lines with NUL.
pub(crate) fn load_value_lines(path: &Path, allow_nul: bool) -> io::Result<(Vec<String>, String)> {
    if !path.metadata()?.is_file() {
        return Err(invalid("value file must be a regular file"));
    }
    let mut bytes = Vec::new();
    File::open(path)?.take((MAX_CORPUS_BYTES + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > MAX_CORPUS_BYTES {
        return Err(invalid("value file exceeds 1 MiB"));
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| invalid("value file must be UTF-8"))?;
    let lines: Vec<String> = text.lines().map(str::to_owned).collect();
    if lines.is_empty() {
        return Err(invalid("value file has no lines"));
    }
    for (index, line) in lines.iter().enumerate() {
        if line.chars().count() > MAX_TEXT_CHARS {
            return Err(invalid(format!("line {} exceeds {MAX_TEXT_CHARS} characters", index + 1)));
        }
        if !allow_nul && line.contains('\0') {
            return Err(invalid(format!("line {} contains NUL, which an environment value cannot carry", index + 1)));
        }
    }
    Ok((lines, format!("{:x}", Sha256::digest(&bytes))))
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_alphabets_never_produce_nul() {
        let mut rng = Rng::for_case(7, 0);
        let raw: String = (0..20_000).map(|_| Alphabet::EnvRaw.pick(&mut rng)).collect();
        assert!(!raw.contains('\0'));
        assert!(raw.chars().any(|character| character.is_control()), "controls are still probed");
        assert!(raw.contains('\u{7F}') && raw.contains('\u{1}') && raw.contains('\u{1F}'));
        let digits: String = (0..200).map(|_| Alphabet::Num.pick(&mut rng)).collect();
        assert!(digits.chars().all(|character| character.is_ascii_digit()));
        assert_eq!((Alphabet::Num.name(), Alphabet::EnvRaw.name()), ("num", "raw"));
    }

    #[test]
    fn value_files_keep_blank_lines_and_reject_nul() {
        let dir = std::env::temp_dir().join(format!("nadir-lines-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("values.txt");
        std::fs::write(&file, "../etc\r\n\n; rm -rf /\n").unwrap();
        let (lines, sha) = load_value_lines(&file, false).unwrap();
        assert_eq!(lines, ["../etc", "", "; rm -rf /"]);
        assert_eq!(sha.len(), 64);
        std::fs::write(&file, "ok\nbad\0value\n").unwrap();
        assert!(load_value_lines(&file, false).unwrap_err().to_string().contains("line 2 contains NUL"));
        assert_eq!(load_value_lines(&file, true).unwrap().0, ["ok", "bad\0value"]);
        std::fs::write(&file, "").unwrap();
        assert!(load_value_lines(&file, false).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn seeded_text_has_equal_length_sides_and_the_selected_alphabet() {
        for alphabet in [Alphabet::Alpha, Alphabet::Alphanum] {
            let generator = TextGenerator {
                alphabet,
                below: 4,
                above: 8,
                max: 20,
            };
            let shorter = (0..1000)
                .filter(|iteration| {
                    let generated = generator.generate(&mut Rng::for_case(7, *iteration), "Seed99");
                    let replay = generator.generate(&mut Rng::for_case(7, *iteration), "Seed99");
                    assert_eq!(generated, replay);
                    assert!(generated.chars().all(|character| alphabet.contains(character)));
                    assert!(generated.len() < 4 || (9..=20).contains(&generated.len()));
                    generated.len() < 4
                })
                .count();
            assert!((450..=550).contains(&shorter), "{shorter}");
        }
    }

    #[test]
    fn raw_text_counts_characters_and_mixes_ascii_controls_and_unicode() {
        let generator = TextGenerator { alphabet: Alphabet::Raw, below: 64, above: 64, max: 256 };
        let (mut controls, mut non_ascii, mut punctuation) = (0, 0, 0);
        for iteration in 0..500 {
            let generated = generator.generate(&mut Rng::for_case(1, iteration), "");
            assert_eq!(generated, generator.generate(&mut Rng::for_case(1, iteration), ""));
            let length = generated.chars().count();
            assert!(length < 64 || (65..=256).contains(&length), "{length}");
            controls += generated.chars().filter(char::is_ascii_control).count();
            non_ascii += generated.chars().filter(|character| !character.is_ascii()).count();
            punctuation += generated.chars().filter(char::is_ascii_punctuation).count();
        }
        assert!(controls > 0 && non_ascii > 0 && punctuation > 0);
        // Serialized as JSON, every generated value stays a valid string.
        let value = serde_json::Value::String(generator.generate(&mut Rng::for_case(1, 3), ""));
        let encoded = serde_json::to_string(&value).unwrap();
        assert_eq!(serde_json::from_str::<serde_json::Value>(&encoded).unwrap(), value);
    }

    #[test]
    fn raw_keeps_the_whole_seed_prefix() {
        let generator = TextGenerator { alphabet: Alphabet::Raw, below: 3, above: 3, max: 10 };
        for iteration in 0..50 {
            let generated = generator.generate(&mut Rng::for_case(2, iteration), "a-b!");
            let prefix: String = "a-b!".chars().take(generated.chars().count()).collect();
            assert!(generated.starts_with(&prefix), "{generated:?}");
        }
    }

    #[test]
    fn header_raw_text_never_has_controls() {
        let generator = TextGenerator { alphabet: Alphabet::HeaderRaw, below: 8, above: 8, max: 64 };
        let mut non_ascii = 0;
        for iteration in 0..300 {
            let generated = generator.generate(&mut Rng::for_case(4, iteration), "");
            assert_eq!(generated, generator.generate(&mut Rng::for_case(4, iteration), ""));
            assert!(generated.chars().all(header_safe), "{generated:?}");
            assert!(http::HeaderValue::from_str(&generated).is_ok(), "{generated:?}");
            let length = generated.chars().count();
            assert!(length < 8 || (9..=64).contains(&length));
            non_ascii += generated.chars().filter(|character| !character.is_ascii()).count();
        }
        assert!(non_ascii > 0);
    }
}
