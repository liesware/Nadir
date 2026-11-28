//! The `malform` catalog: named, deterministic ways to break a well-formed
//! HTTP/1.1 request at the byte level.
//!
//! # Design notes
//!
//! **Chosen:** each malformation starts from the same well-formed request and
//! changes one thing, so a finding points at one parser rule. Names are
//! stable: saved cases refer to them and `replay` rebuilds the bytes.

use std::fmt::Write as _;

/// A request as it is written on the wire, before any malformation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawRequest {
    pub method: String,
    /// Path and query.
    pub target: String,
    pub version: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Default size of the long URI and header values.
const HUGE: usize = 64 * 1024;
/// Default header count for `many_headers`.
const MANY_HEADERS: usize = 10_000;

/// The sizes of `huge_uri`, `huge_header` and `many_headers` (`malform`
/// `sizes`): aim them one byte past the server's documented limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MalformSizes {
    /// Total bytes of the request target (path and query) of `huge_uri`.
    pub uri: usize,
    /// Bytes of the `X-Nadir` value of `huge_header`.
    pub header: usize,
    /// Headers `many_headers` adds.
    pub headers: usize,
}

impl Default for MalformSizes {
    fn default() -> Self {
        Self { uri: HUGE, header: HUGE, headers: MANY_HEADERS }
    }
}
/// Body used by body-framing malformations when the request has none.
const FILLER_BODY: &[u8] = b"nadir-body";

impl RawRequest {
    /// The request with CRLF line endings, the empty line and the body.
    pub fn render(&self) -> Vec<u8> {
        self.render_with("\r\n", true)
    }

    fn render_with(&self, line_end: &str, terminator: bool) -> Vec<u8> {
        let mut text = format!("{} {} {}{line_end}", self.method, self.target, self.version);
        for (name, value) in &self.headers {
            let _ = write!(text, "{name}: {value}{line_end}");
        }
        if terminator {
            text.push_str(line_end);
        }
        let mut bytes = text.into_bytes();
        if terminator {
            bytes.extend_from_slice(&self.body);
        }
        bytes
    }

    fn header_index(&self, name: &str) -> Option<usize> {
        self.headers.iter().position(|(existing, _)| existing.eq_ignore_ascii_case(name))
    }

    fn remove_header(&mut self, name: &str) {
        self.headers.retain(|(existing, _)| !existing.eq_ignore_ascii_case(name));
    }

    /// Replace the header's value, or add it at the end.
    fn set_header(&mut self, name: &str, value: impl Into<String>) {
        match self.header_index(name) {
            Some(index) => self.headers[index].1 = value.into(),
            None => self.headers.push((name.into(), value.into())),
        }
    }

    /// Insert `text` at the end of the path, before any query.
    fn with_in_path(&self, text: &str) -> String {
        match self.target.split_once('?') {
            Some((path, query)) => format!("{path}{text}?{query}"),
            None => format!("{}{text}", self.target),
        }
    }

    /// The body to frame, with a filler when the request has none.
    fn framed_body(&self) -> Vec<u8> {
        if self.body.is_empty() {
            FILLER_BODY.to_vec()
        } else {
            self.body.clone()
        }
    }
}

macro_rules! malformations {
    ($($variant:ident => $name:literal,)*) => {
        /// One way to break a request. See [`Malformation::apply`].
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum Malformation {
            $($variant,)*
        }

        impl Malformation {
            /// Every malformation, in catalog order.
            pub const ALL: &'static [Malformation] = &[$(Self::$variant,)*];

            /// The stable name used in experiments and saved cases.
            pub fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => $name,)*
                }
            }

            pub fn from_name(name: &str) -> Option<Self> {
                match name {
                    $($name => Some(Self::$variant),)*
                    _ => None,
                }
            }
        }
    };
}

malformations! {
    BadMethod => "bad_method",
    LowercaseMethod => "lowercase_method",
    BadVersion => "bad_version",
    MissingVersion => "missing_version",
    DoubleSpace => "double_space",
    HugeUri => "huge_uri",
    NulInPath => "nul_in_path",
    CrInPath => "cr_in_path",
    CrlfInjection => "crlf_injection",
    BareLf => "bare_lf",
    HeaderWithoutColon => "header_without_colon",
    SpaceBeforeColon => "space_before_colon",
    FoldedHeader => "folded_header",
    ManyHeaders => "many_headers",
    HugeHeader => "huge_header",
    MissingHost => "missing_host",
    DuplicateHost => "duplicate_host",
    ShortLength => "short_length",
    LongLength => "long_length",
    NegativeLength => "negative_length",
    NonNumericLength => "non_numeric_length",
    ConflictingLengths => "conflicting_lengths",
    LengthAndChunked => "length_and_chunked",
    BadChunkSize => "bad_chunk_size",
    UnterminatedChunked => "unterminated_chunked",
    TrailingBytes => "trailing_bytes",
    NoHeaderTerminator => "no_header_terminator",
    TruncatedRequest => "truncated_request",
}

impl Malformation {
    /// The bytes of `request` with this one thing broken.
    pub fn apply(self, request: &RawRequest) -> Vec<u8> {
        self.apply_sized(request, &MalformSizes::default())
    }

    /// The bytes of `request` with this one thing broken, at these sizes.
    pub fn apply_sized(self, request: &RawRequest, sizes: &MalformSizes) -> Vec<u8> {
        let mut broken = request.clone();
        match self {
            Self::BadMethod => {
                broken.method = match broken.method.find('O') {
                    Some(index) => format!("{}0{}", &broken.method[..index], &broken.method[index + 1..]),
                    None => format!("{}0", broken.method),
                };
            }
            Self::LowercaseMethod => broken.method = broken.method.to_ascii_lowercase(),
            Self::BadVersion => broken.version = "HTTP/9.9".into(),
            Self::MissingVersion => {
                let mut bytes = format!("{} {}\r\n", broken.method, broken.target).into_bytes();
                let full = broken.render();
                let line_end = full.windows(2).position(|pair| pair == b"\r\n").unwrap_or(0) + 2;
                bytes.extend_from_slice(&full[line_end..]);
                return bytes;
            }
            Self::DoubleSpace => broken.method.push(' '),
            Self::HugeUri => {
                let separator = if broken.target.contains('?') { '&' } else { '?' };
                // Padded so the whole target is `sizes.uri` bytes long.
                let prefix = format!("{}{separator}nadir=", broken.target);
                broken.target = format!("{prefix}{}", "A".repeat(sizes.uri.saturating_sub(prefix.len())));
            }
            Self::NulInPath => broken.target = broken.with_in_path("\0"),
            Self::CrInPath => broken.target = broken.with_in_path("\r"),
            // A bare CR inside a value: the parser must reject it, not split
            // the line into a second header.
            Self::CrlfInjection => broken.headers.push(("X-Nadir".into(), "nadir\rX-Injected: 1".into())),
            Self::BareLf => return broken.render_with("\n", true),
            Self::HeaderWithoutColon => {
                let mut bytes = broken.render_with("\r\n", false);
                bytes.extend_from_slice(b"X-Nadir-No-Colon value\r\n\r\n");
                bytes.extend_from_slice(&broken.body);
                return bytes;
            }
            Self::SpaceBeforeColon => {
                let index = broken.header_index("Host").unwrap_or(0);
                if let Some(header) = broken.headers.get_mut(index) {
                    header.0.push(' ');
                }
            }
            // obs-fold: a value continued on the next line.
            Self::FoldedHeader => broken.headers.push(("X-Nadir".into(), "first\r\n second".into())),
            Self::ManyHeaders => {
                broken.headers.extend((0..sizes.headers).map(|index| (format!("X-Nadir-{index}"), "x".into())));
            }
            Self::HugeHeader => broken.headers.push(("X-Nadir".into(), "A".repeat(sizes.header))),
            Self::MissingHost => broken.remove_header("Host"),
            Self::DuplicateHost => broken.headers.push(("Host".into(), "nadir.invalid".into())),
            Self::ShortLength => {
                broken.body = broken.framed_body();
                broken.set_header("Content-Length", (broken.body.len() / 2).to_string());
            }
            Self::LongLength => {
                broken.body = broken.framed_body();
                broken.set_header("Content-Length", (broken.body.len() + 100).to_string());
            }
            Self::NegativeLength => {
                broken.body = broken.framed_body();
                broken.set_header("Content-Length", "-1");
            }
            Self::NonNumericLength => {
                broken.body = broken.framed_body();
                broken.set_header("Content-Length", "nadir");
            }
            Self::ConflictingLengths => {
                broken.body = broken.framed_body();
                let length = broken.body.len();
                broken.set_header("Content-Length", length.to_string());
                broken.headers.push(("Content-Length".into(), (length + 1).to_string()));
            }
            Self::LengthAndChunked => {
                broken.body = broken.framed_body();
                broken.set_header("Content-Length", broken.body.len().to_string());
                broken.headers.push(("Transfer-Encoding".into(), "chunked".into()));
            }
            Self::BadChunkSize => {
                let body = broken.framed_body();
                broken.remove_header("Content-Length");
                broken.set_header("Transfer-Encoding", "chunked");
                broken.body = [b"zz\r\n".as_slice(), &body, b"\r\n0\r\n\r\n"].concat();
            }
            Self::UnterminatedChunked => {
                let body = broken.framed_body();
                broken.remove_header("Content-Length");
                broken.set_header("Transfer-Encoding", "chunked");
                broken.body = [format!("{:x}\r\n", body.len()).as_bytes(), &body, b"\r\n"].concat();
            }
            Self::TrailingBytes => broken.body.extend_from_slice(b"nadir-trailing-bytes"),
            Self::NoHeaderTerminator => return broken.render_with("\r\n", false),
            Self::TruncatedRequest => {
                let headers = broken.render_with("\r\n", false);
                return headers[..headers.len() / 2].to_vec();
            }
        }
        broken.render()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_aim_the_big_cases_at_a_chosen_limit() {
        let sizes = MalformSizes { uri: 8193, header: 16385, headers: 101 };
        let text = |malformation: Malformation| String::from_utf8(malformation.apply_sized(&login(), &sizes)).unwrap();
        let first_line = text(Malformation::HugeUri).lines().next().unwrap().to_owned();
        let target = first_line.split(' ').nth(1).unwrap();
        assert_eq!(target.len(), 8193, "{}", &target[..40]);
        assert!(text(Malformation::HugeHeader).contains(&format!("X-Nadir: {}\r\n", "A".repeat(16385))));
        assert_eq!(text(Malformation::ManyHeaders).matches("\r\nX-Nadir-").count(), 101);
        assert_eq!(MalformSizes::default(), MalformSizes { uri: 65536, header: 65536, headers: 10_000 });
    }

    fn login() -> RawRequest {
        RawRequest {
            method: "POST".into(),
            target: "/api/login".into(),
            version: "HTTP/1.1".into(),
            headers: vec![
                ("Host".into(), "127.0.0.1:3000".into()),
                ("Content-Type".into(), "application/json".into()),
                ("Content-Length".into(), "31".into()),
                ("Connection".into(), "close".into()),
            ],
            body: br#"{"usuario":"ana","clave":"123"}"#.to_vec(),
        }
    }

    fn text(malformation: Malformation) -> String {
        String::from_utf8_lossy(&malformation.apply(&login())).into_owned()
    }

    const BASE_HEADERS: &str = "Host: 127.0.0.1:3000\r\nContent-Type: application/json\r\n";
    const BODY: &str = r#"{"usuario":"ana","clave":"123"}"#;

    #[test]
    fn the_base_request_is_exact_http_1_1() {
        assert_eq!(
            String::from_utf8(login().render()).unwrap(),
            format!("POST /api/login HTTP/1.1\r\n{BASE_HEADERS}Content-Length: 31\r\nConnection: close\r\n\r\n{BODY}")
        );
    }

    #[test]
    fn names_are_stable_and_unique() {
        assert_eq!(Malformation::ALL.len(), 28);
        for malformation in Malformation::ALL {
            assert_eq!(Malformation::from_name(malformation.name()), Some(*malformation));
        }
        assert_eq!(Malformation::from_name("nope"), None);
    }

    #[test]
    fn request_line_malformations() {
        use Malformation::*;
        assert!(text(BadMethod).starts_with("P0ST /api/login HTTP/1.1\r\n"));
        assert!(text(LowercaseMethod).starts_with("post /api/login HTTP/1.1\r\n"));
        assert!(text(BadVersion).starts_with("POST /api/login HTTP/9.9\r\n"));
        assert!(text(MissingVersion).starts_with(&format!("POST /api/login\r\n{BASE_HEADERS}")));
        assert!(text(DoubleSpace).starts_with("POST  /api/login HTTP/1.1\r\n"));
        let huge = text(HugeUri);
        assert!(huge.starts_with("POST /api/login?nadir=AAAA"));
        assert!(huge.len() > HUGE);
        assert!(text(NulInPath).starts_with("POST /api/login\0 HTTP/1.1\r\n"));
        assert!(text(CrInPath).starts_with("POST /api/login\r HTTP/1.1\r\n"));
    }

    #[test]
    fn header_malformations() {
        use Malformation::*;
        assert!(text(CrlfInjection).contains("\r\nX-Nadir: nadir\rX-Injected: 1\r\n\r\n"));
        let bare = text(BareLf);
        assert!(bare.starts_with("POST /api/login HTTP/1.1\nHost: 127.0.0.1:3000\n"));
        assert!(!bare.contains('\r'));
        assert!(bare.ends_with(&format!("\n\n{BODY}")));
        assert!(text(HeaderWithoutColon).contains("\r\nX-Nadir-No-Colon value\r\n\r\n"));
        assert!(text(SpaceBeforeColon).contains("\r\nHost : 127.0.0.1:3000\r\n"));
        assert!(text(FoldedHeader).contains("\r\nX-Nadir: first\r\n second\r\n"));
        assert_eq!(text(ManyHeaders).matches("\r\nX-Nadir-").count(), MANY_HEADERS);
        assert!(text(HugeHeader).len() > HUGE);
        assert!(!text(MissingHost).contains("Host:"));
        assert!(text(DuplicateHost).contains("\r\nHost: nadir.invalid\r\n"));
    }

    #[test]
    fn body_framing_malformations() {
        use Malformation::*;
        assert!(text(ShortLength).contains(&format!("Content-Length: 15\r\nConnection: close\r\n\r\n{BODY}")));
        assert!(text(LongLength).contains("Content-Length: 131\r\n"));
        assert!(text(NegativeLength).contains("Content-Length: -1\r\n"));
        assert!(text(NonNumericLength).contains("Content-Length: nadir\r\n"));
        assert!(text(ConflictingLengths).contains("Content-Length: 31\r\nConnection: close\r\nContent-Length: 32\r\n"));
        assert!(text(LengthAndChunked).ends_with(&format!("Content-Length: 31\r\nConnection: close\r\nTransfer-Encoding: chunked\r\n\r\n{BODY}")));
        let bad_chunk = text(BadChunkSize);
        assert!(!bad_chunk.contains("Content-Length"));
        assert!(bad_chunk.ends_with(&format!("Transfer-Encoding: chunked\r\n\r\nzz\r\n{BODY}\r\n0\r\n\r\n")));
        assert!(text(UnterminatedChunked).ends_with(&format!("\r\n\r\n1f\r\n{BODY}\r\n")));
        assert!(text(TrailingBytes).ends_with(&format!("{BODY}nadir-trailing-bytes")));
    }

    #[test]
    fn requests_that_end_early() {
        use Malformation::*;
        let open = text(NoHeaderTerminator);
        assert!(open.ends_with("Connection: close\r\n") && !open.contains("\r\n\r\n"));
        let truncated = text(TruncatedRequest);
        assert!(truncated.starts_with("POST /api/login HTTP/1.1\r\n"));
        assert!(truncated.len() < open.len() && !truncated.contains("Connection"));
    }

    #[test]
    fn body_framing_uses_a_filler_without_a_body() {
        let mut get = login();
        get.method = "GET".into();
        get.body.clear();
        get.remove_header("Content-Length");
        let short = String::from_utf8(Malformation::ShortLength.apply(&get)).unwrap();
        assert!(short.ends_with("Content-Length: 5\r\n\r\nnadir-body"));
    }
}
