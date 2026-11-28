//! Managed target process: spawn, readiness polling, lifecycle, stderr
//! capture, and SIGTERM/SIGKILL shutdown.
//!
//! # Design notes
//!
//! **Chosen:** `ManagedTarget` owns the child process handle and a background
//! reader thread for stderr capture. The stderr ring buffer is the last 64 KiB.
//! `ManagedTarget::start()` spawns, `wait_ready()` polls a health endpoint,
//! `stop()` sends SIGTERM → wait 2s → SIGKILL.
//!
//! The child stays in Nadir's process group, so Ctrl+C in a terminal reaches
//! both. Commands should start the server directly (or `exec` it from a shell)
//! so that SIGTERM reaches the process that owns the port.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;

use crate::experiment::types::TargetConfig;

/// Placeholder replaced by the target port in `command` and `env` values.
pub const TARGET_PORT_REFERENCE: &str = "${target.port}";

// ── Types ──────────────────────────────────────────────────────────

/// Errors during target startup.
#[derive(Debug, thiserror::Error)]
pub enum StartupError {
    #[error("failed to spawn process: {0}")]
    Spawn(std::io::Error),
    #[error("target not ready after {0:?}")]
    NotReady(Duration),
    #[error("target exited during readiness check")]
    ExitedDuringStartup,
    #[error("target startup I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Errors during target shutdown.
#[derive(Debug, thiserror::Error)]
pub enum StopError {
    #[error("failed to signal process: {0}")]
    Signal(String),
    #[error("failed to wait for process: {0}")]
    Wait(String),
}

/// Process status as observable by the monitor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessStatus {
    Running,
    Exited {
        code: Option<i32>,
        signal: Option<i32>,
    },
    Unknown,
}

/// Ring buffer for stderr capture (64 KiB).
const STDERR_RING_SIZE: usize = 64 * 1024;

/// How long `stop()` waits for the stderr reader after the process ends.
/// A grandchild that inherited the pipe can keep it open indefinitely.
const STDERR_JOIN_TIMEOUT: Duration = Duration::from_secs(1);

struct RingBuffer {
    buf: VecDeque<u8>,
    cap: usize,
}

impl RingBuffer {
    fn new(cap: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(cap),
            cap,
        }
    }

    fn write(&mut self, data: &[u8]) {
        for &b in data {
            if self.buf.len() >= self.cap {
                self.buf.pop_front();
            }
            self.buf.push_back(b);
        }
    }

    fn as_bytes(&self) -> Vec<u8> {
        self.buf.iter().copied().collect()
    }
}

// ── ManagedTarget ──────────────────────────────────────────────────

/// A supervised target process.
pub struct ManagedTarget {
    child: Option<Child>,
    pid: Option<u32>,
    /// Exit status observed before the child handle was released.
    exit_status: Option<ProcessStatus>,
    stderr_ring: Arc<Mutex<RingBuffer>>,
    stderr_thread: Option<JoinHandle<()>>,
    generation: u32,
}

impl ManagedTarget {
    /// Start the target process from a `TargetConfig`.
    ///
    /// `${target.port}` in `command` and `env` values is replaced with `port`.
    /// The process inherits Nadir's environment plus `config.env`. A relative
    /// program path (one containing `/`) is resolved against `config.cwd`.
    /// Stdout is discarded; stderr is captured in a ring buffer.
    pub fn start(config: &TargetConfig, port: u16) -> Result<Self, StartupError> {
        if config.command.is_empty() {
            return Err(StartupError::Spawn(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "empty command",
            )));
        }

        let port = port.to_string();
        let substitute = |value: &str| value.replace(TARGET_PORT_REFERENCE, &port);

        let mut program = substitute(&config.command[0]);
        if let Some(cwd) = &config.cwd {
            let path = Path::new(&program);
            if program.contains('/') && path.is_relative() {
                program = Path::new(cwd).join(path).to_string_lossy().into_owned();
            }
        }

        let mut cmd = Command::new(&program);
        cmd.args(config.command[1..].iter().map(|arg| substitute(arg)))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        if let Some(ref cwd) = config.cwd {
            cmd.current_dir(cwd);
        }

        for (key, value) in &config.env {
            cmd.env(key, substitute(value));
        }

        let mut child = cmd.spawn().map_err(StartupError::Spawn)?;
        let pid = child.id();

        // Spawn stderr reader thread
        let stderr = child.stderr.take();
        let ring = Arc::new(Mutex::new(RingBuffer::new(STDERR_RING_SIZE)));
        let ring_clone = ring.clone();

        let stderr_thread = stderr.map(|stderr| {
            thread::spawn(move || {
                let mut reader = stderr;
                let mut buf = [0u8; 4096];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break, // EOF
                        Ok(n) => {
                            if let Ok(mut ring) = ring_clone.lock() {
                                ring.write(&buf[..n]);
                            }
                        }
                        Err(_) => break,
                    }
                }
            })
        });

        Ok(Self {
            child: Some(child),
            pid: Some(pid),
            exit_status: None,
            stderr_ring: ring,
            stderr_thread,
            generation: 1,
        })
    }

    /// Poll `GET <path>` on `address` until it answers with a 2xx status,
    /// the process exits, or `timeout` elapses.
    ///
    /// Uses a minimal HTTP/1.0 request over `std::net::TcpStream` to avoid
    /// depending on the full `HttpClient`.
    pub fn wait_ready(
        &mut self,
        address: SocketAddr,
        path: &str,
        timeout: Duration,
    ) -> Result<(), StartupError> {
        let deadline = Instant::now() + timeout;

        loop {
            if matches!(self.is_running(), ProcessStatus::Exited { .. }) {
                return Err(StartupError::ExitedDuringStartup);
            }

            if probe_ready(address, path) {
                return Ok(());
            }

            if Instant::now() >= deadline {
                return Err(StartupError::NotReady(timeout));
            }

            thread::sleep(Duration::from_millis(50));
        }
    }

    /// Stop the target: SIGTERM → wait 2s → SIGKILL. Calling it again is a no-op.
    pub fn stop(&mut self) -> Result<(), StopError> {
        let mut result = Ok(());

        if let Some(mut child) = self.child.take() {
            // Only signal a child that has not been reaped, so the PID cannot
            // belong to an unrelated process.
            if matches!(child.try_wait(), Ok(None)) {
                let pid = Pid::from_raw(child.id() as i32);
                if let Err(error) = signal::kill(pid, Signal::SIGTERM) {
                    result = Err(StopError::Signal(error.to_string()));
                }
                let deadline = Instant::now() + Duration::from_secs(2);
                while matches!(child.try_wait(), Ok(None)) {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        break;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
            }
            match child.wait() {
                Ok(status) => {
                    self.exit_status.get_or_insert(ProcessStatus::Exited {
                        code: status.code(),
                        signal: status.signal(),
                    });
                }
                Err(error) if result.is_ok() => result = Err(StopError::Wait(error.to_string())),
                Err(_) => {}
            }
        }
        self.pid = None;

        if let Some(handle) = self.stderr_thread.take() {
            let deadline = Instant::now() + STDERR_JOIN_TIMEOUT;
            while !handle.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            if handle.is_finished() {
                let _ = handle.join();
            }
        }

        result
    }

    /// Check whether the process is still running.
    pub fn is_running(&mut self) -> ProcessStatus {
        if let Some(status) = &self.exit_status {
            return status.clone();
        }
        match self.child.as_mut() {
            Some(child) => match child.try_wait() {
                Ok(Some(status)) => {
                    let status = ProcessStatus::Exited {
                        code: status.code(),
                        signal: status.signal(),
                    };
                    self.exit_status = Some(status.clone());
                    status
                }
                Ok(None) => ProcessStatus::Running,
                Err(_) => ProcessStatus::Unknown,
            },
            None => ProcessStatus::Unknown,
        }
    }

    /// Get the last 64 KiB of captured stderr.
    pub fn stderr_tail(&self) -> Vec<u8> {
        self.stderr_ring
            .lock()
            .map(|ring| ring.as_bytes())
            .unwrap_or_default()
    }

    /// Restart the target (stop + start with same config).
    pub fn restart(
        &mut self,
        config: &TargetConfig,
        address: SocketAddr,
        ready_path: &str,
        ready_timeout: Duration,
    ) -> Result<(), StartupError> {
        let _ = self.stop();
        let generation = self.generation + 1;

        *self = Self::start(config, address.port())?;
        self.generation = generation;
        self.wait_ready(address, ready_path, ready_timeout)?;

        Ok(())
    }

    /// Current generation (incremented on restart).
    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// PID of the managed process, if it has not been stopped.
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }
}

impl Drop for ManagedTarget {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// Send one readiness request and report whether the status line is 2xx.
pub(crate) fn probe_ready(address: SocketAddr, path: &str) -> bool {
    let Ok(mut stream) = TcpStream::connect_timeout(&address, Duration::from_millis(100)) else {
        return false;
    };
    let timeout = Some(Duration::from_millis(500));
    if stream.set_read_timeout(timeout).is_err() || stream.set_write_timeout(timeout).is_err() {
        return false;
    }
    let request = format!("GET {path} HTTP/1.0\r\nHost: {address}\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }

    let mut response = Vec::new();
    let mut buffer = [0u8; 256];
    while !response.contains(&b'\n') && response.len() < 1024 {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(n) => response.extend_from_slice(&buffer[..n]),
        }
    }
    is_success_status_line(&response)
}

/// `HTTP/1.x 2xx ...` → true.
fn is_success_status_line(response: &[u8]) -> bool {
    let text = String::from_utf8_lossy(response);
    let mut parts = text.lines().next().unwrap_or("").split_whitespace();
    let version = parts.next().unwrap_or("");
    let code = parts.next().unwrap_or("");
    version.starts_with("HTTP/")
        && code.len() == 3
        && code.starts_with('2')
        && code.bytes().all(|b| b.is_ascii_digit())
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, TcpListener};
    use std::path::PathBuf;

    fn fixture_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/python_api/server.py")
    }

    fn free_address() -> SocketAddr {
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
    }

    fn make_config(mode: &str) -> TargetConfig {
        use crate::experiment::types::ReadyConfig;
        TargetConfig {
            command: vec![
                "python3".into(),
                "-I".into(),
                fixture_path().display().to_string(),
                "--port".into(),
                TARGET_PORT_REFERENCE.into(),
                "--mode".into(),
                mode.into(),
            ],
            ready: ReadyConfig {
                path: "/health".into(),
                timeout_ms: 10000,
            },
            coverage: None,
            cwd: None,
            env: {
                let mut env = std::collections::HashMap::new();
                env.insert(
                    "NADIR_FIXTURE_PASSWORD".into(),
                    "test-password".into(),
                );
                env
            },
        }
    }

    #[test]
    fn test_start_and_wait_ready() {
        let address = free_address();
        let config = make_config("corrected");
        let mut target = ManagedTarget::start(&config, address.port()).unwrap();
        assert!(matches!(target.is_running(), ProcessStatus::Running));

        let result = target.wait_ready(address, "/health", Duration::from_secs(10));
        assert!(result.is_ok(), "wait_ready failed: {:?}", result.err());

        target.stop().unwrap();
    }

    #[test]
    fn test_stop_and_status() {
        let address = free_address();
        let config = make_config("corrected");
        let mut target = ManagedTarget::start(&config, address.port()).unwrap();
        target.wait_ready(address, "/health", Duration::from_secs(10)).unwrap();

        target.stop().unwrap();
        assert_eq!(target.pid(), None);
        assert!(
            matches!(target.is_running(), ProcessStatus::Exited { .. }),
            "expected exited, got {:?}",
            target.is_running()
        );
        // A second stop must not signal anything.
        target.stop().unwrap();
        assert!(TcpStream::connect(address).is_err(), "port still open after stop");
    }

    #[test]
    fn test_stderr_capture() {
        let address = free_address();
        let config = make_config("corrected");
        let mut target = ManagedTarget::start(&config, address.port()).unwrap();
        target.wait_ready(address, "/health", Duration::from_secs(10)).unwrap();
        target.stop().unwrap();

        let stderr = target.stderr_tail();
        let text = String::from_utf8_lossy(&stderr);
        assert!(
            text.contains(&format!("listening on 127.0.0.1:{}", address.port())),
            "expected stderr to contain startup message, got: {}",
            text
        );
    }

    #[test]
    fn test_reports_exit_during_startup() {
        let address = free_address();
        let mut config = make_config("corrected");
        config.command = vec!["python3".into(), "-c".into(), "raise SystemExit(3)".into()];
        let mut target = ManagedTarget::start(&config, address.port()).unwrap();
        let result = target.wait_ready(address, "/health", Duration::from_secs(10));
        assert!(matches!(result, Err(StartupError::ExitedDuringStartup)), "{result:?}");
        assert_eq!(
            target.is_running(),
            ProcessStatus::Exited { code: Some(3), signal: None }
        );
    }

    #[test]
    fn test_startup_error_invalid_command() {
        let config = TargetConfig {
            command: vec!["/nonexistent/binary".into()],
            ready: crate::experiment::types::ReadyConfig {
                path: "/health".into(),
                timeout_ms: 1000,
            },
            coverage: None,
            cwd: None,
            env: Default::default(),
        };
        let result = ManagedTarget::start(&config, 18083);
        assert!(result.is_err(), "expected startup error");
    }

    #[test]
    fn test_success_status_line() {
        assert!(is_success_status_line(b"HTTP/1.0 200 OK\r\n"));
        assert!(is_success_status_line(b"HTTP/1.1 204 No Content\r\n"));
        assert!(!is_success_status_line(b"HTTP/1.0 404 Not Found\r\nDate: 2026\r\n"));
        assert!(!is_success_status_line(b"HTTP/1.0 503 Service Unavailable\r\n"));
        assert!(!is_success_status_line(b"SSH-2.0-OpenSSH\r\n"));
        assert!(!is_success_status_line(b""));
    }
}
