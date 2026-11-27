//! CLI dispatch and regression output.

mod args;

use std::path::Path;
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
    WorkflowRunner::from_resolved(configuration).map_err(|error| error.to_string())
}

fn cmd_run(path: &Path) -> Result<Exit, String> {
    let configuration = load_configuration(path)?;
    let mut runner = regression_runner(&configuration)?;
    let target = start_target(&configuration, &mut runner)?;
    let destination = storage::RunDirectory::reserve(&configuration.directory)
        .map_err(|error| error.to_string())?;
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
    let result = exploration::run_enumeration(&mut runner, &configuration.experiment, &mut budget)
        .map_err(|error| format!("could not execute the regression workflow: {error}"))?;
    let written = destination
        .write(&configuration, &result, runner.diagnostic_secrets())
        .map_err(|error| error.to_string())?;
    // The enumeration already waited for any exit in progress.
    finish_target(target, Duration::ZERO);
    reporting::print_regression_run(path, &result, &written);
    Ok(regression_exit(result.status()))
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
        runner.diagnostic_secrets(),
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
        configuration.experiment.adapters.http.as_ref(),
    ) else {
        return Ok(None);
    };
    let running = target::launch(
        target_config,
        &http.base_url,
        &configuration.directory,
        runner.diagnostic_secrets().to_vec(),
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
