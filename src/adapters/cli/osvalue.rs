//! Environment values that are not UTF-8.
//!
//! Nadir keeps environment values as text. A value with arbitrary bytes is
//! encoded as text that starts with NUL, `"\0bytes:"` followed by the bytes
//! in hex: no real environment value can contain NUL, so the encoding never
//! collides with one. The cli adapter decodes it when it starts the process.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;

const PREFIX: &str = "\0bytes:";

/// The `non_unicode` case value: an ASCII marker followed by bytes that are
/// not valid UTF-8.
pub fn non_unicode() -> String {
    encode(b"nadir-nonutf8\xff\xfe")
}

/// Encode `bytes` as a value the cli adapter passes byte for byte.
pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::from(PREFIX);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// The raw bytes of an encoded value, if `value` is one.
fn bytes(value: &str) -> Option<Vec<u8>> {
    let hex = value.strip_prefix(PREFIX)?;
    if hex.len() % 2 != 0 {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(hex.get(index..index + 2)?, 16).ok())
        .collect()
}

/// What the process gets for `value`.
pub fn decode(value: &str) -> OsString {
    match bytes(value) {
        Some(raw) => OsString::from_vec(raw),
        None => OsString::from(value),
    }
}

/// `value` for display: an encoded one with its invalid bytes as `\xNN`.
pub fn display(value: &str) -> String {
    match bytes(value) {
        Some(raw) => raw.utf8_chunks().fold(String::new(), |mut out, chunk| {
            out.push_str(chunk.valid());
            for byte in chunk.invalid() {
                out.push_str(&format!("\\x{byte:02x}"));
            }
            out
        }),
        None => value.to_owned(),
    }
}

/// The text to look for in the output when checking that `value` is not
/// echoed: the valid UTF-8 prefix of an encoded value, or `value` itself.
pub fn echo_token(value: &str) -> String {
    match bytes(value) {
        Some(raw) => raw.utf8_chunks().next().map(|chunk| chunk.valid().to_owned()).unwrap_or_default(),
        None => value.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_values_round_trip_and_display_their_invalid_bytes() {
        let value = non_unicode();
        assert!(value.starts_with('\0'));
        assert_eq!(decode(&value).into_vec(), b"nadir-nonutf8\xff\xfe");
        assert_eq!(display(&value), "nadir-nonutf8\\xff\\xfe");
        assert_eq!(echo_token(&value), "nadir-nonutf8");
        // Plain values pass through untouched.
        assert_eq!(decode("dev"), OsString::from("dev"));
        assert_eq!((display("dev"), echo_token("dev")), ("dev".into(), "dev".into()));
        assert_eq!(decode("\0bytes:zz"), OsString::from("\0bytes:zz"));
    }
}
