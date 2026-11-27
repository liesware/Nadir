//! Project evaluated workflows to the redacted artifact consumed by storage and the terminal.

use super::{CaseRecord, StepResultRecord, VerdictRecord};
use crate::evaluation::case::EvaluatedCase;
use crate::evaluation::{resolve_expectation, Verdict};
use crate::execution::runner::StepOutcome;
use crate::experiment::selectors::{JsonPathSegment, ObservationSelector};
use crate::experiment::types::{ExpectCondition, Experiment};
use crate::observation::SelectedValue;

pub(crate) fn regression_record<'a>(
    experiment: &Experiment,
    evaluated: &'a EvaluatedCase,
    number: u32,
    diagnostic_secrets: &'a [String],
) -> CaseRecord {
    let case = &evaluated.execution;
    let mut secret_values: Vec<&str> = diagnostic_secrets.iter().map(String::as_str).collect();
    for step in &case.step_results {
        if let Some(source) = experiment
            .steps
            .iter()
            .find(|source| source.id == step.step_id)
        {
            for declaration in source
                .outputs
                .iter()
                .filter(|output| output.secret || step.sensitive_response)
            {
                if let Some(value) = step.extracted_outputs.get(&declaration.name) {
                    collect_secret_strings(value, &mut secret_values);
                }
            }
        }
    }

    let step_results = case
        .step_results
        .iter()
        .zip(&evaluated.checks)
        .map(|(step, checks)| {
            let outcome = match &step.outcome {
                StepOutcome::Completed
                    if step
                        .observation
                        .as_ref()
                        .is_some_and(|observation| observation.truncated) =>
                {
                    "response_limit"
                }
                StepOutcome::Completed => "completed",
                StepOutcome::Skipped { .. } => "skipped (required output unavailable)",
                StepOutcome::TransportError { .. } => "transport_error",
                StepOutcome::InputError { .. } => "input_error",
                StepOutcome::ExtractionError { .. } => "extraction_error",
            };
            let oracle_verdicts = checks
                .iter()
                .map(|check| {
                    let (verdict, category) = match &check.evaluation.verdict {
                        Verdict::Pass => ("pass", None),
                        Verdict::Skipped => ("skipped", None),
                        Verdict::Inconclusive => ("inconclusive", None),
                        Verdict::Fail { category } => ("fail", Some(format!("{category:?}"))),
                    };
                    let mut expected = None;
                    let mut observed = None;
                    if verdict == "fail"
                        || verdict == "inconclusive"
                        || (verdict == "skipped" && check.oracle.setup)
                    {
                        observed = step
                            .observation
                            .as_ref()
                            .map(|value| format!("HTTP {}", value.status));
                    }
                    if verdict == "fail"
                        || verdict == "inconclusive"
                        || (verdict == "pass" && check.record_observation_on_pass)
                    {
                        match (&check.oracle.select, &check.oracle.expect) {
                            (Some(selector), Some(expect)) => {
                                let (source, selector) = match selector {
                                    ObservationSelector::Step { step_id, selector } => {
                                        (step_id, selector.as_ref())
                                    }
                                    selector => (&step.step_id, selector),
                                };
                                let is_secret = experiment
                                    .steps
                                    .iter()
                                    .filter(|step| &step.id == source)
                                    .flat_map(|step| &step.outputs)
                                    .any(|output| {
                                        output.secret && selectors_overlap(&output.from, selector)
                                    });
                                let sensitive_response_data =
                                    !matches!(selector, ObservationSelector::Status)
                                        && case.step_results.iter().any(|step| {
                                            &step.step_id == source && step.sensitive_response
                                        });
                                if is_secret
                                    || sensitive_response_data
                                    || sensitive_selector(selector)
                                {
                                    expected = Some("[redacted]".into());
                                    observed = Some("[redacted]".into());
                                } else {
                                    expected = Some(match expect {
                                        ExpectCondition::Equals(value) => diagnostic_value(
                                            &resolve_expectation(value, &case.case_id),
                                            &secret_values,
                                        ),
                                        ExpectCondition::OneOf(values) => values
                                            .iter()
                                            .map(|value| {
                                                diagnostic_value(
                                                    &resolve_expectation(value, &case.case_id),
                                                    &secret_values,
                                                )
                                            })
                                            .collect::<Vec<_>>()
                                            .join(" or "),
                                        ExpectCondition::Exists => "present".into(),
                                        ExpectCondition::Rejected => "rejection (status >= 400)".into(),
                                        ExpectCondition::Absent => "absent".into(),
                                    });
                                    observed = match &check.evaluation.observed {
                                        Some(SelectedValue::Present(value)) => {
                                            Some(diagnostic_value(value, &secret_values))
                                        }
                                        Some(SelectedValue::Missing) => Some("field absent".into()),
                                        Some(SelectedValue::Unavailable(error)) => {
                                            Some(error.description().into())
                                        }
                                        None => observed,
                                    };
                                }
                            }
                            _ => {
                                expected = Some(match check.oracle.oracle_type {
                                    crate::experiment::types::OracleType::OpenApi => {
                                        let declared = experiment
                                            .steps
                                            .iter()
                                            .find(|source| source.id == step.step_id)
                                            .map(|source| source.declared_statuses.as_slice())
                                            .unwrap_or(&[]);
                                        openapi_expectation(declared)
                                    }
                                    crate::experiment::types::OracleType::OpenApiSchema => {
                                        "response conforms to declared OpenAPI schema".into()
                                    }
                                    crate::experiment::types::OracleType::RecordAbsent => {
                                        "record absent".into()
                                    }
                                    crate::experiment::types::OracleType::Assert => {
                                        "assertion can be evaluated".into()
                                    }
                                });
                            }
                        }
                    }
                    VerdictRecord {
                        oracle_id: check.oracle.id.clone(),
                        verdict: verdict.into(),
                        category,
                        expected,
                        observed,
                        reason: check.evaluation.reason.map(str::to_owned),
                        schema_location: check.evaluation.schema_location.clone(),
                    }
                })
                .collect();
            let (transport_reason, transport_phase) = match &step.outcome {
                StepOutcome::TransportError {
                    transport_reason: reason,
                    transport_phase: phase,
                    ..
                } => (*reason, *phase),
                _ => (None, None),
            };
            StepResultRecord {
                step_id: step.step_id.clone(),
                outcome: outcome.into(),
                status: step.observation.as_ref().map(|response| response.status),
                body_sha256: None,
                extracted_outputs: Default::default(),
                oracle_verdicts,
                transport_reason,
                transport_phase,
            }
        })
        .collect();

    CaseRecord {
        artifact_version: CaseRecord::CURRENT_VERSION,
        case_id: case.case_id.clone(),
        case_number: number,
        step_results,
        coverage_snapshot_sha256: None,
        regression_plan: None,
    }
}

fn collect_secret_strings<'a>(value: &'a serde_json::Value, secrets: &mut Vec<&'a str>) {
    match value {
        serde_json::Value::String(value) if !value.is_empty() => secrets.push(value),
        serde_json::Value::Array(values) => {
            for value in values {
                collect_secret_strings(value, secrets);
            }
        }
        serde_json::Value::Object(values) => {
            for value in values.values() {
                collect_secret_strings(value, secrets);
            }
        }
        _ => {}
    }
}

fn selectors_overlap(first: &ObservationSelector, second: &ObservationSelector) -> bool {
    match (first, second) {
        (ObservationSelector::Body(first), ObservationSelector::Body(second)) => {
            body_path_starts_with(first, second) || body_path_starts_with(second, first)
        }
        (first, second) => first == second,
    }
}

fn body_path_starts_with(longer: &[JsonPathSegment], prefix: &[JsonPathSegment]) -> bool {
    longer.len() >= prefix.len() && longer.iter().zip(prefix).all(|(a, b)| a == b)
}

fn sensitive_selector(selector: &ObservationSelector) -> bool {
    match selector {
        ObservationSelector::Status => false,
        ObservationSelector::Body(path) => path.iter().any(|segment| {
            let key = segment.as_key().map(|k| k.to_ascii_lowercase());
            let Some(key) = key else { return false };
            [
                "pass",
                "secret",
                "token",
                "auth",
                "credential",
                "cookie",
                "api_key",
                "apikey",
            ]
            .iter()
            .any(|piece| key.contains(piece))
        }),
        _ => true,
    }
}

/// `declared status (200, 404), no 5xx, valid JSON`.
fn openapi_expectation(declared: &[u16]) -> String {
    let statuses = if declared.is_empty() || declared.contains(&0) {
        "any status".to_string()
    } else {
        let list: Vec<String> = declared.iter().map(u16::to_string).collect();
        format!("declared status ({})", list.join(", "))
    };
    format!("{statuses}, no 5xx, valid JSON")
}

fn diagnostic_value(value: &serde_json::Value, secrets: &[&str]) -> String {
    match value {
        serde_json::Value::String(value)
            if secrets.iter().any(|&secret| {
                value.as_str() == secret || (secret.len() >= 4 && value.contains(secret))
            }) =>
        {
            "[redacted]".into()
        }
        serde_json::Value::String(value) if value.len() > 160 => "[text omitted]".into(),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            "[structured value omitted]".into()
        }
        _ => value.to_string(),
    }
}
