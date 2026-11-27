//! Start an experiment's `target` process for a CLI run, wait until it is
//! ready, and report on it afterwards.
//!
//! # Design notes
//!
//! **Chosen:** the target listens on the address of `adapters.http.base_url`;
//! `${target.port}` expands to that port. Paths in the `target` block are
//! relative to the experiment file, like `adapters.http.openapi`. The running
//! process is shared between the CLI (which stops it and reports on it) and the
//! workflow runner (which polls it to detect a crash mid-case).

use std::cell::RefCell;
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use url::{Host, Url};

use crate::experiment::selectors::{parse_binding_value, BindingParseResult, Reference};
use crate::experiment::types::{CompositePart, TargetConfig};
use crate::target::monitor::ProcessMonitor;
use crate::target::supervisor::{ManagedTarget, ProcessStatus, StartupError, StopError};

/// Lines of target stderr shown in diagnostics.
const STDERR_EXCERPT_LINES: usize = 20;

/// Errors while launching the target for a run.
#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error("target: adapters.http.base_url must be an IP literal URL to start target.command")]
    Address,
    #[error("target: cannot use the experiment directory {path}: {error}")]
    Directory { path: String, error: std::io::Error },
    #[error(
        "target: {0} is already accepting connections; stop that service or remove the target block"
    )]
    AddressInUse(SocketAddr),
    #[error(
        "target: missing environment variable '{0}' (set it, or give a default with ${{env.{0}:-value}})"
    )]
    Environment(String),
    #[error("target: {0}")]
    Reference(String),
    #[error("target: {error}{stderr}")]
    Startup { error: StartupError, stderr: String },
}

/// A target process started for this run.
pub struct RunningTarget {
    process: Rc<RefCell<ManagedTarget>>,
    secrets: Vec<String>,
}

/// Start `config.command`, then wait for `config.ready.path` to return 2xx.
///
/// `secrets` are redacted from any stderr excerpt in errors and reports.
pub fn launch(
    config: &TargetConfig,
    base_url: &str,
    experiment_dir: &Path,
    secrets: Vec<String>,
) -> Result<RunningTarget, LaunchError> {
    let address = socket_address(base_url).ok_or(LaunchError::Address)?;
    if TcpStream::connect_timeout(&address, Duration::from_millis(200)).is_ok() {
        return Err(LaunchError::AddressInUse(address));
    }

    let experiment_dir = experiment_dir
        .canonicalize()
        .map_err(|error| LaunchError::Directory {
            path: experiment_dir.display().to_string(),
            error,
        })?;
    let mut config = config.clone();
    let port = address.port();
    config.command = config
        .command
        .iter()
        .map(|value| expand(value, port))
        .collect::<Result<_, _>>()?;
    for value in config.env.values_mut() {
        *value = expand(value, port)?;
    }
    let cwd = match &config.cwd {
        Some(cwd) => experiment_dir.join(cwd),
        None => experiment_dir,
    };
    config.cwd = Some(cwd.to_string_lossy().into_owned());

    let mut secrets = secrets;
    secrets.extend(config.env.values().cloned());

    let mut process =
        ManagedTarget::start(&config, address.port()).map_err(|error| LaunchError::Startup {
            error,
            stderr: String::new(),
        })?;
    let timeout = Duration::from_millis(config.ready.timeout_ms);
    if let Err(error) = process.wait_ready(address, &config.ready.path, timeout) {
        let _ = process.stop();
        return Err(LaunchError::Startup {
            error,
            stderr: stderr_excerpt(&process.stderr_tail(), &secrets),
        });
    }

    Ok(RunningTarget {
        process: Rc::new(RefCell::new(process)),
        secrets,
    })
}

impl RunningTarget {
    /// A monitor the workflow runner polls to notice that the target exited.
    pub fn monitor(&self) -> Box<dyn ProcessMonitor> {
        Box::new(SharedMonitor(self.process.clone()))
    }

    /// PID of the target process while it runs.
    pub fn pid(&self) -> Option<u32> {
        self.process.borrow().pid()
    }

    /// Stop the target. If it exited on its own (waiting up to `grace` for an
    /// exit in progress), describe the exit and include the end of its stderr.
    pub fn finish(self, grace: Duration) -> (Option<String>, Result<(), StopError>) {
        let mut process = self.process.borrow_mut();
        let deadline = std::time::Instant::now() + grace;
        let mut status = process.is_running();
        while status == ProcessStatus::Running && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
            status = process.is_running();
        }
        let stopped = process.stop();
        let report = match status {
            ProcessStatus::Exited { code, signal } => Some(format!(
                "target process exited unexpectedly ({}){}",
                describe_exit(code, signal),
                stderr_excerpt(&process.stderr_tail(), &self.secrets)
            )),
            _ => None,
        };
        (report, stopped)
    }
}

struct SharedMonitor(Rc<RefCell<ManagedTarget>>);

impl ProcessMonitor for SharedMonitor {
    fn status(&mut self) -> ProcessStatus {
        self.0.borrow_mut().is_running()
    }
}

/// Expand `${target.port}` and `${env.NAME[:-default]}` in a target value.
fn expand(value: &str, port: u16) -> Result<String, LaunchError> {
    let parts = match parse_binding_value(&serde_json::Value::String(value.to_owned())) {
        BindingParseResult::Reference(reference) => vec![CompositePart::Reference(reference)],
        BindingParseResult::Composite(parts) => parts,
        BindingParseResult::Error(error) => return Err(LaunchError::Reference(error.to_string())),
        _ => return Ok(value.to_owned()),
    };
    let mut expanded = String::new();
    for part in parts {
        match part {
            CompositePart::Literal(text) => expanded.push_str(&text),
            CompositePart::Reference(Reference::TargetPort) => {
                expanded.push_str(&port.to_string())
            }
            CompositePart::Reference(Reference::Env { name, default }) => {
                let value = match (std::env::var(&name), default) {
                    (Ok(value), Some(default)) if value.is_empty() => default,
                    (Ok(value), _) => value,
                    (Err(std::env::VarError::NotPresent), Some(default)) => default,
                    (Err(_), _) => return Err(LaunchError::Environment(name)),
                };
                expanded.push_str(&value);
            }
            CompositePart::Reference(other) => {
                return Err(LaunchError::Reference(format!("{other} is not available here")))
            }
        }
    }
    Ok(expanded)
}

/// `exit code 1` / `signal 9`.
pub fn describe_exit(code: Option<i32>, signal: Option<i32>) -> String {
    match (code, signal) {
        (Some(code), _) => format!("exit code {code}"),
        (None, Some(signal)) => format!("signal {signal}"),
        (None, None) => "unknown status".into(),
    }
}

fn socket_address(base_url: &str) -> Option<SocketAddr> {
    let url = Url::parse(base_url).ok()?;
    let ip = match url.host()? {
        Host::Ipv4(ip) => IpAddr::V4(ip),
        Host::Ipv6(ip) => IpAddr::V6(ip),
        Host::Domain(_) => return None,
    };
    Some(SocketAddr::new(ip, url.port_or_known_default()?))
}

/// The last lines of stderr, with lines that contain a secret replaced.
fn stderr_excerpt(stderr: &[u8], secrets: &[String]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return String::new();
    }
    let start = lines.len().saturating_sub(STDERR_EXCERPT_LINES);
    let mut excerpt = format!("\n--- last {} lines of target stderr ---", lines.len() - start);
    for line in &lines[start..] {
        let secret = secrets
            .iter()
            .any(|secret| secret.len() >= 4 && line.contains(secret.as_str()));
        excerpt.push('\n');
        excerpt.push_str(if secret { "[redacted]" } else { line });
    }
    excerpt
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_the_target_address_from_base_url() {
        assert_eq!(
            socket_address("http://127.0.0.1:8080"),
            Some("127.0.0.1:8080".parse().unwrap())
        );
        assert_eq!(socket_address("http://[::1]:9000/api"), Some("[::1]:9000".parse().unwrap()));
        assert_eq!(socket_address("http://127.0.0.2"), Some("127.0.0.2:80".parse().unwrap()));
        assert_eq!(socket_address("http://localhost:8080"), None);
    }

    #[test]
    fn expands_port_and_environment_references() {
        assert_eq!(expand("--port=${target.port}", 8080).unwrap(), "--port=8080");
        assert_eq!(expand("plain", 1).unwrap(), "plain");
        assert_eq!(
            expand("${env.NADIR_TEST_SURELY_UNSET_VAR:-corrected}", 1).unwrap(),
            "corrected"
        );
        assert!(matches!(
            expand("${env.NADIR_TEST_SURELY_UNSET_VAR}", 1),
            Err(LaunchError::Environment(name)) if name == "NADIR_TEST_SURELY_UNSET_VAR"
        ));
        assert!(matches!(expand("${case.id}", 1), Err(LaunchError::Reference(_))));
    }

    #[test]
    fn stderr_excerpt_keeps_the_tail_and_redacts_secrets() {
        let stderr: String = (1..=30).map(|n| format!("line {n}\n")).collect::<String>()
            + "password is hunter22\n";
        let excerpt = stderr_excerpt(stderr.as_bytes(), &["hunter22".into(), "1".into()]);
        assert!(excerpt.contains("last 20 lines"), "{excerpt}");
        assert!(!excerpt.contains("line 11\n"), "{excerpt}");
        assert!(excerpt.contains("line 30"), "{excerpt}");
        assert!(excerpt.ends_with("[redacted]"), "{excerpt}");
        assert!(!excerpt.contains("hunter22"), "{excerpt}");
        assert_eq!(stderr_excerpt(b"", &[]), "");
    }
}
