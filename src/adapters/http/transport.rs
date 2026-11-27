//! HTTP client wrapper: loopback-only, no redirects, timeout, response
//! normalization.
//!
//! # Design notes
//!
//! **Chosen:** `ureq` 3.x as the blocking HTTP client. The `HttpClient` wraps
//! an `ureq::Agent` and enforces four invariants:
//! 1. Destination is loopback-only (`127.0.0.0/8` or `::1`)
//! 2. Redirects are not followed (3xx returned as-is)
//! 3. Every request has a timeout
//! 4. Response bodies >256 KiB are truncated
//!
//! `ureq` types never appear outside this file.

use std::io::Read;
use std::net::IpAddr;
use std::time::Duration;

use url::Url;

// ── Public types ───────────────────────────────────────────────────

/// Normalized HTTP observation from a single request.
#[derive(Debug, Clone)]
pub struct HttpObservation {
    /// HTTP status code.
    pub status: u16,
    /// Response headers (keys normalized to lower-case).
    pub headers: Vec<(String, String)>,
    /// Response body (≤256 KiB).
    pub body: Vec<u8>,
    /// Whether the body was truncated.
    pub truncated: bool,
}

/// The phase at which a transport failure was observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailurePhase {
    /// Failure occurred before any response headers were received.
    #[serde(alias = "beforeheaders")]
    BeforeHeaders,
    /// Failure occurred while reading the response body.
    #[serde(alias = "duringbodyread")]
    DuringBodyRead,
    /// The phase could not be determined by the transport adapter.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportReason {
    ConnectionFailed,
    Timeout,
    Reset,
    ConnectionClosed,
    BodyReadFailed,
    NonLoopback,
    InvalidUrl,
}

impl std::fmt::Display for TransportReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::ConnectionFailed => "connection_failed",
            Self::Timeout => "timeout",
            Self::Reset => "reset",
            Self::ConnectionClosed => "connection_closed",
            Self::BodyReadFailed => "body_read_failed",
            Self::NonLoopback => "non_loopback",
            Self::InvalidUrl => "invalid_url",
        })
    }
}

impl std::fmt::Display for FailurePhase {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::BeforeHeaders => "before_headers",
            Self::DuringBodyRead => "during_body_read",
            Self::Unknown => "unknown",
        })
    }
}

/// Errors that can occur during transport.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("connection failed: {0}")]
    ConnectionFailed(String, FailurePhase),
    #[error("request timed out")]
    Timeout(FailurePhase),
    #[error("connection reset")]
    Reset(FailurePhase),
    #[error("connection closed")]
    Closed(FailurePhase),
    #[error("reading response body failed")]
    BodyRead,
    #[error("non-loopback destination rejected: {0}")]
    NonLoopback(String),
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
}

impl TransportError {
    /// The failure phase, if applicable.
    pub fn phase(&self) -> Option<FailurePhase> {
        match self {
            Self::ConnectionFailed(_, phase)
            | Self::Timeout(phase)
            | Self::Reset(phase)
            | Self::Closed(phase) => Some(*phase),
            Self::BodyRead => Some(FailurePhase::DuringBodyRead),
            _ => None,
        }
    }

    pub fn reason(&self) -> TransportReason {
        match self {
            Self::ConnectionFailed(..) => TransportReason::ConnectionFailed,
            Self::Timeout(_) => TransportReason::Timeout,
            Self::Reset(_) => TransportReason::Reset,
            Self::Closed(_) => TransportReason::ConnectionClosed,
            Self::BodyRead => TransportReason::BodyReadFailed,
            Self::NonLoopback(_) => TransportReason::NonLoopback,
            Self::InvalidUrl(_) => TransportReason::InvalidUrl,
        }
    }
}

/// Maximum response body size (256 KiB).
pub const MAX_RESPONSE_BODY: usize = 256 * 1024;

// ── HttpClient ─────────────────────────────────────────────────────

use super::request::ResolvedStepInput;

/// Wrapper around `ureq::Agent` with loopback enforcement and timeout.
pub struct HttpClient {
    agent: ureq::Agent,
    base_url: Url,
    _timeout: Duration,
    requests_sent: std::sync::atomic::AtomicU32,
}

impl HttpClient {
    /// Create a new HTTP client.
    ///
    /// Returns `TransportError::NonLoopback` if `base_url` does not resolve
    /// to a loopback address.
    pub fn new(base_url: Url, timeout: Duration) -> Result<Self, TransportError> {
        validate_loopback(&base_url)?;

        let config = ureq::Agent::config_builder()
            .proxy(None)
            .max_redirects(0)
            .max_redirects_will_error(false) // don't follow redirects, return the 3xx
            .timeout_global(Some(timeout))
            .http_status_as_error(false) // treat all HTTP responses as Ok
            .build();

        let agent: ureq::Agent = config.into();

        Ok(Self {
            agent,
            base_url,
            _timeout: timeout,
            requests_sent: std::sync::atomic::AtomicU32::new(0),
        })
    }

    /// Execute an HTTP request against the base URL.
    ///
    /// Returns `Ok(HttpObservation)` on any HTTP response (including 4xx, 5xx).
    /// Returns `Err(TransportError)` on connection failure, reset, or timeout.
    pub fn execute(&self, request: &ResolvedStepInput) -> Result<HttpObservation, TransportError> {
        let url = &request.url;
        if url.origin() != self.base_url.origin() {
            return Err(TransportError::NonLoopback(
                "request left the configured origin".into(),
            ));
        }
        self.requests_sent
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let result = dispatch_request(
            &self.agent,
            &request.method,
            url.as_str(),
            &request.headers,
            request.body.as_deref(),
            request.content_type.as_deref(),
        );

        match result {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let headers = normalize_headers(resp.headers());
                let (body_bytes, truncated) = read_body_truncated(resp.into_body())
                    .map_err(|error| classify_io_error(error, FailurePhase::DuringBodyRead))?;
                Ok(HttpObservation {
                    status,
                    headers,
                    body: body_bytes,
                    truncated,
                })
            }
            Err(e) => convert_ureq_error(e),
        }
    }

    pub(crate) fn requests_sent(&self) -> u32 {
        self.requests_sent
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn base_url(&self) -> &Url {
        &self.base_url
    }
}

// ── Request dispatch ───────────────────────────────────────────────

/// Dispatch to the correct ureq agent method based on HTTP method and body.
///
/// Uses `send_bytes` for methods that support a body, `call` for body-less ones.
fn dispatch_request(
    agent: &ureq::Agent,
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    content_type: Option<&str>,
) -> Result<http::Response<ureq::Body>, ureq::Error> {
    match method.to_uppercase().as_str() {
        "GET" => {
            let mut req = agent.get(url);
            req = apply_headers(req, headers, content_type);
            req.call()
        }
        "POST" => {
            let mut req = agent.post(url);
            req = apply_headers(req, headers, content_type);
            match body {
                Some(b) => req.send(b),
                None => req.send_empty(),
            }
        }
        "PUT" => {
            let mut req = agent.put(url);
            req = apply_headers(req, headers, content_type);
            match body {
                Some(b) => req.send(b),
                None => req.send_empty(),
            }
        }
        "DELETE" => {
            let mut req = agent.delete(url);
            req = apply_headers(req, headers, content_type);
            req.call()
        }
        "PATCH" => {
            let mut req = agent.patch(url);
            req = apply_headers(req, headers, content_type);
            match body {
                Some(b) => req.send(b),
                None => req.send_empty(),
            }
        }
        "HEAD" => {
            let mut req = agent.head(url);
            req = apply_headers(req, headers, content_type);
            req.call()
        }
        "OPTIONS" => {
            let mut req = agent.options(url);
            req = apply_headers(req, headers, content_type);
            req.call()
        }
        other => Err(ureq::Error::BadUri(format!(
            "unsupported HTTP method: {}",
            other
        ))),
    }
}

/// Apply headers and content type to a request builder.
fn apply_headers<B>(
    mut builder: ureq::RequestBuilder<B>,
    headers: &[(String, String)],
    content_type: Option<&str>,
) -> ureq::RequestBuilder<B> {
    for (key, value) in headers {
        builder = builder.header(key, value);
    }
    if let Some(ct) = content_type {
        builder = builder.header("Content-Type", ct);
    }
    builder
}

// ── Response helpers ───────────────────────────────────────────────

fn convert_ureq_error(error: ureq::Error) -> Result<HttpObservation, TransportError> {
    let phase = FailurePhase::BeforeHeaders;
    match error {
        ureq::Error::StatusCode(status) => Ok(HttpObservation {
            status,
            headers: Vec::new(),
            body: Vec::new(),
            truncated: false,
        }),
        ureq::Error::Timeout(_) => Err(TransportError::Timeout(phase)),
        ureq::Error::Io(error) => Err(classify_io_error(error, phase)),
        error => Err(TransportError::ConnectionFailed(error.to_string(), phase)),
    }
}

fn classify_io_error(error: std::io::Error, phase: FailurePhase) -> TransportError {
    use std::io::ErrorKind;
    if matches!(
        error
            .get_ref()
            .and_then(|source| source.downcast_ref::<ureq::Error>()),
        Some(ureq::Error::Timeout(_))
    ) {
        return TransportError::Timeout(phase);
    }
    match error.kind() {
        ErrorKind::TimedOut | ErrorKind::WouldBlock => TransportError::Timeout(phase),
        ErrorKind::ConnectionReset => TransportError::Reset(phase),
        ErrorKind::UnexpectedEof | ErrorKind::ConnectionAborted | ErrorKind::BrokenPipe => {
            TransportError::Closed(phase)
        }
        _ if phase == FailurePhase::DuringBodyRead => TransportError::BodyRead,
        _ => TransportError::ConnectionFailed(error.to_string(), phase),
    }
}

/// Normalize response headers from an `http::HeaderMap`.
fn normalize_headers(headers: &http::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_lowercase(),
                value.to_str().unwrap_or("<binary>").to_string(),
            )
        })
        .collect()
}

/// Read a `ureq::Body` with truncation at MAX_RESPONSE_BODY.
fn read_body_truncated(body: ureq::Body) -> std::io::Result<(Vec<u8>, bool)> {
    let mut limited = body.into_reader().take(MAX_RESPONSE_BODY as u64 + 1);
    let mut bytes = Vec::new();
    limited.read_to_end(&mut bytes)?;
    let truncated = bytes.len() > MAX_RESPONSE_BODY;
    bytes.truncate(MAX_RESPONSE_BODY);
    Ok((bytes, truncated))
}

// ── Loopback validation ────────────────────────────────────────────

/// Validate that a URL resolves to a loopback address.
fn validate_loopback(url: &Url) -> Result<(), TransportError> {
    let host = url
        .host_str()
        .ok_or_else(|| TransportError::NonLoopback("missing host".into()))?;

    // Strip brackets from IPv6 addresses (URL crate returns them)
    let clean_host = host.trim_start_matches('[').trim_end_matches(']');

    // Reject hostnames (including "localhost")
    let ip: IpAddr = match clean_host.parse() {
        Ok(ip) => ip,
        Err(_) => {
            return Err(TransportError::NonLoopback(format!(
                "hostname '{}' rejected — only IP literals allowed",
                host
            )));
        }
    };

    match ip {
        IpAddr::V4(v4) if v4.octets()[0] == 127 => Ok(()),
        IpAddr::V6(v6) if v6.is_loopback() => Ok(()),
        _ => Err(TransportError::NonLoopback(format!(
            "{} is not a loopback address (only 127.0.0.0/8 and ::1 allowed)",
            ip
        ))),
    }
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rejects_non_loopback() {
        let url = Url::parse("http://192.168.1.1:8080").unwrap();
        let result = HttpClient::new(url, Duration::from_secs(5));
        assert!(matches!(result, Err(TransportError::NonLoopback(_))));
    }

    #[test]
    fn test_rejects_localhost() {
        let url = Url::parse("http://localhost:8080").unwrap();
        let result = HttpClient::new(url, Duration::from_secs(5));
        assert!(matches!(result, Err(TransportError::NonLoopback(_))));
    }

    #[test]
    fn test_accepts_loopback_v4() {
        let url = Url::parse("http://127.0.0.1:8080").unwrap();
        let result = HttpClient::new(url, Duration::from_secs(5));
        assert!(result.is_ok());
    }

    #[test]
    fn test_accepts_loopback_v6() {
        let url = Url::parse("http://[::1]:8080").unwrap();
        let result = HttpClient::new(url, Duration::from_secs(5));
        assert!(result.is_ok(), "failed: {:?}", result.err());
    }

    #[test]
    fn rejects_a_prepared_request_for_a_different_origin_before_dispatch() {
        let base = Url::parse("http://127.0.0.1:8080").unwrap();
        let client = HttpClient::new(base, Duration::from_millis(500)).unwrap();
        let request = ResolvedStepInput {
            url: Url::parse("http://127.0.0.1:8081/reports?view=summary").unwrap(),
            method: "GET".into(),
            headers: Vec::new(),
            body: None,
            content_type: None,
        };
        assert!(matches!(
            client.execute(&request),
            Err(TransportError::NonLoopback(_))
        ));
        assert_eq!(client.requests_sent(), 0);
    }

    #[test]
    fn test_connection_refused() {
        let url = Url::parse("http://127.0.0.1:19999").unwrap();
        let client = HttpClient::new(url, Duration::from_millis(500)).unwrap();
        let result = client.execute(&ResolvedStepInput {
            url: client.base_url().clone(),
            method: "GET".into(),
            headers: Vec::new(),
            body: None,
            content_type: None,
        });
        assert!(matches!(result, Err(TransportError::ConnectionFailed(..))));
    }

    #[test]
    fn test_accepts_127_0_0_2() {
        let url = Url::parse("http://127.0.0.2:8080").unwrap();
        let result = HttpClient::new(url, Duration::from_secs(5));
        assert!(result.is_ok());
    }

    #[test]
    fn test_rejects_non_loopback_v6() {
        let url = Url::parse("http://[::2]:8080").unwrap();
        let result = HttpClient::new(url, Duration::from_secs(5));
        assert!(matches!(result, Err(TransportError::NonLoopback(_))));
    }
}
