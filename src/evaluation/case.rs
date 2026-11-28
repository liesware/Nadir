use crate::execution::runner::{CaseResult, StepOutcome};
use crate::experiment::selectors::ObservationSelector;
use crate::experiment::types::{
    ExpectCondition, Experiment, OracleDecl, OracleType, ReplacementValue,
};
use crate::mutation::RegressionVariant;

use super::schema_validator::SchemaValidatorCache;

/// Implicit rule added to `mode: schema` cases whose value the schema forbids.
pub(crate) const SCHEMA_REJECTION_RULE: &str = "schema:rejects-invalid";
/// The implicit rule of every `malform` case.
pub(crate) const WIRE_HANDLED_RULE: &str = "wire:handled";
/// The implicit rule of every `envar` `mode: spec` case.
pub(crate) const STARTUP_REFUSES_RULE: &str = "startup:refuses";
/// The implicit rule of the `unset` case of an optional `mode: spec` variable.
pub(crate) const STARTUP_DEFAULTS_RULE: &str = "startup:defaults";
/// The implicit rule of every `envfile` case.
pub(crate) const ENVFILE_REFUSES_RULE: &str = "envfile:refuses";

/// The file and the broken form of an `envfile` case, from the step.
fn envfile_case(step: &crate::experiment::types::Step, name: &str) -> Option<(String, crate::adapters::cli::envbreak::Applied)> {
    step.mutators.iter().find_map(|mutator| match mutator {
        crate::experiment::types::MutatorDecl::EnvFile { file, base, cases } => cases
            .iter()
            .find(|case| case.name == name)
            .map(|case| (file.clone(), crate::adapters::cli::envbreak::apply(base, case))),
        _ => None,
    })
}

/// How a `mode: spec` case is judged.
enum SpecJudgement<'a> {
    /// `startup:refuses`, with the variable and the value sent (`""` when unset).
    Refuses(&'a str, &'a str),
    /// `startup:defaults`: the variable was removed and has a default.
    Defaults(&'a str),
}

/// The `mode: spec` rule of `variable` in `step`, if it has one.
fn spec_rule<'a>(step: &'a crate::experiment::types::Step, variable: &str) -> Option<&'a crate::adapters::cli::spec::SpecRule> {
    use crate::experiment::types::{EnvarVariableRule, MutatorDecl};
    step.mutators.iter().find_map(|mutator| match mutator {
        MutatorDecl::Envar { variables, .. } => variables.iter().find_map(|candidate| match &candidate.rule {
            EnvarVariableRule::Spec(rule) if candidate.name == variable => Some(rule),
            _ => None,
        }),
        _ => None,
    })
}
use super::{evaluate_check, OracleContext, OracleEvaluation, Verdict};

pub struct EvaluatedCase {
    pub execution: CaseResult,
    pub plan: ExecutedPlan,
    pub(crate) checks: Vec<Vec<CaseCheck>>,
    /// Set when the target process exited during this case.
    pub process_exit: Option<crate::artifacts::ProcessExitDiagnostic>,
}

pub enum ExecutedPlan {
    Baseline,
    Corpus(crate::mutation::text::CorpusCaseRef),
    Variant {
        step_id: String,
        selector: String,
        replacement: ReplacementValue,
        replay: Option<RegressionVariant>,
    },
    /// A random case from `strategy: fuzz`, reproducible from its seed and iteration.
    Fuzz {
        step_id: String,
        replacements: Vec<(String, ReplacementValue)>,
        seed: u64,
        iteration: u64,
        /// `GeneratorVersion` number that built the case.
        generator: u32,
        max_fields: Option<u16>,
        key_expectation: Option<(String, u16)>,
    },
    /// An enumerated `envar` case: one variable of a cli step set to `value`.
    Envar {
        step_id: String,
        variable: String,
        case: crate::artifacts::EnvarCase,
        /// `None`: the variable was removed.
        value: Option<String>,
    },
    /// An `envfile` case: one cli step's `.env`-style file broken one way.
    EnvFile {
        step_id: String,
        name: String,
    },
    /// A `malform` case: one `http_raw` step's request broken one way.
    Malform {
        step_id: String,
        malformation: crate::adapters::http_raw::Malformation,
    },
    /// A case generated from a `cases.examples` entry.
    Example {
        example_id: String,
        step_id: String,
        expect_status: u16,
        json_example: Option<crate::experiment::types::JsonExample>,
    },
}

pub(crate) struct CaseCheck {
    pub oracle: OracleDecl,
    pub evaluation: OracleEvaluation,
    pub record_observation_on_pass: bool,
}

impl EvaluatedCase {
    pub(crate) fn new(
        experiment: &Experiment,
        execution: CaseResult,
        plan: ExecutedPlan,
        schema_cache: &SchemaValidatorCache,
    ) -> Self {
        let outputs = std::collections::HashMap::new();
        let checks = execution
            .step_results
            .iter()
            .enumerate()
            .map(|(index, result)| {
                let step = experiment
                    .steps
                    .iter()
                    .find(|step| step.id == result.step_id);
                let operation_id = step.map(|s| s.operation_key.as_str()).unwrap_or("");
                let declared_statuses = step.map(|s| s.declared_statuses.as_slice()).unwrap_or(&[]);
                let context = OracleContext {
                    current_step: result,
                    previous_steps: &execution.step_results[..index],
                    case_id: &execution.case_id,
                    step_outputs: &outputs,
                    operation_id,
                    declared_statuses,
                    schema_cache,
                };
                // A malformed request is judged by `wire:handled` alone: the
                // step's own oracles describe the well-formed baseline.
                let malformed = matches!(&plan, ExecutedPlan::Malform { step_id, .. } if step_id == &result.step_id);
                // A broken file is judged by `envfile:refuses` alone.
                let envfile = match &plan {
                    ExecutedPlan::EnvFile { step_id, name } if step_id == &result.step_id => {
                        step.and_then(|step| envfile_case(step, name))
                    }
                    _ => None,
                };
                // A spec case is judged by `startup:refuses` alone, like `malform`.
                let spec_case = match &plan {
                    ExecutedPlan::Envar { step_id, variable, case, value } if step_id == &result.step_id => {
                        use crate::artifacts::EnvarCase;
                        match (case, step.and_then(|step| spec_rule(step, variable))) {
                            (EnvarCase::Spec(_), _) => {
                                Some(SpecJudgement::Refuses(variable, value.as_deref().unwrap_or_default()))
                            }
                            (EnvarCase::Unset, Some(rule)) if rule.required => Some(SpecJudgement::Refuses(variable, "")),
                            (EnvarCase::Unset, Some(_)) => Some(SpecJudgement::Defaults(variable)),
                            _ => None,
                        }
                    }
                    _ => None,
                };
                let mut checks: Vec<_> = step
                    .into_iter()
                    .flat_map(|step| &step.oracles)
                    .map(|oracle| CaseCheck {
                        oracle: oracle.clone(),
                        evaluation: if malformed || spec_case.is_some() || envfile.is_some() {
                            OracleEvaluation { verdict: Verdict::Skipped, observed: None, reason: None, schema_location: None }
                        } else {
                            evaluate_check(oracle, &context)
                        },
                        record_observation_on_pass: false,
                    })
                    .collect();
                if let Some(judgement) = spec_case {
                    let (id, expect, evaluation) = match judgement {
                        SpecJudgement::Refuses(variable, value) => {
                            (STARTUP_REFUSES_RULE, ExpectCondition::Refused, startup_refuses(result, variable, value))
                        }
                        SpecJudgement::Defaults(variable) => {
                            (STARTUP_DEFAULTS_RULE, ExpectCondition::StartsWithDefault, startup_defaults(result, variable))
                        }
                    };
                    checks.push(CaseCheck {
                        oracle: OracleDecl {
                            id: id.into(),
                            oracle_type: OracleType::Assert,
                            kind: None,
                            setup: false,
                            select: Some(ObservationSelector::ProcessReady),
                            when_condition: None,
                            expect: Some(expect),
                            record_reference: None,
                        },
                        evaluation,
                        record_observation_on_pass: true,
                    });
                }
                if let Some((file, applied)) = &envfile {
                    checks.push(CaseCheck {
                        oracle: OracleDecl {
                            id: ENVFILE_REFUSES_RULE.into(),
                            oracle_type: OracleType::Assert,
                            kind: None,
                            setup: false,
                            select: Some(ObservationSelector::ProcessReady),
                            when_condition: None,
                            expect: Some(ExpectCondition::Refused),
                            record_reference: None,
                        },
                        evaluation: envfile_refuses(result, file, applied),
                        record_observation_on_pass: true,
                    });
                }
                if malformed {
                    checks.push(CaseCheck {
                        oracle: OracleDecl {
                            id: WIRE_HANDLED_RULE.into(),
                            oracle_type: OracleType::Assert,
                            kind: None,
                            setup: false,
                            select: Some(ObservationSelector::Status),
                            when_condition: None,
                            expect: Some(ExpectCondition::Handled),
                            record_reference: None,
                        },
                        evaluation: wire_handled(result),
                        record_observation_on_pass: true,
                    });
                }
                // A schema-invalid value must be rejected by the step that received it.
                if let ExecutedPlan::Variant {
                    step_id,
                    replay: Some(variant),
                    ..
                } = &plan
                {
                    if step_id == &result.step_id && variant.expects_rejection() {
                        let oracle = OracleDecl {
                            id: SCHEMA_REJECTION_RULE.into(),
                            oracle_type: OracleType::Assert,
                            kind: None,
                            setup: false,
                            select: Some(ObservationSelector::Status),
                            when_condition: None,
                            expect: Some(ExpectCondition::Rejected),
                            record_reference: None,
                        };
                        checks.push(CaseCheck {
                            evaluation: evaluate_check(&oracle, &context),
                            record_observation_on_pass: true,
                            oracle,
                        });
                    }
                }
                let expectation = match &plan {
                    ExecutedPlan::Example { example_id, step_id, expect_status, .. } =>
                        Some((step_id, format!("example:{example_id}"), *expect_status)),
                    ExecutedPlan::Fuzz { step_id, key_expectation: Some((key, status)), .. } =>
                        Some((step_id, format!("fuzz:key:{key}"), *status)),
                    _ => None,
                };
                if let Some((_, id, status)) = expectation.filter(|(step, _, _)| *step == &result.step_id) {
                        let oracle = OracleDecl {
                            id,
                            oracle_type: OracleType::Assert,
                            kind: None,
                            setup: false,
                            select: Some(ObservationSelector::Status),
                            when_condition: None,
                            expect: Some(ExpectCondition::Equals(serde_json::Value::from(
                                status,
                            ))),
                            record_reference: None,
                        };
                        checks.push(CaseCheck {
                            evaluation: evaluate_check(&oracle, &context),
                            record_observation_on_pass: true,
                            oracle,
                        });
                }
                checks
            })
            .collect();
        Self {
            execution,
            plan,
            checks,
            process_exit: None,
        }
    }

    pub(crate) fn setup_failure(&self) -> Option<(usize, usize)> {
        if !matches!(self.plan, ExecutedPlan::Baseline) {
            return None;
        }
        self.checks.iter().enumerate().find_map(|(step, checks)| {
            checks
                .iter()
                .position(|check| check.oracle.setup && check.evaluation.verdict != Verdict::Pass)
                .map(|check| (step, check))
        })
    }

    pub(crate) fn is_incomplete(&self) -> bool {
        self.execution.step_results.iter().enumerate().any(|(index, step)| {
            // A malformed request that ended in a close was judged, not lost.
            let judged = self.checks.get(index).is_some_and(|checks| {
                checks.iter().any(|check| {
                    check.oracle.id == WIRE_HANDLED_RULE && check.evaluation.verdict != Verdict::Inconclusive
                })
            });
            !judged && (step.outcome != StepOutcome::Completed
                || step
                    .observation
                    .as_ref()
                    .is_none_or(|response| response.is_truncated()))
        }) || self
            .checks
            .iter()
            .flatten()
            .any(|check| check.evaluation.verdict == Verdict::Inconclusive)
    }

    pub(crate) fn failures(
        &self,
    ) -> impl Iterator<Item = (usize, usize, &super::ViolationCategory)> + '_ {
        self.checks
            .iter()
            .enumerate()
            .flat_map(move |(step, checks)| {
                checks
                    .iter()
                    .enumerate()
                    .filter_map(move |(check, result)| match &result.evaluation.verdict {
                        Verdict::Fail { category } => Some((step, check, category)),
                        _ => None,
                    })
            })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Passed,
    Failed,
    Incomplete,
}

impl RunStatus {
    pub(crate) fn classify(setup_failed: bool, has_findings: bool, incomplete: bool) -> Self {
        match (setup_failed, has_findings, incomplete) {
            (true, _, _) | (false, false, true) => Self::Incomplete,
            (false, true, _) => Self::Failed,
            (false, false, false) => Self::Passed,
        }
    }

    pub(crate) fn for_case(case: &EvaluatedCase) -> Self {
        Self::classify(
            case.setup_failure().is_some(),
            case.failures().next().is_some(),
            case.is_incomplete(),
        )
    }
}

/// Only values carrying this marker are searched for in the output: others
/// (`0`, ` sqlite `) appear in normal messages by chance.
const ECHO_MARKER: &str = "nadir";

/// `startup:refuses`: with one variable breaking its specification, the
/// service must exit before ready with a non-zero code (not a panic, a
/// signal or a timeout), name the variable, and not echo the value.
fn startup_refuses(result: &crate::execution::runner::StepResult, variable: &str, value: &str) -> OracleEvaluation {
    use crate::observation::{SelectedValue, StepObservation};
    use super::ViolationCategory::*;
    let verdict = |verdict, observed: String| OracleEvaluation {
        verdict,
        observed: Some(SelectedValue::Present(serde_json::Value::String(observed))),
        reason: None,
        schema_location: None,
    };
    let Some(StepObservation::Cli(run)) = &result.observation else {
        return OracleEvaluation { verdict: Verdict::Inconclusive, observed: None, reason: None, schema_location: None };
    };
    let output = format!("{}\n{}", run.stdout, run.stderr);
    if run.ready == Some(true) {
        return verdict(Verdict::Fail { category: AcceptedInvalidConfig }, "started (ready)".into());
    }
    if run.timed_out {
        return verdict(Verdict::Fail { category: Unresponsive }, "neither ready nor exited before the timeout".into());
    }
    if run.exit_code == 101 || run.exit_code < 0 {
        return verdict(Verdict::Fail { category: Crash }, format!("exited {} (panic or signal)", run.exit_code));
    }
    if run.exit_code == 0 {
        return verdict(Verdict::Fail { category: AcceptedInvalidConfig }, "exited 0".into());
    }
    // A value that is not UTF-8 is looked for by its readable prefix.
    let token = crate::adapters::cli::osvalue::echo_token(value);
    if token.contains(ECHO_MARKER) && output.contains(&token) {
        return verdict(Verdict::Fail { category: Security }, format!("exited {}, echoing the value", run.exit_code));
    }
    if !output.contains(variable) {
        return verdict(Verdict::Fail { category: Assertion }, format!("exited {} without naming {variable}", run.exit_code));
    }
    verdict(Verdict::Pass, format!("exited {}, naming {variable}", run.exit_code))
}

/// `envfile:refuses`: with its file broken, the program must refuse (not
/// ready, non-zero exit, no panic, signal or hang), say where (`line N`, or
/// the file name for a file-level problem) and not print the injected value.
fn envfile_refuses(
    result: &crate::execution::runner::StepResult,
    file: &str,
    applied: &crate::adapters::cli::envbreak::Applied,
) -> OracleEvaluation {
    use crate::adapters::cli::envbreak::Location;
    use crate::observation::{SelectedValue, StepObservation};
    use super::ViolationCategory::*;
    let verdict = |verdict, observed: String| OracleEvaluation {
        verdict,
        observed: Some(SelectedValue::Present(serde_json::Value::String(observed))),
        reason: None,
        schema_location: None,
    };
    let Some(StepObservation::Cli(run)) = &result.observation else {
        return OracleEvaluation { verdict: Verdict::Inconclusive, observed: None, reason: None, schema_location: None };
    };
    let output = format!("{}\n{}", run.stdout, run.stderr);
    if run.ready == Some(true) {
        return verdict(Verdict::Fail { category: AcceptedInvalidConfig }, "started (ready)".into());
    }
    if run.timed_out {
        return verdict(Verdict::Fail { category: Unresponsive }, "neither ready nor exited before the timeout".into());
    }
    if run.exit_code == 101 || run.exit_code < 0 {
        return verdict(Verdict::Fail { category: Crash }, format!("exited {} (panic or signal)", run.exit_code));
    }
    if run.exit_code == 0 {
        return verdict(Verdict::Fail { category: AcceptedInvalidConfig }, "exited 0".into());
    }
    if applied.token.is_some_and(|token| output.contains(token)) {
        return verdict(Verdict::Fail { category: Security }, format!("exited {}, printing the value", run.exit_code));
    }
    let name = std::path::Path::new(file).file_name().and_then(|name| name.to_str()).unwrap_or(file);
    let (expected, found) = match applied.location {
        Location::Line(line) => (format!("line {line}"), output.contains(&format!("line {line}"))),
        Location::File => (name.to_owned(), output.contains(name)),
    };
    if !found {
        return verdict(Verdict::Fail { category: Assertion }, format!("exited {} without naming {expected}", run.exit_code));
    }
    verdict(Verdict::Pass, format!("exited {}, naming {expected}", run.exit_code))
}

/// `startup:defaults`: without an optional variable the program must start
/// with its documented default: ready with `service`, exit 0 without it.
fn startup_defaults(result: &crate::execution::runner::StepResult, variable: &str) -> OracleEvaluation {
    use crate::observation::{SelectedValue, StepObservation};
    use super::ViolationCategory::*;
    let verdict = |verdict, observed: String| OracleEvaluation {
        verdict,
        observed: Some(SelectedValue::Present(serde_json::Value::String(observed))),
        reason: None,
        schema_location: None,
    };
    let Some(StepObservation::Cli(run)) = &result.observation else {
        return OracleEvaluation { verdict: Verdict::Inconclusive, observed: None, reason: None, schema_location: None };
    };
    // The error is usually the last line of stderr; stdout only without one.
    let last = |text: &str| text.lines().rfind(|line| !line.trim().is_empty()).map(str::to_owned);
    let last_line = last(&run.stderr).or_else(|| last(&run.stdout)).unwrap_or_default();
    match (run.ready, run.timed_out, run.exit_code) {
        (Some(true), _, _) => verdict(Verdict::Pass, "started with the default (ready)".into()),
        (None, false, 0) => verdict(Verdict::Pass, "exited 0 with the default".into()),
        (_, true, _) => verdict(Verdict::Fail { category: Unresponsive }, "neither ready nor exited before the timeout".into()),
        (_, _, code) if code == 101 || code < 0 => {
            verdict(Verdict::Fail { category: Crash }, format!("exited {code} (panic or signal) without {variable}"))
        }
        (_, _, code) => verdict(
            Verdict::Fail { category: Assertion },
            format!("exited {code} without {variable}, which has a default: {last_line}"),
        ),
    }
}

/// `wire:handled`: a malformed request must get an answer without a 5xx, or
/// a closed connection, before the timeout. Accepting it (2xx/3xx) is
/// tolerated: HTTP/1.1 allows recipients to accept some forms.
fn wire_handled(result: &crate::execution::runner::StepResult) -> OracleEvaluation {
    use crate::adapters::http::transport::TransportReason;
    use crate::observation::SelectedValue;
    let evaluation = |verdict, observed: String| OracleEvaluation {
        verdict,
        observed: Some(SelectedValue::Present(serde_json::Value::String(observed))),
        reason: None,
        schema_location: None,
    };
    match (&result.outcome, result.observation.as_ref().and_then(|observation| observation.http_status())) {
        (StepOutcome::Completed, Some(status)) if (500..600).contains(&status) => evaluation(
            Verdict::Fail { category: super::ViolationCategory::ServerError },
            format!("HTTP {status}"),
        ),
        (StepOutcome::Completed, Some(status)) => evaluation(Verdict::Pass, format!("HTTP {status}")),
        (StepOutcome::TransportError { transport_reason: Some(TransportReason::ConnectionClosed), .. }, _) => {
            evaluation(Verdict::Pass, "connection closed".into())
        }
        (StepOutcome::TransportError { transport_reason: Some(TransportReason::Reset), .. }, _) => {
            evaluation(Verdict::Pass, "connection reset".into())
        }
        (StepOutcome::TransportError { transport_reason: Some(TransportReason::Timeout), .. }, _) => evaluation(
            Verdict::Fail { category: super::ViolationCategory::Unresponsive },
            "no answer and no close before the timeout".into(),
        ),
        (StepOutcome::TransportError { error, .. }, _) => evaluation(Verdict::Inconclusive, error.clone()),
        _ => OracleEvaluation { verdict: Verdict::Inconclusive, observed: None, reason: None, schema_location: None },
    }
}

#[cfg(test)]
mod tests {
    use super::RunStatus;

    #[test]
    fn classifies_combined_outcomes() {
        assert_eq!(RunStatus::classify(false, false, false), RunStatus::Passed);
        assert_eq!(
            RunStatus::classify(false, false, true),
            RunStatus::Incomplete
        );
        assert_eq!(RunStatus::classify(false, true, true), RunStatus::Failed);
        assert_eq!(RunStatus::classify(true, true, true), RunStatus::Incomplete);
    }

    #[test]
    fn startup_refuses_wants_a_clean_named_exit_without_the_value() {
        use super::{startup_refuses, Verdict};
        use crate::adapters::cli::CliObservation;
        use crate::evaluation::ViolationCategory::*;
        use crate::execution::runner::{StepOutcome, StepResult};
        use crate::observation::StepObservation;
        let run = |exit_code, stderr: &str, ready, timed_out| StepResult {
            step_id: "serve".into(),
            outcome: StepOutcome::Completed,
            observation: Some(StepObservation::Cli(CliObservation {
                exit_code,
                stdout: String::new(),
                stderr: stderr.into(),
                timed_out,
                ready: Some(ready),
                stopped_code: ready.then_some(0),
            })),
            extracted_outputs: Default::default(),
            sensitive_response: false,
            request_diagnostic: None,
        };
        let verdict = |result| startup_refuses(&result, "MODE", "nadir-invalid").verdict;
        assert_eq!(verdict(run(1, "MODE (process): must be dev or prod", false, false)), Verdict::Pass);
        assert_eq!(verdict(run(0, "", true, false)), Verdict::Fail { category: AcceptedInvalidConfig });
        assert_eq!(verdict(run(101, "panicked", false, false)), Verdict::Fail { category: Crash });
        assert_eq!(verdict(run(-1, "", false, false)), Verdict::Fail { category: Crash });
        assert_eq!(verdict(run(-1, "", false, true)), Verdict::Fail { category: Unresponsive });
        assert_eq!(verdict(run(0, "MODE bad", false, false)), Verdict::Fail { category: AcceptedInvalidConfig });
        assert_eq!(verdict(run(1, "bad MODE nadir-invalid", false, false)), Verdict::Fail { category: Security });
        assert_eq!(verdict(run(1, "something failed", false, false)), Verdict::Fail { category: Assertion });
        // Values without the marker are not searched for: they appear by chance.
        let plain = startup_refuses(&run(1, "MODE must be dev or prod", false, false), "MODE", " dev ");
        assert_eq!(plain.verdict, Verdict::Pass);
    }

    #[test]
    fn wire_handled_accepts_answers_and_closes_and_flags_5xx_and_silence() {
        use super::{wire_handled, Verdict};
        use crate::evaluation::ViolationCategory;
        use crate::adapters::http::transport::{FailurePhase, HttpObservation, TransportReason};
        use crate::execution::runner::{StepOutcome, StepResult};
        use crate::observation::StepObservation;
        let answered = |status| StepResult {
            request_diagnostic: None,
            step_id: "axum".into(),
            outcome: StepOutcome::Completed,
            observation: Some(StepObservation::Http(HttpObservation {
                status,
                headers: Vec::new(),
                body: Vec::new(),
                truncated: false,
            })),
            extracted_outputs: Default::default(),
            sensitive_response: false,
        };
        let lost = |reason| StepResult {
            request_diagnostic: None,
            step_id: "axum".into(),
            outcome: StepOutcome::TransportError {
                error: "lost".into(),
                transport_reason: Some(reason),
                transport_phase: Some(FailurePhase::BeforeHeaders),
            },
            observation: None,
            extracted_outputs: Default::default(),
            sensitive_response: false,
        };
        assert_eq!(wire_handled(&answered(400)).verdict, Verdict::Pass);
        assert_eq!(wire_handled(&answered(200)).verdict, Verdict::Pass);
        assert_eq!(wire_handled(&lost(TransportReason::ConnectionClosed)).verdict, Verdict::Pass);
        assert_eq!(wire_handled(&lost(TransportReason::Reset)).verdict, Verdict::Pass);
        assert_eq!(
            wire_handled(&answered(502)).verdict,
            Verdict::Fail { category: ViolationCategory::ServerError }
        );
        assert_eq!(
            wire_handled(&lost(TransportReason::Timeout)).verdict,
            Verdict::Fail { category: ViolationCategory::Unresponsive }
        );
        assert_eq!(wire_handled(&lost(TransportReason::ConnectionFailed)).verdict, Verdict::Inconclusive);
    }
}
