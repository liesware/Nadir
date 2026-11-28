//! Request inspection for `print` and `debug`: a protocol-neutral view of a
//! request, the hook called around each send, and plain-text rendering.
//!
//! # Design notes
//!
//! **Chosen:** rendering is pure (no terminal access) so it can be tested and
//! reused by the interactive display and by plain-text output. Values are
//! redacted before they are escaped, and escaping only changes what is shown:
//! the request is always sent byte for byte.

use crate::experiment::types::Step;

/// Shown instead of a hidden value.
pub const HIDDEN_PLACEHOLDER: &str = "[hidden]";
/// Values shorter than this are not hidden (they would match everywhere).
pub const MIN_HIDDEN_LEN: usize = 4;

/// A request as it will be written on the wire, minus transport details.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestView {
    pub method: String,
    /// Path and query, as sent.
    pub target: String,
    /// `host[:port]` of the base URL.
    pub host: String,
    /// Headers Nadir sets, in order (the HTTP client may add its own).
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    /// `http_raw`: the exact bytes sent, shown instead of the fields above.
    pub raw: Option<Vec<u8>>,
}

/// What came back for the last request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponseView {
    Received { status: u16, body: Vec<u8> },
    Failed { error: String },
    /// A `protocol: cli` run, already summarized.
    Process { summary: String },
}

/// The template of one step for `print`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestTemplate {
    pub step_id: String,
    pub request: RequestView,
    /// One line per mutator or example: what replaces each placeholder.
    pub notes: Vec<String>,
}

/// What to do with a request shown by `debug`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Send,
    Stop,
}

/// Called around each request of a step with `debug`. `secrets` are the
/// values to redact for this request: the environment variables the
/// experiment reads and the secret outputs of the current case.
pub trait RequestInspector {
    fn before_send(&mut self, step: &Step, request: &RequestView, secrets: &[String]) -> Control;
    fn after_send(&mut self, step: &Step, response: &ResponseView, secrets: &[String]);
}

/// How to render a request.
#[derive(Debug, Clone, Copy)]
pub struct RenderOptions<'a> {
    /// Values shown as `[hidden]`.
    pub secrets: &'a [String],
    /// Show `Content-Length: {{len}}` instead of the real length.
    pub template: bool,
    /// Characters per body row.
    pub width: usize,
    /// Most body rows; the rest is summarized.
    pub max_body_rows: usize,
}

/// Most rows shown for raw bytes; the rest is summarized.
const MAX_RAW_ROWS: usize = 40;

/// Render a request as HTTP/1.1 text, one entry per row.
pub fn render_request(request: &RequestView, options: &RenderOptions<'_>) -> Vec<String> {
    if let Some(raw) = &request.raw {
        return raw_rows(raw, options);
    }
    let mut rows = vec![
        format!("{} {} HTTP/1.1", request.method, show(&request.target, options.secrets)),
        format!("Host: {}", request.host),
    ];
    for (name, value) in &request.headers {
        rows.push(format!("{name}: {}", show(value, options.secrets)));
    }
    if let Some(body) = &request.body {
        rows.push(if options.template {
            "Content-Length: {{len}}".to_owned()
        } else {
            format!("Content-Length: {}", body.len())
        });
        rows.push(String::new());
        rows.extend(body_rows(body, options));
    }
    rows
}

/// One row describing a response.
pub fn render_response(response: &ResponseView, secrets: &[String], width: usize) -> String {
    let text = match response {
        ResponseView::Received { status, body } => {
            let body = show(&String::from_utf8_lossy(body), secrets);
            format!("response: {status} {body}")
        }
        ResponseView::Failed { error } => format!("response: {}", show(error, secrets)),
        ResponseView::Process { summary } => format!("result: {}", show(summary, secrets)),
    };
    clip(&text, width)
}

/// The exact bytes of an `http_raw` request: one row per `\r\n`, with a
/// lone `\r` or `\n` shown as `\r`/`\n` so a malformation is visible.
fn raw_rows(raw: &[u8], options: &RenderOptions<'_>) -> Vec<String> {
    let text = String::from_utf8_lossy(raw);
    let lines: Vec<&str> = text.split("\r\n").collect();
    let width = options.width.max(1);
    let mut rows: Vec<String> = lines
        .iter()
        .take(MAX_RAW_ROWS)
        .map(|line| clip(&show(&line.replace('\r', "\\r").replace('\n', "\\n"), options.secrets), width))
        .collect();
    if lines.len() > MAX_RAW_ROWS {
        let shown: usize = lines[..MAX_RAW_ROWS].iter().map(|line| line.len() + 2).sum();
        rows.push(format!(
            "… ({} more rows, {} more bytes)",
            lines.len() - MAX_RAW_ROWS,
            raw.len().saturating_sub(shown)
        ));
    }
    rows
}

fn body_rows(body: &[u8], options: &RenderOptions<'_>) -> Vec<String> {
    let text = show(&String::from_utf8_lossy(body), options.secrets);
    let chars: Vec<char> = text.chars().collect();
    let width = options.width.max(1);
    let rows = chars.len().div_ceil(width);
    let shown = rows.min(options.max_body_rows.max(1));
    let mut output: Vec<String> = chars
        .chunks(width)
        .take(shown)
        .map(|row| row.iter().collect())
        .collect();
    if rows > shown {
        // Report what is left of the original body, in bytes.
        let shown_chars: String = chars[..shown * width].iter().collect();
        let hidden = body.len().saturating_sub(original_bytes(&shown_chars, body));
        output.push(format!("… ({hidden} more bytes)"));
    }
    output
}

/// Bytes of the original body covered by a displayed prefix. Escapes and
/// placeholders make the display longer, so this is an estimate that never
/// exceeds the body.
fn original_bytes(shown: &str, body: &[u8]) -> usize {
    shown.len().min(body.len())
}

fn clip(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    let mut clipped: String = text.chars().take(width.saturating_sub(1)).collect();
    clipped.push('…');
    clipped
}

/// Redact secrets, then escape characters that would hide or rearrange text.
pub fn show(text: &str, secrets: &[String]) -> String {
    escape(&redact(text, secrets))
}

/// Replace every hidden value of at least four characters, longest first.
pub fn redact(text: &str, secrets: &[String]) -> String {
    let mut secrets: Vec<&String> = secrets
        .iter()
        .filter(|secret| secret.chars().count() >= MIN_HIDDEN_LEN)
        .collect();
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    let mut output = text.to_owned();
    for secret in secrets {
        output = output.replace(secret.as_str(), HIDDEN_PLACEHOLDER);
    }
    output
}

/// Show control, invisible, bidirectional and noncharacter code points as
/// `\u{XXXX}`.
pub fn escape(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    for character in text.chars() {
        if needs_escape(character) {
            output.push_str(&format!("\\u{{{:04X}}}", u32::from(character)));
        } else {
            output.push(character);
        }
    }
    output
}

fn needs_escape(character: char) -> bool {
    let code = u32::from(character);
    character.is_control()
        || matches!(code,
            0x061C | 0x200B..=0x200F | 0x2028..=0x202E | 0x2060..=0x2069 | 0xFEFF
            | 0xFDD0..=0xFDEF)
        || code & 0xFFFE == 0xFFFE
}

/// Decode `%XX` escapes for display (template paths hold `{{…}}` markers).
pub fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let hex = |offset: usize| bytes.get(index + offset).and_then(|byte| (*byte as char).to_digit(16));
        match (bytes[index], hex(1), hex(2)) {
            (b'%', Some(high), Some(low)) => {
                output.push((high * 16 + low) as u8);
                index += 3;
            }
            (byte, _, _) => {
                output.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(secrets: &[String]) -> RenderOptions<'_> {
        RenderOptions { secrets, template: false, width: 40, max_body_rows: 3 }
    }

    fn request(body: &str) -> RequestView {
        RequestView {
            method: "POST".into(),
            target: "/sign/abc".into(),
            host: "127.0.0.1:3000".into(),
            headers: vec![
                ("X-API-Key".into(), "key-123456".into()),
                ("Authorization".into(), "Bearer anything".into()),
                ("Content-Type".into(), "application/json".into()),
            ],
            body: Some(body.as_bytes().to_vec()),
            raw: None,
        }
    }

    #[test]
    fn renders_http_text_with_real_length_and_hidden_values_only() {
        let secrets = vec!["key-123456".to_owned()];
        let rows = render_request(&request(r#"{"echo":"key-123456"}"#), &options(&secrets));
        assert_eq!(rows, vec![
            "POST /sign/abc HTTP/1.1",
            "Host: 127.0.0.1:3000",
            "X-API-Key: [hidden]",
            "Authorization: Bearer anything",
            "Content-Type: application/json",
            "Content-Length: 21",
            "",
            r#"{"echo":"[hidden]"}"#,
        ]);
    }

    #[test]
    fn template_shows_a_length_placeholder_and_keeps_markers() {
        let template = RenderOptions { template: true, ..options(&[]) };
        let rows = render_request(&request(r#"{"{{raw}}":"{{raw}}"}"#), &template);
        assert!(rows.contains(&"Content-Length: {{len}}".to_owned()));
        assert!(rows.contains(&"Authorization: Bearer anything".to_owned()));
        assert_eq!(rows.last().unwrap(), r#"{"{{raw}}":"{{raw}}"}"#);
    }

    #[test]
    fn escapes_invisible_and_bidirectional_characters_only_for_display() {
        assert_eq!(escape("a\u{0}b\u{7F}c\u{202E}d\u{FEFF}é中😀\u{FFFF}\u{10FFFF}"),
            "a\\u{0000}b\\u{007F}c\\u{202E}d\\u{FEFF}é中😀\\u{FFFF}\\u{10FFFF}");
        assert_eq!(escape("plain text"), "plain text");
    }

    #[test]
    fn long_bodies_are_cut_with_the_remaining_bytes() {
        let body = "x".repeat(200);
        let rows = render_request(&request(&body), &options(&[]));
        let body_rows: Vec<_> = rows.iter().skip_while(|row| !row.is_empty()).skip(1).collect();
        assert_eq!(body_rows.len(), 4);
        assert_eq!(body_rows[3], "… (80 more bytes)");
    }

    #[test]
    fn short_secrets_are_not_redacted_and_longest_wins() {
        let secrets = vec!["abc".to_owned(), "token".to_owned(), "token-long".to_owned()];
        assert_eq!(redact("abc token token-long", &secrets), "abc [hidden] [hidden]");
    }

    #[test]
    fn responses_fit_one_row() {
        let response = ResponseView::Received { status: 400, body: b"{\"error\":\"bad hex\"}".to_vec() };
        assert_eq!(render_response(&response, &[], 80), "response: 400 {\"error\":\"bad hex\"}");
        assert_eq!(render_response(&response, &[], 25).chars().count(), 25);
    }

    #[test]
    fn raw_requests_show_their_exact_bytes() {
        let mut view = request("");
        view.raw = Some(b"P0ST /x HTTP/1.1\r\nHost: h\nX-K: key-123456\r\nX-Nul: a\0b\r\n\r\nbody".to_vec());
        let secrets = vec!["key-123456".to_owned()];
        assert_eq!(render_request(&view, &options(&secrets)), vec![
            "P0ST /x HTTP/1.1",
            "Host: h\\nX-K: [hidden]",
            "X-Nul: a\\u{0000}b",
            "",
            "body",
        ]);
        view.raw = Some((0..100).flat_map(|index| format!("X-{index}: v\r\n").into_bytes()).collect());
        let rows = render_request(&view, &options(&[]));
        assert_eq!(rows.len(), MAX_RAW_ROWS + 1);
        assert!(rows.last().unwrap().starts_with("… (61 more rows"), "{rows:?}");
    }

    #[test]
    fn decodes_percent_escapes_for_display() {
        assert_eq!(percent_decode("/items/%7B%7Benv.ID%7D%7D?q=%2"), "/items/{{env.ID}}?q=%2");
    }
}
