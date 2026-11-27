//! A loopback-only TCP client that writes request bytes exactly as given and
//! reads whatever comes back.
//!
//! # Design notes
//!
//! **Chosen:** the client first waits half the timeout for an answer with
//! the connection open: servers such as hyper drop a pending response when
//! the client half-closes. If nothing arrives by then, it shuts down its
//! write side, so a server waiting for bytes a malformed request promised (a
//! long `Content-Length`, a missing empty line) sees the end of the request
//! and can answer or close. Only a server that does neither within the full
//! timeout is reported as a timeout. The response reader tolerates anything: it
//! skips `1xx` responses, reads the body by `Content-Length`, `chunked` or
//! until the connection closes, and keeps at most `MAX_RESPONSE_BODY` bytes.

use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use url::Url;

use crate::adapters::http::transport::{
    validate_loopback, FailurePhase, HttpObservation, TransportError, MAX_RESPONSE_BODY,
};

/// Most bytes read for the status line and headers.
const MAX_HEAD: usize = 64 * 1024;

pub struct RawClient {
    address: SocketAddr,
    host: String,
    timeout: Duration,
    requests_sent: AtomicU32,
}

impl RawClient {
    /// A client for `base_url`, which must be a loopback IP literal.
    pub fn new(base_url: &Url, timeout: Duration) -> Result<Self, TransportError> {
        validate_loopback(base_url)?;
        let host = base_url.host_str().unwrap_or_default();
        let ip: IpAddr = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse()
            .map_err(|_| TransportError::InvalidUrl(base_url.to_string()))?;
        let port = base_url
            .port_or_known_default()
            .ok_or_else(|| TransportError::InvalidUrl(base_url.to_string()))?;
        let host = match base_url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_owned(),
        };
        Ok(Self { address: SocketAddr::new(ip, port), host, timeout, requests_sent: AtomicU32::new(0) })
    }

    /// The `Host` header value for well-formed requests.
    pub fn host(&self) -> &str {
        &self.host
    }

    pub(crate) fn requests_sent(&self) -> u32 {
        self.requests_sent.load(Ordering::Relaxed)
    }

    /// Write `bytes`, end the request, and read the response.
    pub fn send(&self, bytes: &[u8]) -> Result<HttpObservation, TransportError> {
        let deadline = Instant::now() + self.timeout;
        let mut stream = TcpStream::connect_timeout(&self.address, self.timeout)
            .map_err(|error| TransportError::ConnectionFailed(error.to_string(), FailurePhase::BeforeHeaders))?;
        self.requests_sent.fetch_add(1, Ordering::Relaxed);
        let _ = stream.set_nodelay(true);
        let _ = stream.set_write_timeout(Some(self.timeout));
        // A server may answer and close before reading everything (431 for
        // huge headers): a failed write still leaves a response to read.
        let _ = stream.write_all(bytes).and_then(|()| stream.flush());
        let patience = Instant::now() + self.timeout / 2;
        read_response(&mut stream, patience.min(deadline), deadline)
    }
}

/// What reading produced before the connection ended or the deadline passed.
enum Ended {
    Closed,
    Reset,
    Timeout,
}

/// Read into `buffer` until `done` says so, the connection ends, or the deadline.
fn read_until(
    stream: &mut TcpStream,
    buffer: &mut Vec<u8>,
    deadline: Instant,
    limit: usize,
    done: impl Fn(&[u8]) -> bool,
) -> Option<Ended> {
    let mut chunk = [0u8; 8192];
    while !done(buffer) {
        if buffer.len() > limit {
            return None;
        }
        let Some(left) = deadline.checked_duration_since(Instant::now()).filter(|left| !left.is_zero()) else {
            return Some(Ended::Timeout);
        };
        let _ = stream.set_read_timeout(Some(left));
        match stream.read(&mut chunk) {
            Ok(0) => return Some(Ended::Closed),
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) if matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {
                return Some(Ended::Timeout)
            }
            Err(error) if error.kind() == ErrorKind::ConnectionReset => return Some(Ended::Reset),
            Err(_) => return Some(Ended::Closed),
        }
    }
    None
}

/// End of the header block (`\r\n\r\n`, or `\n\n` from a lenient server).
fn head_end(bytes: &[u8]) -> Option<(usize, usize)> {
    let crlf = bytes.windows(4).position(|window| window == b"\r\n\r\n").map(|at| (at, at + 4));
    let lf = bytes.windows(2).position(|window| window == b"\n\n").map(|at| (at, at + 2));
    match (crlf, lf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (found, None) | (None, found) => found,
    }
}

/// Read the response. Until `patience` the request stays open; after it,
/// the write side is shut down and reading continues until `deadline`.
fn read_response(
    stream: &mut TcpStream,
    patience: Instant,
    deadline: Instant,
) -> Result<HttpObservation, TransportError> {
    let mut buffer = Vec::new();
    let mut ended = read_until(stream, &mut buffer, patience, MAX_HEAD, |bytes| head_end(bytes).is_some());
    if matches!(ended, Some(Ended::Timeout)) && head_end(&buffer).is_none() {
        let _ = stream.shutdown(Shutdown::Write);
        ended = read_until(stream, &mut buffer, deadline, MAX_HEAD, |bytes| head_end(bytes).is_some());
    }
    loop {
        let Some((head_len, body_start)) = head_end(&buffer) else {
            let phase = FailurePhase::BeforeHeaders;
            return Err(match ended {
                _ if buffer.len() > MAX_HEAD => TransportError::ConnectionFailed("response headers too long".into(), phase),
                Some(Ended::Timeout) => TransportError::Timeout(phase),
                Some(Ended::Reset) if buffer.is_empty() => TransportError::Reset(phase),
                _ if buffer.is_empty() => TransportError::Closed(phase),
                _ => TransportError::ConnectionFailed(
                    format!("incomplete HTTP response: {}", excerpt(&buffer)),
                    phase,
                ),
            });
        };
        let head = String::from_utf8_lossy(&buffer[..head_len]).into_owned();
        let mut lines = head.lines();
        let status = lines
            .next()
            .and_then(|line| {
                let mut parts = line.split_whitespace();
                parts.next().filter(|version| version.starts_with("HTTP/"))?;
                parts.next()?.parse::<u16>().ok()
            })
            .ok_or_else(|| {
                TransportError::ConnectionFailed(
                    format!("invalid HTTP response: {}", excerpt(&buffer)),
                    FailurePhase::BeforeHeaders,
                )
            })?;
        let headers: Vec<(String, String)> = lines
            .filter_map(|line| {
                let (name, value) = line.split_once(':')?;
                Some((name.trim().to_ascii_lowercase(), value.trim().to_owned()))
            })
            .collect();
        buffer.drain(..body_start);
        // Interim responses (100 Continue) precede the real one.
        if (100..200).contains(&status) {
            ended = read_until(stream, &mut buffer, deadline, MAX_HEAD, |bytes| head_end(bytes).is_some());
            continue;
        }
        let header = |name: &str| headers.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str());
        let chunked = header("transfer-encoding").is_some_and(|value| value.to_ascii_lowercase().contains("chunked"));
        let length = header("content-length").and_then(|value| value.parse::<usize>().ok());
        let ended = if chunked {
            read_until(stream, &mut buffer, deadline, MAX_RESPONSE_BODY * 2, |bytes| decode_chunked(bytes).1)
        } else if let Some(length) = length {
            read_until(stream, &mut buffer, deadline, MAX_RESPONSE_BODY, |bytes| bytes.len() >= length)
        } else {
            read_until(stream, &mut buffer, deadline, MAX_RESPONSE_BODY, |_| false)
        };
        if matches!(ended, Some(Ended::Timeout)) && !chunked && length.is_none_or(|length| buffer.len() < length) {
            // Headers arrived but the body never finished.
            if length.is_some() {
                return Err(TransportError::Timeout(FailurePhase::DuringBodyRead));
            }
        }
        let mut body = if chunked { decode_chunked(&buffer).0 } else { buffer };
        if let Some(length) = length {
            body.truncate(length);
        }
        let truncated = body.len() > MAX_RESPONSE_BODY;
        body.truncate(MAX_RESPONSE_BODY);
        return Ok(HttpObservation { status, headers, body, truncated });
    }
}

/// The data of a chunked body so far, and whether the last chunk arrived.
fn decode_chunked(bytes: &[u8]) -> (Vec<u8>, bool) {
    let mut data = Vec::new();
    let mut at = 0;
    loop {
        let Some(line_end) = bytes[at..].windows(2).position(|window| window == b"\r\n") else {
            return (data, false);
        };
        let size_text = String::from_utf8_lossy(&bytes[at..at + line_end]);
        let Ok(size) = usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16) else {
            return (data, true);
        };
        at += line_end + 2;
        if size == 0 {
            return (data, true);
        }
        let end = (at + size).min(bytes.len());
        data.extend_from_slice(&bytes[at..end]);
        if at + size + 2 > bytes.len() {
            return (data, false);
        }
        at += size + 2;
    }
}

fn excerpt(bytes: &[u8]) -> String {
    crate::inspect::escape(&String::from_utf8_lossy(&bytes[..bytes.len().min(80)]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    /// A one-connection server that reads the request to EOF, then runs `reply`.
    fn serve(reply: impl FnOnce(&mut TcpStream, Vec<u8>) + Send + 'static) -> (RawClient, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let _ = stream.read_to_end(&mut request);
            reply(&mut stream, request);
        });
        (RawClient::new(&url, Duration::from_millis(500)).unwrap(), worker)
    }

    #[test]
    fn reads_length_chunked_interim_and_close_delimited_bodies() {
        let cases: Vec<(&[u8], &[u8])> = vec![
            (b"HTTP/1.1 400 Bad Request\r\nContent-Length: 3\r\n\r\nbad-extra", b"bad"),
            (b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n", b"abcde"),
            (b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\nok", b"ok"),
            (b"HTTP/1.0 200 OK\r\n\r\nuntil-close", b"until-close"),
        ];
        for (response, body) in cases {
            let response = response.to_vec();
            let (client, worker) = serve(move |stream, request| {
                assert_eq!(request, b"GET / HTTP/1.1\r\n\r\n");
                stream.write_all(&response).unwrap();
            });
            let observation = client.send(b"GET / HTTP/1.1\r\n\r\n").unwrap();
            assert_eq!(observation.body, body);
            worker.join().unwrap();
        }
    }

    #[test]
    fn waits_before_half_closing_like_servers_that_drop_responses_on_eof() {
        // Like hyper: once the client half-closes, the pending answer is dropped.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 18];
            stream.read_exact(&mut request).unwrap();
            thread::sleep(Duration::from_millis(50));
            stream.set_nonblocking(true).unwrap();
            let half_closed = matches!(stream.read(&mut [0u8; 1]), Ok(0));
            stream.set_nonblocking(false).unwrap();
            if !half_closed {
                stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok").unwrap();
            }
        });
        let client = RawClient::new(&url, Duration::from_millis(500)).unwrap();
        assert_eq!(client.send(b"GET / HTTP/1.1\r\n\r\n").unwrap().status, 200);
        worker.join().unwrap();

        // A server that waits for promised bytes gets the end of the request
        // after half the timeout, and can still answer before the timeout.
        let (client, worker) = serve(|stream, request| {
            assert!(request.ends_with(b"\r\n\r\nshort"));
            stream.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n").unwrap();
        });
        let started = Instant::now();
        let observation = client.send(b"POST / HTTP/1.1\r\nContent-Length: 99\r\n\r\nshort").unwrap();
        assert_eq!(observation.status, 400);
        assert!(started.elapsed() >= Duration::from_millis(240), "{:?}", started.elapsed());
        worker.join().unwrap();
    }

    #[test]
    fn closes_resets_and_silence_are_told_apart() {
        let (client, worker) = serve(|_, _| {});
        assert!(matches!(client.send(b"GET / HTTP/1.1\r\n\r\n"), Err(TransportError::Closed(FailurePhase::BeforeHeaders))));
        worker.join().unwrap();

        // A server that never answers nor closes.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let worker = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_millis(900));
            drop(stream);
        });
        let client = RawClient::new(&url, Duration::from_millis(300)).unwrap();
        let started = Instant::now();
        assert!(matches!(client.send(b"GET / HTTP/1.1\r\n\r\n"), Err(TransportError::Timeout(FailurePhase::BeforeHeaders))));
        assert!(started.elapsed() < Duration::from_millis(800));
        worker.join().unwrap();

        // Unread request data makes the kernel answer the close with a reset.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let worker = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_millis(100));
            drop(stream);
        });
        let client = RawClient::new(&url, Duration::from_millis(500)).unwrap();
        let result = client.send(&vec![b'x'; 1 << 20]);
        assert!(matches!(result, Err(TransportError::Reset(_) | TransportError::Closed(_))), "{result:?}");
        worker.join().unwrap();
    }

    #[test]
    fn refuses_non_loopback_and_reports_refused_connections() {
        assert!(RawClient::new(&Url::parse("http://10.0.0.1:80").unwrap(), Duration::from_secs(1)).is_err());
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let client = RawClient::new(&Url::parse(&format!("http://127.0.0.1:{port}")).unwrap(), Duration::from_secs(1)).unwrap();
        assert_eq!(client.host(), format!("127.0.0.1:{port}"));
        assert!(matches!(client.send(b"GET / HTTP/1.1\r\n\r\n"), Err(TransportError::ConnectionFailed(..))));
    }

    #[test]
    fn chunked_decoding_tolerates_partial_and_bad_input() {
        assert_eq!(decode_chunked(b"3\r\nabc\r\n0\r\n\r\n"), (b"abc".to_vec(), true));
        assert_eq!(decode_chunked(b"3\r\nab"), (b"ab".to_vec(), false));
        assert_eq!(decode_chunked(b"zz\r\nabc"), (Vec::new(), true));
    }
}
