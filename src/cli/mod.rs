//! CLI dispatch and regression output.

mod args;
mod debugger;
mod incidents;
mod progress;

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;

use crate::artifacts::regression_record;
use crate::evaluation::case::RunStatus;
use crate::execution::runner::StepOutcome;
use crate::execution::WorkflowRunner;
use crate::experiment::types::ExplorationStrategy;
use crate::exploration::{self, RunBudget};
use crate::{config, replay, reporting, storage, target};

pub enum Exit {
    Success = 0,
    Fail = 1,
    Usage = 2,
}

impl From<Exit> for ExitCode {
    fn from(exit: Exit) -> Self {
        ExitCode::from(exit as u8)
    }
}

pub fn run() -> ExitCode {
    let command = match args::Cli::try_parse() {
        Ok(cli) => cli.command,
        Err(error) => {
            let exit = if error.use_stderr() {
                Exit::Usage
            } else {
                Exit::Success
            };
            let _ = error.print();
            return exit.into();
        }
    };

    let result = match command {
        args::Command::Validate { experiment } => load_configuration(&experiment).map(|loaded| {
            eprintln!(
                "experiment '{}' is valid ({} steps)",
                loaded.experiment.name,
                loaded.experiment.steps.len()
            );
            Exit::Success
        }),
        args::Command::Run {
            corpus: Some(_), ..
        } => Err("--corpus is unavailable in this release".into()),
        args::Command::Run { experiment, .. } => cmd_run(&experiment),
        args::Command::Replay { experiment, case } => cmd_replay(&experiment, &case),
    };
    match result {
        Ok(exit) => exit.into(),
        Err(error) => {
            eprintln!("error: {error}");
            Exit::Usage.into()
        }
    }
}

fn load_configuration(path: &Path) -> Result<config::ResolvedExperiment, String> {
    let configuration = config::load_resolved(path).map_err(|error| error.to_string())?;
    for warning in &configuration.warnings {
        eprintln!("warning: {warning}");
    }
    Ok(configuration)
}

/// Prepare the runner. `target.coverage` is still rejected here, before any
/// process starts.
fn regression_runner(configuration: &config::ResolvedExperiment) -> Result<WorkflowRunner, String> {
    let runner = WorkflowRunner::from_resolved(configuration).map_err(|error| error.to_string())?;
    for name in runner.short_hidden() {
        eprintln!(
            "warning: hidden variable '{name}' has fewer than {} characters and is not hidden",
            crate::inspect::MIN_HIDDEN_LEN
        );
    }
    Ok(runner)
}

fn cmd_run(path: &Path) -> Result<Exit, String> {
    let configuration = load_configuration(path)?;
    let mut runner = regression_runner(&configuration)?;
    let progress = Rc::new(RefCell::new(progress::ProgressDisplay::new()));
    let target = start_target(&configuration, &mut runner)?;
    let destination = storage::RunDirectory::reserve(&configuration.directory)
        .map_err(|error| error.to_string())?;
    eprintln!("Endpoints: {}", runner.endpoint_count());
    let templates = runner.request_templates();
    if !templates.is_empty() {
        let mut display = progress.borrow_mut();
        if display.interactive() {
            let width = display.size().0;
            display.set_print(debugger::template_rows(&templates, width));
        } else {
            eprintln!("{}", debugger::template_rows(&templates, 100).join("\n"));
        }
    }
    // Keys are read (raw mode) only while `debug` has something to show.
    let mut keys = None;
    if configuration.experiment.steps.iter().any(|step| step.debug.is_some()) {
        keys = debugger::Keys::start(&progress.borrow());
        let signals = keys.as_ref().map(debugger::Keys::signals);
        runner.set_inspector(Some(Box::new(debugger::Debugger::new(progress.clone(), signals))));
    }
    let secrets = runner.hidden_values().to_vec();
    let mut incidents = incidents::IncidentRecorder::default();
    let mut on_case_evidence = |case: &crate::evaluation::case::EvaluatedCase, number, oracle_evidence| {
        incidents.record_case(&configuration, case, number, &secrets, oracle_evidence, |kind, row| {
            progress.borrow_mut().add_evidence(kind, row);
        });
    };
    let max_cases = configuration.experiment.max_cases.get();
    let mut budget = match configuration.experiment.exploration.strategy {
        ExplorationStrategy::Enumerate => RunBudget::new(max_cases, None, None),
        ExplorationStrategy::Fuzz {
            max_execs,
            duration_s,
            ..
        } => RunBudget::new(
            max_cases,
            Some(max_execs),
            Some(Duration::from_secs(u64::from(duration_s))),
        ),
    };
    progress.borrow_mut().start_clock();
    let result = exploration::enumerate::run_with_progress(
        &mut runner,
        &configuration.experiment,
        &mut budget,
        &mut |snapshot| progress.borrow_mut().update(snapshot),
        &mut on_case_evidence,
    );
    runner.set_inspector(None);
    // Restore the terminal before anything else is printed.
    drop(keys);
    let mut progress = progress.borrow_mut();
    let result = result.map_err(|error| match error {
        crate::execution::runner::RunError::StoppedByUser => {
            "stopped by the user before the baseline was sent; nothing was recorded".to_owned()
        }
        error => format!("could not execute the regression workflow: {error}"),
    })?;
    let written = destination
        .write(&configuration, &result, runner.hidden_values(), incidents.transport_errors())
        .map_err(|error| error.to_string())?;
    let sections = progress.final_sections();
    let duration = progress.final_duration();
    progress.clear();
    // The enumeration already waited for any exit in progress.
    finish_target(target, Duration::ZERO);
    reporting::print_regression_run(
        path, &result, &written, &progress.cases_line(), duration.as_deref(), &sections,
    );
    Ok(regression_exit(result.status()))
}

fn crash_label(record: &crate::artifacts::CaseRecord) -> String {
    let malform = record.regression_plan.as_ref().and_then(|plan| plan.malform.as_ref());
    if let Some(malform) = malform {
        let observed = record
            .step_results
            .iter()
            .flat_map(|step| &step.oracle_verdicts)
            .find(|check| check.oracle_id == crate::evaluation::case::WIRE_HANDLED_RULE && check.verdict == "fail")
            .and_then(|check| check.observed.as_deref())
            .map(|observed| observed.trim_matches('"').to_owned());
        if let Some(observed) = observed {
            return format!("wire:handled · {} · {} · {observed}", malform.step_id, malform.name);
        }
    }
    let failed = record.step_results.iter().find_map(|step| {
        let check = step.oracle_verdicts.iter().find(|check| check.verdict == "fail")?;
        Some((check.oracle_id.as_str(), step))
    });
    match failed {
        Some((rule, step)) => match step.status {
            Some(status) => format!("{rule} · {} · HTTP {status}", step.step_id),
            None => format!("{rule} · {}", step.step_id),
        },
        None => "failed rule".into(),
    }
}

fn cmd_replay(path: &Path, record: &Path) -> Result<Exit, String> {
    let configuration = load_configuration(path)?;
    let mut runner = regression_runner(&configuration)?;
    let replay_error = |error| format!("replay {}: {error}", record.display());
    // Check the saved case before starting a target process for it.
    let prepared =
        replay::prepare_replay(record, &configuration, &runner).map_err(replay_error)?;
    let target = start_target(&configuration, &mut runner)?;
    let result = replay::execute_replay(prepared, &mut runner).map_err(replay_error)?;
    let lost_connection = result
        .case
        .execution
        .step_results
        .iter()
        .any(|step| matches!(step.outcome, StepOutcome::TransportError { .. }));
    let target_exited = finish_target(
        target,
        if lost_connection { TARGET_EXIT_GRACE } else { Duration::ZERO },
    );
    match result.contract {
        replay::ReplayContract::Legacy => {
            eprintln!("Original OpenAPI contract is unverified (version 1 record).")
        }
        replay::ReplayContract::Changed(files) => {
            eprintln!(
                "Replay used a changed OpenAPI contract: {}",
                files.join(", ")
            );
        }
        replay::ReplayContract::Matching => {}
    }
    let report = regression_record(
        runner.experiment(),
        &result.case,
        0,
        runner.hidden_values(),
    );
    reporting::print_replayed_case(&result.case, &report);
    if target_exited {
        // The saved case still brings the service down.
        return Ok(Exit::Fail);
    }
    Ok(regression_exit(RunStatus::for_case(&result.case)))
}

/// Start the experiment's `target.command`, if any, and let the runner watch it.
///
/// Called after the runner is prepared, so configuration errors never start
/// a process. Dropping the returned target stops the process.
fn start_target(
    configuration: &config::ResolvedExperiment,
    runner: &mut WorkflowRunner,
) -> Result<Option<target::RunningTarget>, String> {
    let (Some(target_config), Some(http)) = (
        configuration.experiment.target.as_ref(),
        configuration.experiment.protocols.http.as_ref(),
    ) else {
        return Ok(None);
    };
    let running = target::launch(
        target_config,
        &http.base_url,
        &configuration.directory,
        runner.hidden_values().to_vec(),
    )
    .map_err(|error| error.to_string())?;
    eprintln!(
        "Target ready: pid {} at {}{}",
        running.pid().unwrap_or_default(),
        http.base_url.trim_end_matches('/'),
        target_config.ready.path
    );
    runner.set_monitor(Some(running.monitor()));
    Ok(Some(running))
}

/// How long replay waits for a target exit in progress after a lost connection.
const TARGET_EXIT_GRACE: Duration = Duration::from_millis(500);

/// Stop the target and report it if it exited on its own. Returns whether it did.
fn finish_target(target: Option<target::RunningTarget>, grace: Duration) -> bool {
    let Some(target) = target else {
        return false;
    };
    let (report, stopped) = target.finish(grace);
    if let Err(error) = stopped {
        eprintln!("warning: could not stop the target process: {error}");
    }
    match report {
        Some(report) => {
            eprintln!("{report}");
            true
        }
        None => false,
    }
}

fn regression_exit(status: RunStatus) -> Exit {
    match status {
        RunStatus::Passed => Exit::Success,
        RunStatus::Failed => Exit::Fail,
        RunStatus::Incomplete => Exit::Usage,
    }
}
