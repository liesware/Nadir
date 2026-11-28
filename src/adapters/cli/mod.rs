//! CLI protocol adapter: execute a local command and capture its output.
//!
//! # Design notes
//!
//! **Chosen:** spawn a child process with an isolated environment (the step's
//! `env` map). `stdin` is closed. `stdout` and `stderr` are read in parallel
//! with a 64 KiB limit each. `timeout_ms` is enforced; if exceeded the
//! process is killed and the result is marked as timed out.

pub mod envfile;
pub mod spec;
pub mod envbreak;
pub mod osvalue;
mod process_diagnostic;

pub use process_diagnostic::{ProcessDiagnostic, ProcessStatusOperation};

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::adapters::{AdapterExecution, StepInput, StepProtocol};
use crate::execution::runner::RunError;
use crate::experiment::cli_command::CliCommand;
use crate::experiment::types::{BindingValue, CliSetup, ReplacementValue, ServiceConfig, Step};
use crate::inspect::{Control, RequestTemplate, RequestView, ResponseView};
use crate::observation::StepObservation;

/// Maximum bytes captured per output stream.
const OUTPUT_LIMIT: usize = 64 * 1024;

/// Result of executing a CLI command.
#[derive(Debug, Clone)]
pub struct CliObservation {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    /// `service` steps: whether the ready URL answered 2xx.
    pub ready: Option<bool>,
    /// `service` steps: the exit code after Nadir stopped the ready service.
    pub stopped_code: Option<i32>,
    pub process_diagnostic: Option<ProcessDiagnostic>,
}

impl CliObservation {
    pub(crate) fn exit_diagnostic(&self) -> Option<&ProcessDiagnostic> {
        if self.ready == Some(true) || self.timed_out {
            return None;
        }
        self.process_diagnostic.as_ref()
    }

    pub(crate) fn is_incomplete(&self) -> bool {
        self.timed_out || matches!(
            self.exit_diagnostic(),
            Some(ProcessDiagnostic::SpawnError { .. } | ProcessDiagnostic::StatusError { .. })
        )
    }
}

pub(crate) struct CliStepProtocol {
    timeout: Duration,
}

impl CliStepProtocol {
    pub(crate) fn prepare(
        timeout_ms: u64,
    ) -> Self {
        Self {
            timeout: Duration::from_millis(timeout_ms),
        }
    }

    /// Run `command` and wait for it (no `service`): used by the tests.
    #[cfg(test)]
    fn execute_command(&self, command: &[String], env: &HashMap<String, String>) -> (i32, String, String, bool) {
        let command = CliCommand::parse(Some(command)).unwrap();
        let observation = self.run(command, env, Path::new(""), None, None);
        (observation.exit_code, observation.stdout, observation.stderr, observation.timed_out)
    }

    /// Start `command` in `cwd` with exactly `env`. Without `service`, wait
    /// for it to end (or kill it at the timeout). With `service`, wait until
    /// the ready URL answers 2xx, the process ends, or the ready timeout
    /// passes; a ready service is then stopped with SIGTERM (SIGKILL after 2 s).
    fn run(
        &self,
        command: CliCommand<'_>,
        env: &HashMap<String, String>,
        experiment_dir: &Path,
        cwd: Option<&Path>,
        service: Option<&ServiceConfig>,
    ) -> CliObservation {
        let (program, args) = command.parts();
        let executable = Path::new(program);
        let program = if program.contains('/') && executable.is_relative() && !experiment_dir.as_os_str().is_empty() {
            let path = experiment_dir.join(executable);
            Cow::Owned(std::path::absolute(&path).unwrap_or(path).to_string_lossy().into_owned())
        } else {
            Cow::Borrowed(program)
        };
        let mut process = Command::new(program.as_ref());
        // An encoded value (`osvalue`) reaches the process as its raw bytes.
        let env = env.iter().map(|(name, value)| (name, osvalue::decode(value)));
        process.args(args).env_clear().envs(env).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        if let Some(cwd) = cwd {
            process.current_dir(cwd);
        }
        let mut child = match process.spawn() {
            Ok(child) => child,
            Err(error) => {
                let message = format!("could not start '{program}': {error}");
                let diagnosis = ProcessDiagnostic::SpawnError {
                    message: message.clone(),
                    os_error: error.raw_os_error(),
                };
                return Outcome::Finished(Err(diagnosis))
                    .observe(service.is_some(), String::new(), message);
            }
        };
        let stdout = capture(child.stdout.take());
        let stderr = capture(child.stderr.take());
        let collect = |(buffer, reader): (Arc<Mutex<Vec<u8>>>, Option<JoinHandle<()>>)| {
            if let Some(reader) = reader {
                let _ = reader.join();
            }
            let bytes = buffer.lock().map(|bytes| bytes.clone()).unwrap_or_default();
            String::from_utf8_lossy(&bytes).into_owned()
        };
        let timeout = service.map_or(self.timeout, |service| service.ready_timeout);
        let deadline = Instant::now() + timeout;
        let ready_target = service.and_then(|service| {
            let host = service.ready_url.host_str()?.trim_start_matches('[').trim_end_matches(']').parse().ok()?;
            let port = service.ready_url.port_or_known_default()?;
            let mut path = service.ready_url.path().to_owned();
            if let Some(query) = service.ready_url.query() {
                path = format!("{path}?{query}");
            }
            Some((std::net::SocketAddr::new(host, port), path))
        });
        let mut last_probe = Instant::now() - Duration::from_secs(1);
        let outcome = loop {
            match poll_exit(&mut child) {
                Ok(Some(status)) => break Outcome::Finished(Ok(status)),
                Ok(None) => {}
                Err(error) => {
                    let _ = kill_and_wait(&mut child);
                    break Outcome::Finished(Err(error));
                }
            }
            if let Some((address, path)) = &ready_target {
                if last_probe.elapsed() >= Duration::from_millis(50) {
                    last_probe = Instant::now();
                    if crate::target::supervisor::probe_ready(*address, path) {
                        break Outcome::Ready(stop(&mut child));
                    }
                }
            }
            if Instant::now() >= deadline {
                break Outcome::TimedOut(kill_and_wait(&mut child));
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let (stdout, stderr) = (collect(stdout), collect(stderr));
        outcome.observe(service.is_some(), stdout, stderr)
    }
}

/// How a run ended.
enum Outcome {
    Finished(Result<ExitStatus, ProcessDiagnostic>),
    TimedOut(Result<ExitStatus, ProcessDiagnostic>),
    Ready(Result<ExitStatus, ProcessDiagnostic>),
}

impl Outcome {
    fn observe(self, is_service: bool, stdout: String, stderr: String) -> CliObservation {
        let (status, timed_out, ready) = match self {
            Self::Finished(status) => (status.map(Some), false, is_service.then_some(false)),
            Self::TimedOut(status) => (status.map(|_| None), true, is_service.then_some(false)),
            Self::Ready(status) => (status.map(Some), false, Some(true)),
        };
        let (exit_code, process_diagnostic) = match status {
            Ok(Some(status)) => (
                status.code().unwrap_or(-1),
                status.signal().map(|signal| ProcessDiagnostic::Signaled { signal }),
            ),
            Err(error) => (-1, Some(error)),
            Ok(None) => (-1, None),
        };
        CliObservation {
            exit_code,
            stdout,
            stderr,
            timed_out,
            ready,
            stopped_code: (ready == Some(true)).then_some(exit_code),
            process_diagnostic,
        }
    }
}

/// Read a pipe to its end (keeping at most `OUTPUT_LIMIT` bytes) on a thread.
fn capture<R: Read + Send + 'static>(reader: Option<R>) -> (Arc<Mutex<Vec<u8>>>, Option<JoinHandle<()>>) {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let handle = reader.map(|mut reader| {
        let buffer = buffer.clone();
        std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        let Ok(mut bytes) = buffer.lock() else { break };
                        let room = OUTPUT_LIMIT.saturating_sub(bytes.len());
                        bytes.extend_from_slice(&chunk[..read.min(room)]);
                    }
                }
            }
        })
    });
    (buffer, handle)
}

fn poll_exit(child: &mut Child) -> Result<Option<ExitStatus>, ProcessDiagnostic> {
    child.try_wait().map_err(|error| ProcessDiagnostic::status_error(ProcessStatusOperation::TryWait, error))
}

fn kill_and_wait(child: &mut Child) -> Result<ExitStatus, ProcessDiagnostic> {
    let _ = child.kill();
    child.wait().map_err(|error| ProcessDiagnostic::status_error(ProcessStatusOperation::Wait, error))
}

fn stop(child: &mut Child) -> Result<ExitStatus, ProcessDiagnostic> {
    let pid = nix::unistd::Pid::from_raw(child.id() as i32);
    let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match poll_exit(child) {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(error) => {
                let _ = kill_and_wait(child);
                return Err(error);
            }
        }
        if Instant::now() >= deadline {
            return kill_and_wait(child);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A fresh working directory with the step's files and the helper files
/// `mode: spec` points at; removed when dropped.
struct Workdir(PathBuf);

impl Workdir {
    fn create(setup: &CliSetup, files: &[(String, String)]) -> std::io::Result<Self> {
        let path = std::env::temp_dir().join(format!("nadir-cli-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path)?;
        let workdir = Self(path);
        for (from, to) in files {
            let destination = workdir.0.join(to);
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(setup.dir.join(from), destination)?;
        }
        std::fs::write(workdir.0.join(spec::REGULAR_FILE), b"nadir")?;
        let unreadable = workdir.0.join(spec::UNREADABLE_FILE);
        std::fs::write(&unreadable, b"nadir")?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000))?;
        Ok(workdir)
    }
}

impl Drop for Workdir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// One line for `debug`: what the run did.
fn summary(observation: &CliObservation) -> String {
    if observation.ready == Some(true) {
        return match &observation.process_diagnostic {
            Some(ProcessDiagnostic::Signaled { signal }) => format!("ready, stopped by signal {signal}"),
            Some(error) => format!("ready · {error}"),
            None => format!("ready, stopped with {}", observation.stopped_code.unwrap_or(-1)),
        };
    }

    let last = observation
        .stderr
        .lines()
        .rev()
        .chain(observation.stdout.lines().rev())
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    let output = if last.is_empty() { String::new() } else { format!(" · {last}") };
    if observation.timed_out {
        let timeout = if observation.ready.is_some() {
            "not ready before the timeout (killed)"
        } else {
            "timed out (killed)"
        };
        let diagnosis = observation.process_diagnostic.as_ref()
            .map_or_else(String::new, |diagnosis| format!(" · {diagnosis}"));
        return format!("{timeout}{diagnosis}{output}");
    }
    let termination = observation.process_diagnostic.as_ref()
        .map_or_else(|| format!("exited {}", observation.exit_code), ToString::to_string);
    let before_ready = if observation.ready.is_some() { " before ready" } else { "" };
    if matches!(observation.process_diagnostic, Some(ProcessDiagnostic::SpawnError { .. })) {
        return termination;
    }
    format!("{termination}{before_ready}{output}")
}

impl StepProtocol for CliStepProtocol {
    fn execute(&mut self, step: &Step, input: StepInput<'_>) -> Result<AdapterExecution, RunError> {
        let command = CliCommand::parse(step.command.as_deref())
            .map_err(|error| RunError::Infrastructure(format!("step '{}': {error}", step.id)))?;

        let base_env: HashMap<String, String> = step.env_config.clone().unwrap_or_default();

        // Values with `${…}` references arrive resolved as bindings.
        let mut env = base_env.clone();
        for binding in input.bindings {
            if let (crate::experiment::selectors::InputSelector::EnvVar(name), BindingValue::Literal(value)) =
                (&binding.selector, &binding.value)
            {
                let text = value.as_str().map_or_else(|| value.to_string(), str::to_owned);
                env.insert(name.clone(), text);
            }
        }
        let before_changes = env.clone();
        // Apply replacements: each envar replacement overrides one variable
        for (selector, value) in input.replacements {
            match value {
                ReplacementValue::JsonString(text) => {
                    env.insert(selector.clone(), text.clone());
                }
                // `absent`/`unset`: the process starts without the variable.
                ReplacementValue::Absent => {
                    env.remove(selector);
                }
                _ => {
                    return Ok(AdapterExecution::input_error(format!(
                        "envar replacement for '{selector}' must be a string"
                    )));
                }
            }
        }

        let setup = step.cli.clone().unwrap_or_default();
        let workdir = match &setup.workdir_copy {
            Some(files) => match Workdir::create(&setup, files) {
                Ok(workdir) => Some(workdir),
                Err(error) => return Ok(AdapterExecution::input_error(format!("workdir: {error}"))),
            },
            None => None,
        };
        // `mode: spec` file limits: permission and size cases start from the
        // variable's own file, copied with the wrong mode or padded.
        if let Some(workdir) = workdir.as_ref() {
            for mutator in &step.mutators {
                let crate::experiment::types::MutatorDecl::Envar { variables, .. } = mutator else {
                    continue;
                };
                for variable in variables {
                    let crate::experiment::types::EnvarVariableRule::Spec(rule) = &variable.rule else {
                        continue;
                    };
                    let Some(base_value) = before_changes.get(&variable.name) else {
                        continue;
                    };
                    if spec::prepared(&rule.kind).is_empty() {
                        continue;
                    }
                    if let Err(error) = spec::prepare(&workdir.0, &variable.name, rule, base_value) {
                        return Ok(AdapterExecution::input_error(format!("workdir: {error}")));
                    }
                }
            }
        }
        let broken = if let Some(super::Breakage::EnvFile(prepared)) = input.breakage {
            let evidence = prepared.evidence();
            let Some(workdir) = workdir.as_ref() else {
                return Ok(AdapterExecution::input_error(format!(
                    "envfile case '{}' is not declared for this step", evidence.name
                )));
            };
            if let Err(error) = prepared.write_to(&workdir.0) {
                return Ok(AdapterExecution::input_error(format!("envfile: {error}")));
            }
            Some(evidence)
        } else {
            None
        };
        let cwd = workdir.as_ref().map(|workdir| workdir.0.clone()).or_else(|| {
            (!setup.dir.as_os_str().is_empty()).then(|| setup.dir.clone())
        });

        let mut inspection = input.inspection;
        if let Some(inspection) = inspection.as_mut() {
            let mut lines = vec![command.to_string()];
            if let Some(evidence) = broken {
                let at = match evidence.location {
                    envbreak::Location::Line(line) => format!("line {line}"),
                    envbreak::Location::File => "the whole file".into(),
                };
                lines.push(format!("{}: {}   ({at})", evidence.file, evidence.name));
            }
            for (name, _) in input.replacements {
                let old = before_changes.get(name).map_or("(unset)".to_owned(), |value| format!("{value:?}"));
                match env.get(name) {
                    Some(value) => lines.push(format!("{name}={:?}   (was: {old})", osvalue::display(value))),
                    None => lines.push(format!("{name} unset   (was: {old})")),
                }
            }
            let view = RequestView {
                method: String::new(),
                target: String::new(),
                host: String::new(),
                headers: Vec::new(),
                body: None,
                raw: Some(lines.join("\r\n").into_bytes()),
            };
            if inspection.inspector.before_send(step, &view, &inspection.secrets) == Control::Stop {
                return Err(RunError::StoppedByUser);
            }
        }

        let observation = self.run(command, &env, &setup.dir, cwd.as_deref(), setup.service.as_ref());
        drop(workdir);
        if let Some(inspection) = inspection.as_mut() {
            inspection.inspector.after_send(step, &ResponseView::Process { summary: summary(&observation) }, &inspection.secrets);
        }

        Ok(AdapterExecution {
            result: Ok(StepObservation::Cli(observation)),
            requests_sent: 1,
            request_diagnostic: None,
        })
    }

    fn accepts_authorization(&self, _value: &str) -> bool {
        true // CLI steps don't use authorization
    }

    fn baseline_value(&self, step: &Step, selector: &str) -> Option<ReplacementValue> {
        // A value with references has no static baseline.
        step.env_config.as_ref()?.get(selector).filter(|v| !v.contains("${")).map(|v| {
            ReplacementValue::JsonString(v.clone())
        })
    }

    fn accepts_variant(&self, step: &Step, selector: &str, _value: &ReplacementValue) -> bool {
        // envar replacements are always accepted if the variable exists
        step.env_config.as_ref().is_some_and(|env| env.contains_key(selector))
    }

    fn contract_fingerprint(&self) -> Option<[u8; 32]> {
        None
    }

    /// The command; each `envar` rule as one line (`VAR={{mode}}`) or, with
    /// several variables, as a `for` over them; the `env` values; and how
    /// many variables come from `env_file`. The value each case replaces is
    /// shown by `debug` (`was: …`).
    fn request_template(&self, step: &Step) -> Option<RequestTemplate> {
        use crate::experiment::types::{EnvarMode, EnvarVariableRule, MutatorDecl};
        let command = CliCommand::parse(step.command.as_deref()).ok()?;
        let mut lines = vec![command.to_string()];
        let mut notes = Vec::new();
        for mutator in &step.mutators {
            let MutatorDecl::Envar { variables, skipped } = mutator else {
                continue;
            };
            let mut modes: Vec<&str> = Vec::new();
            let (mut cases, mut random) = (0, false);
            let (mut lengths, mut files) = (std::collections::BTreeSet::new(), std::collections::BTreeSet::new());
            for variable in variables {
                let mode = match &variable.rule {
                    EnvarVariableRule::Fixed(_) => {
                        cases += 1;
                        "value"
                    }
                    EnvarVariableRule::Values(values) => {
                        cases += values.len();
                        "values"
                    }
                    EnvarVariableRule::Mode(mode) => {
                        random = true;
                        match mode {
                            EnvarMode::Alpha => "alpha",
                            EnvarMode::Alphanum => "alphanum",
                            EnvarMode::Num => "num",
                            EnvarMode::Raw => "raw",
                        }
                    }
                    EnvarVariableRule::Text(generator) => {
                        random = true;
                        lengths.insert(format!("length ~{}, max {}", generator.below, generator.max));
                        generator.alphabet.name()
                    }
                    EnvarVariableRule::File { path, lines, .. } => {
                        cases += lines.len();
                        files.insert(path.clone());
                        "file"
                    }
                    EnvarVariableRule::Spec(rule) => {
                        cases += spec::cases_for(&variable.name, rule, step.env_config.as_ref()).len();
                        "spec"
                    }
                    EnvarVariableRule::Absent => {
                        cases += 1;
                        "absent"
                    }
                    EnvarVariableRule::Catalog { unconstrained } => {
                        cases += crate::mutation::envar_catalog(*unconstrained).len();
                        if *unconstrained { "unconstrained" } else { "edge_cases" }
                    }
                };
                if !modes.contains(&mode) {
                    modes.push(mode);
                }
            }
            let marker = format!("{{{{{}}}}}", modes.join("|"));
            let mut count = match (cases, random) {
                (0, _) => "random values".to_owned(),
                (1, false) => "1 case".to_owned(),
                (n, false) => format!("{n} cases"),
                (n, true) => format!("{n} cases + random values"),
            };
            for detail in files.iter().map(|path| format!("from {path}")).chain(lengths) {
                count.push_str(&format!(", {detail}"));
            }
            match variables.as_slice() {
                [] => {}
                [only] => lines.push(format!("{}={marker}   ({count})", only.name)),
                _ => {
                    lines.push(format!("for VAR in {} variables, one per case ({count}):", variables.len()));
                    lines.push(format!("  VAR={marker}"));
                }
            }
            notes.extend(group_skipped(skipped));
        }
        for mutator in &step.mutators {
            if let MutatorDecl::EnvFile { file, cases, .. } = mutator {
                lines.push(format!("{file}: {{{{envfile}}}}   ({} cases)", cases.len()));
            }
        }
        let setup = step.cli.clone().unwrap_or_default();
        // Every value set in the YAML, mutated or not: it is the base the
        // cases change. References stay unexpanded (`${env.NAME}`).
        let mut literal: Vec<(&String, &String)> = step
            .env_config
            .iter()
            .flatten()
            .filter(|(name, _)| !setup.from_env_file.contains(name))
            .collect();
        literal.sort();
        lines.extend(literal.into_iter().map(|(name, value)| format!("{name}={value:?}")));
        let from_file = setup.from_env_file.len();
        if let (Some(file), true) = (&setup.env_file, from_file > 0) {
            lines.push(format!("(+{from_file} variables from {file})"));
        }
        if let Some(service) = &setup.service {
            notes.push(format!("service: ready at {} within {} ms", service.ready_url, service.ready_timeout.as_millis()));
        }
        Some(RequestTemplate {
            step_id: step.id.clone(),
            request: RequestView {
                method: String::new(),
                target: String::new(),
                host: String::new(),
                headers: Vec::new(),
                body: None,
                raw: Some(lines.join("\r\n").into_bytes()),
            },
            notes,
        })
    }
}

/// `used_when` omissions grouped by condition: `skipped: A, B (used when X)`.
fn group_skipped(skipped: &[String]) -> Vec<String> {
    let mut groups: Vec<(&str, Vec<&str>)> = Vec::new();
    for reason in skipped {
        let (name, condition) = reason.split_once(" (").map_or((reason.as_str(), ""), |(n, c)| (n, c));
        match groups.iter_mut().find(|(known, _)| *known == condition) {
            Some((_, names)) => names.push(name),
            None => groups.push((condition, vec![name])),
        }
    }
    groups
        .into_iter()
        .map(|(condition, names)| match condition {
            "" => format!("skipped: {}", names.join(", ")),
            condition => format!("skipped: {} ({condition}", names.join(", ")),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skipped_variables_are_grouped_by_condition() {
        let skipped = [
            "CERT (used when MODE=prod)".to_owned(),
            "KEY (used when MODE=prod)".to_owned(),
            "DSN (used when STORAGE=postgres)".to_owned(),
        ];
        assert_eq!(
            group_skipped(&skipped),
            ["skipped: CERT, KEY (used when MODE=prod)", "skipped: DSN (used when STORAGE=postgres)"]
        );
    }

    #[test]
    fn captures_exit_code_zero() {
        let proto = CliStepProtocol::prepare(5000);
        let (code, stdout, _stderr, timed_out) = proto.execute_command(
            &["echo".into(), "hello".into()],
            &std::collections::HashMap::new(),
        );
        assert_eq!(code, 0, "exit code");
        assert!(stdout.contains("hello"), "stdout: {stdout}");
        assert!(!timed_out, "timed_out");
    }

    #[test]
    fn captures_nonzero_exit() {
        let proto = CliStepProtocol::prepare(5000);
        let (code, _, _, _) = proto.execute_command(
            &["sh".into(), "-c".into(), "exit 42".into()],
            &std::collections::HashMap::new(),
        );
        assert_eq!(code, 42, "exit code");
    }

    #[test]
    fn reports_missing_executable() {
        let proto = CliStepProtocol::prepare(5000);
        let (code, _, stderr, timed_out) = proto.execute_command(
            &["nonexistent_command_xyz".into()],
            &std::collections::HashMap::new(),
        );
        assert_eq!(code, -1, "exit code");
        assert!(!timed_out, "timed_out");
        assert!(stderr.contains("could not start 'nonexistent_command_xyz'"), "{stderr}");
    }

    #[test]
    fn observation_errors_retain_operation_and_preserve_ready_or_timeout() {
        let errno = nix::errno::Errno::ECHILD as i32;
        let poll_error = ProcessDiagnostic::status_error(
            ProcessStatusOperation::TryWait,
            std::io::Error::from_raw_os_error(errno),
        );
        let wait_error = ProcessDiagnostic::status_error(
            ProcessStatusOperation::Wait,
            std::io::Error::other("failed to collect test-secret"),
        );
        let run = Outcome::Finished(Err(poll_error)).observe(true, "previous output".into(), String::new());
        assert_eq!((run.exit_code, run.ready, run.stopped_code), (-1, Some(false), None));
        assert!(matches!(
            run.exit_diagnostic(),
            Some(ProcessDiagnostic::StatusError { operation: ProcessStatusOperation::TryWait, os_error: Some(code), .. })
                if *code == errno
        ));
        assert!(summary(&run).contains("could not observe process status (try_wait)"));
        assert_eq!(run.stdout, "previous output");

        let ready = Outcome::Ready(Err(wait_error.clone())).observe(true, String::new(), String::new());
        assert_eq!((ready.exit_code, ready.ready, ready.stopped_code), (-1, Some(true), Some(-1)));
        assert!(ready.exit_diagnostic().is_none());
        assert!(summary(&ready).starts_with("ready · could not observe process status (wait)"));

        let stopped = Outcome::Ready(Ok(ExitStatus::from_raw(nix::sys::signal::Signal::SIGTERM as i32)))
            .observe(true, String::new(), String::new());
        assert_eq!((stopped.exit_code, stopped.ready, stopped.stopped_code), (-1, Some(true), Some(-1)));
        assert_eq!(summary(&stopped), "ready, stopped by signal 15");
        assert!(stopped.exit_diagnostic().is_none());

        let timeout = Outcome::TimedOut(Err(wait_error)).observe(false, String::new(), String::new());
        assert_eq!(timeout.exit_code, -1);
        assert!(timeout.timed_out && timeout.exit_diagnostic().is_none());
        let response = ResponseView::Process { summary: summary(&timeout) };
        let shown = crate::inspect::render_response(&response, &["test-secret".into()], 160);
        assert!(shown.contains("timed out (killed)") && shown.contains("(wait)"), "{shown}");
        assert!(shown.contains("[hidden]") && !shown.contains("test-secret"), "{shown}");
    }

    #[test]
    fn respects_timeout() {
        let proto = CliStepProtocol::prepare(50); // 50ms timeout
        let (code, _, _, timed_out) = proto.execute_command(
            &["/bin/sleep".into(), "10".into()],
            &std::collections::HashMap::new(),
        );
        assert!(timed_out, "should have timed out");
        // Not the PID: a killed process has no exit code.
        assert_eq!(code, -1);
    }

    #[test]
    fn env_isolation_overrides_base() {
        let proto = CliStepProtocol::prepare(5000);
        let mut env = std::collections::HashMap::new();
        env.insert("NADIR_TEST_VAR".into(), "modified".into());
        let (code, stdout, _, _) = proto.execute_command(
            &["sh".into(), "-c".into(), "echo $NADIR_TEST_VAR".into()],
            &env,
        );
        assert_eq!(code, 0);
        assert!(stdout.contains("modified"), "stdout: {stdout}");
    }
}
