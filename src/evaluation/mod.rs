//! Oracle evaluation: openapi conformance, assert, record_absent.
//!
//! # Design notes
//!
//! **Chosen:** each oracle type has a single `evaluate_*` function that
//! returns a `Verdict`. No `Result<Verdict, Error>` — there is no oracle
//! error, only `inconclusive`.

use std::collections::HashMap;

use crate::adapters::http::transport::HttpObservation;
use crate::observation::StepObservation;
use crate::execution::runner::StepResult;
use crate::experiment::selectors::ObservationSelector;
use crate::experiment::types::{ExpectCondition, OracleDecl, OracleType};
use crate::observation::{ObservationError, SelectedValue};

use serde_json::Value;
pub mod case;
pub mod schema_validator;

use schema_validator::{ResponseCheck, SchemaValidatorCache};

// ── Verdict ────────────────────────────────────────────────────────

/// The result of evaluating one oracle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Fail { category: ViolationCategory },
    Skipped,
    Inconclusive,
}

/// Category of a failed oracle.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ViolationCategory {
    /// Schema conformance failure.
    Conformance,
    /// Security property violated.
    Security,
    /// General assertion failure.
    Assertion,
    /// The service answered with a 5xx status.
    ServerError,
    /// The service neither answered nor closed the connection in time.
    Unresponsive,
    /// A service started although a variable broke its specification.
    AcceptedInvalidConfig,
    /// The process died from a panic or a signal.
    Crash,
}

// ── Oracle context ─────────────────────────────────────────────────

/// Context provided to oracle evaluation functions.
pub struct OracleContext<'a> {
    /// The result of the current step.
    pub current_step: &'a StepResult,
    /// Results of all previous steps in declaration order.
    pub previous_steps: &'a [StepResult],
    pub case_id: &'a str,
    /// Flattened step outputs: key = "step_id.output_name".
    pub step_outputs: &'a HashMap<String, Value>,
    /// The operation's internal method and path key, used to look up response schemas.
    pub operation_id: &'a str,
    /// Status codes the operation documents (`0` is `default`); empty when unknown.
    pub declared_statuses: &'a [u16],
    /// Pre-compiled schema validators for the run.
    /// Key: `(operation key, status code)`.
    pub schema_cache: &'a SchemaValidatorCache,
}

pub(crate) struct OracleEvaluation {
    pub verdict: Verdict,
    pub observed: Option<SelectedValue>,
    pub reason: Option<&'static str>,
    pub schema_location: Option<String>,
}

pub fn evaluate_oracle(oracle: &OracleDecl, ctx: &OracleContext<'_>) -> Verdict {
    evaluate_check(oracle, ctx).verdict
}

pub fn evaluate_assert(oracle: &OracleDecl, ctx: &OracleContext<'_>) -> Verdict {
    evaluate_rule(OracleType::Assert, oracle, ctx).verdict
}

pub fn evaluate_record_absent(oracle: &OracleDecl, ctx: &OracleContext<'_>) -> Verdict {
    evaluate_rule(OracleType::RecordAbsent, oracle, ctx).verdict
}

pub(crate) fn evaluate_check(oracle: &OracleDecl, ctx: &OracleContext<'_>) -> OracleEvaluation {
    evaluate_rule(oracle.oracle_type.clone(), oracle, ctx)
}

fn evaluate_rule(
    oracle_type: OracleType,
    oracle: &OracleDecl,
    ctx: &OracleContext<'_>,
) -> OracleEvaluation {
    let make = |verdict, observed, reason| OracleEvaluation {
        verdict,
        observed,
        reason,
        schema_location: None,
    };
    if let Err(error) = available_response(ctx.current_step) {
        return make(
            Verdict::Inconclusive,
            Some(SelectedValue::Unavailable(error)),
            Some(error.description()),
        );
    }
    if let Some(when) = &oracle.when_condition {
        match resolve_observation_selector(&when.select, ctx) {
            SelectedValue::Unavailable(error) => {
                return make(
                    Verdict::Inconclusive,
                    Some(SelectedValue::Unavailable(error)),
                    Some(error.description()),
                );
            }
            SelectedValue::Present(value) if values_equal(&value, &when.equals) => {}
            _ => return make(Verdict::Skipped, None, Some("condition did not match")),
        }
    }
    match oracle_type {
        OracleType::OpenApi => make(
            evaluate_openapi(ctx.current_step.observation.as_ref().and_then(StepObservation::as_http), ctx.declared_statuses),
            None,
            None,
        ),
        OracleType::OpenApiSchema => schema_evaluation(
            ctx.current_step.observation.as_ref().and_then(StepObservation::as_http),
            ctx.operation_id,
            ctx.schema_cache,
        ),
        OracleType::Assert => {
            let (Some(selector), Some(expect)) = (&oracle.select, &oracle.expect) else {
                return make(
                    Verdict::Inconclusive,
                    None,
                    Some("assertion has no selector or expectation"),
                );
            };
            let observed = resolve_observation_selector(selector, ctx);
            if let SelectedValue::Unavailable(error) = observed {
                return make(
                    Verdict::Inconclusive,
                    Some(observed),
                    Some(error.description()),
                );
            }
            let pass = match (expect, &observed) {
                (ExpectCondition::Equals(expected), SelectedValue::Present(actual)) => {
                    values_equal(actual, &resolve_expectation(expected, ctx.case_id))
                }
                (ExpectCondition::OneOf(options), SelectedValue::Present(actual)) => options
                    .iter()
                    .any(|option| values_equal(actual, &resolve_expectation(option, ctx.case_id))),
                (ExpectCondition::Exists, SelectedValue::Present(_))
                | (ExpectCondition::Absent, SelectedValue::Missing) => true,
                (ExpectCondition::Rejected, SelectedValue::Present(actual)) => {
                    actual.as_u64().is_some_and(|status| status >= 400)
                }
                _ => false,
            };
            let verdict = if pass {
                Verdict::Pass
            } else {
                Verdict::Fail {
                    category: ViolationCategory::Assertion,
                }
            };
            make(verdict, Some(observed), None)
        }
        OracleType::RecordAbsent => {
            let response = ctx
                .current_step
                .observation
                .as_ref()
                .and_then(StepObservation::as_http);
            match response.map(|response| response.status) {
                Some(404) => make(Verdict::Pass, None, None),
                Some(200) if oracle.record_reference.as_ref().is_none_or(|selector| {
                    matches!(resolve_observation_selector(selector, ctx), SelectedValue::Present(value) if !value.is_null())
                }) => make(Verdict::Fail { category: ViolationCategory::Security }, None, None),
                _ => make(Verdict::Inconclusive, None, Some("response did not establish record presence")),
            }
        }
    }
}

/// Check the HTTP status range and syntax of a nonempty response declared as JSON.
/// Basic conformance: no 5xx, a status the operation documents (any status
/// when it has a `default` response or `declared` is empty), and a parsable
/// body when the response says it is JSON.
pub fn evaluate_openapi(observation: Option<&HttpObservation>, declared: &[u16]) -> Verdict {
    let Some(response) = observation.filter(|response| !response.truncated) else {
        return Verdict::Inconclusive;
    };
    if (500..600).contains(&response.status) {
        return Verdict::Fail {
            category: ViolationCategory::ServerError,
        };
    }
    let documented = declared.is_empty()
        || declared.contains(&0)
        || declared.contains(&response.status);
    let json = response
        .headers
        .iter()
        .any(|(key, value)| key.eq_ignore_ascii_case("content-type") && value.contains("json"));
    if !(200..600).contains(&response.status)
        || !documented
        || (json
            && !response.body.is_empty()
            && crate::formats::json::decode(&response.body).is_err())
    {
        Verdict::Fail {
            category: ViolationCategory::Conformance,
        }
    } else {
        Verdict::Pass
    }
}

/// Evaluate the response declared for the exact status code.
pub fn evaluate_openapi_schema(
    observation: Option<&HttpObservation>,
    operation_id: &str,
    schema_cache: &SchemaValidatorCache,
) -> Verdict {
    schema_evaluation(observation, operation_id, schema_cache).verdict
}

fn schema_evaluation(
    observation: Option<&HttpObservation>,
    operation_id: &str,
    schema_cache: &SchemaValidatorCache,
) -> OracleEvaluation {
    let make = |verdict, reason, schema_location| OracleEvaluation {
        verdict,
        observed: None,
        reason,
        schema_location,
    };
    let fail = |reason, location| {
        make(
            Verdict::Fail {
                category: ViolationCategory::Conformance,
            },
            Some(reason),
            location,
        )
    };
    let Some(response) = observation.filter(|response| !response.truncated) else {
        return make(
            Verdict::Inconclusive,
            Some("response unavailable or truncated"),
            None,
        );
    };
    let response_location = || Some(format!("response {}", response.status));
    let Some(check) = schema_cache
        .get(operation_id)
        .and_then(|responses| responses.get(&response.status))
    else {
        return fail(
            "status is not declared in the OpenAPI contract",
            response_location(),
        );
    };
    match check {
        ResponseCheck::Bodyless if response.body.is_empty() => make(Verdict::Pass, None, None),
        ResponseCheck::Bodyless => fail("response must have no body", response_location()),
        ResponseCheck::Json(_) if response.body.is_empty() => {
            fail("response requires a JSON body", response_location())
        }
        ResponseCheck::Json(validator) => match crate::formats::json::decode(&response.body) {
            Err(_) => fail("response body is not valid JSON", response_location()),
            Ok(instance) => match validator.validate(&instance) {
                Ok(()) => make(Verdict::Pass, None, None),
                Err(error) => fail(
                    "response does not match the declared schema",
                    Some(format!(
                        "response {} schema {}",
                        response.status,
                        error.location()
                    )),
                ),
            },
        },
    }
}

pub(crate) fn resolve_expectation(value: &Value, case_id: &str) -> Value {
    if value.as_str() == Some("${case.id}") {
        return Value::String(case_id.to_owned());
    }
    value.clone()
}

fn values_equal(first: &Value, second: &Value) -> bool {
    match (first, second) {
        (Value::Number(first), Value::Number(second)) if first.is_f64() || second.is_f64() => first
            .as_f64()
            .zip(second.as_f64())
            .is_some_and(|(first, second)| (first - second).abs() < f64::EPSILON),
        _ => first == second,
    }
}

fn available_response(step: &StepResult) -> Result<&StepObservation, ObservationError> {
    if step.outcome != crate::execution::runner::StepOutcome::Completed {
        return Err(ObservationError::NoResponse);
    }
    let response = step
        .observation
        .as_ref()
        .ok_or(ObservationError::NoResponse)?;
    if response.is_truncated() {
        Err(ObservationError::ResponseLimit)
    } else {
        Ok(response)
    }
}

pub(crate) fn resolve_observation_selector(
    selector: &ObservationSelector,
    ctx: &OracleContext<'_>,
) -> SelectedValue {
    let (selector, step) = match selector {
        ObservationSelector::Step { step_id, selector } => {
            let Some(step) = ctx
                .previous_steps
                .iter()
                .find(|step| &step.step_id == step_id)
            else {
                return SelectedValue::Unavailable(ObservationError::NoResponse);
            };
            (selector.as_ref(), step)
        }
        selector => (selector, ctx.current_step),
    };
    match available_response(step) {
        Ok(response) => response.select(selector),
        Err(error) => SelectedValue::Unavailable(error),
    }
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::http::transport::HttpObservation;
    use crate::execution::runner::{StepOutcome, StepResult};
    use crate::experiment::types::WhenCondition;
    use std::collections::HashMap;

    fn make_step_result(status: u16, body: &[u8]) -> StepResult {
        StepResult {
            request_diagnostic: None,
            step_id: "test".into(),
            outcome: StepOutcome::Completed,
            observation: Some(StepObservation::Http(HttpObservation {
                status,
                headers: vec![("content-type".into(), "application/json".into())],
                body: body.to_vec(),
                truncated: false,
            })),
            extracted_outputs: HashMap::new(),
            sensitive_response: false,
        }
    }

    static EMPTY_OUTPUTS: std::sync::LazyLock<HashMap<String, Value>> =
        std::sync::LazyLock::new(HashMap::new);
    static EMPTY_SCHEMA_CACHE: std::sync::LazyLock<SchemaValidatorCache> =
        std::sync::LazyLock::new(SchemaValidatorCache::new);

    fn make_ctx<'a>(step: &'a StepResult) -> OracleContext<'a> {
        OracleContext {
            current_step: step,
            previous_steps: &[],
            case_id: "case-1",
            step_outputs: &EMPTY_OUTPUTS,
            operation_id: "",
            declared_statuses: &[],
            schema_cache: &EMPTY_SCHEMA_CACHE,
        }
    }

    #[test]
    fn test_openapi_flags_server_errors_and_undocumented_statuses() {
        let oracle = OracleDecl {
            id: "openapi".into(),
            oracle_type: OracleType::OpenApi,
            kind: None,
            setup: false,
            select: None,
            when_condition: None,
            expect: None,
            record_reference: None,
        };
        let evaluate = |status: u16, declared: &[u16]| {
            let step = make_step_result(status, b"");
            evaluate_oracle(
                &oracle,
                &OracleContext {
                    declared_statuses: declared,
                    ..make_ctx(&step)
                },
            )
        };
        let server_error = Verdict::Fail {
            category: ViolationCategory::ServerError,
        };
        let conformance = Verdict::Fail {
            category: ViolationCategory::Conformance,
        };
        assert_eq!(evaluate(500, &[200, 404]), server_error);
        assert_eq!(evaluate(503, &[200, 503]), server_error);
        assert_eq!(evaluate(503, &[]), server_error);
        assert_eq!(evaluate(418, &[200, 404]), conformance);
        assert_eq!(evaluate(404, &[200, 404]), Verdict::Pass);
        // `default` (0) documents every status, but a 5xx is still an error.
        assert_eq!(evaluate(418, &[0, 200]), Verdict::Pass);
        assert_eq!(evaluate(500, &[0, 200]), server_error);
        // Without contract information only the status range is checked.
        assert_eq!(evaluate(418, &[]), Verdict::Pass);
    }

    #[test]
    fn test_openapi_pass_matching_status() {
        let step = make_step_result(200, br#"{"ok": true}"#);
        let ctx = make_ctx(&step);
        let oracle = OracleDecl {
            id: "openapi".into(),
            oracle_type: OracleType::OpenApi,
            kind: None,
            setup: false,
            select: None,
            when_condition: None,
            expect: None,
            record_reference: None,
        };
        assert_eq!(evaluate_oracle(&oracle, &ctx), Verdict::Pass);
    }

    #[test]
    fn test_openapi_fail_invalid_json() {
        let step = make_step_result(200, b"not json");
        let ctx = make_ctx(&step);
        let oracle = OracleDecl {
            id: "openapi".into(),
            oracle_type: OracleType::OpenApi,
            kind: None,
            setup: false,
            select: None,
            when_condition: None,
            expect: None,
            record_reference: None,
        };
        assert_eq!(
            evaluate_oracle(&oracle, &ctx),
            Verdict::Fail {
                category: ViolationCategory::Conformance
            }
        );
    }

    #[test]
    fn test_openapi_inconclusive_on_transport_error() {
        let step = StepResult {
            request_diagnostic: None,
            step_id: "test".into(),
            outcome: StepOutcome::TransportError {
                error: "timeout".into(),
                transport_reason: Some(crate::adapters::http::transport::TransportReason::Timeout),
                transport_phase: None,
            },
            observation: None,
            extracted_outputs: HashMap::new(),
            sensitive_response: false,
        };
        let ctx = make_ctx(&step);
        let oracle = OracleDecl {
            id: "openapi".into(),
            oracle_type: OracleType::OpenApi,
            kind: None,
            setup: false,
            select: None,
            when_condition: None,
            expect: None,
            record_reference: None,
        };
        assert_eq!(evaluate_oracle(&oracle, &ctx), Verdict::Inconclusive);
    }

    #[test]
    fn test_assert_equals_pass() {
        let step = make_step_result(200, br#"{"value": "hello"}"#);
        let ctx = make_ctx(&step);
        let oracle = OracleDecl {
            id: "eq".into(),
            oracle_type: OracleType::Assert,
            kind: None,
            setup: false,
            select: Some(ObservationSelector::Body(vec!["value".into()])),
            when_condition: None,
            expect: Some(ExpectCondition::Equals(Value::String("hello".into()))),
            record_reference: None,
        };
        assert_eq!(evaluate_oracle(&oracle, &ctx), Verdict::Pass);
    }

    #[test]
    fn test_assert_equals_fail() {
        let step = make_step_result(200, br#"{"value": "wrong"}"#);
        let ctx = make_ctx(&step);
        let oracle = OracleDecl {
            id: "eq".into(),
            oracle_type: OracleType::Assert,
            kind: None,
            setup: false,
            select: Some(ObservationSelector::Body(vec!["value".into()])),
            when_condition: None,
            expect: Some(ExpectCondition::Equals(Value::String("expected".into()))),
            record_reference: None,
        };
        assert!(matches!(
            evaluate_oracle(&oracle, &ctx),
            Verdict::Fail { .. }
        ));
    }

    #[test]
    fn test_assert_exists_pass() {
        let step = make_step_result(200, br#"{"value": "present"}"#);
        let ctx = make_ctx(&step);
        let oracle = OracleDecl {
            id: "ex".into(),
            oracle_type: OracleType::Assert,
            kind: None,
            setup: false,
            select: Some(ObservationSelector::Body(vec!["value".into()])),
            when_condition: None,
            expect: Some(ExpectCondition::Exists),
            record_reference: None,
        };
        assert_eq!(evaluate_oracle(&oracle, &ctx), Verdict::Pass);
    }

    #[test]
    fn test_assert_absent_fail_on_present() {
        let step = make_step_result(200, br#"{"value": "present"}"#);
        let ctx = make_ctx(&step);
        let oracle = OracleDecl {
            id: "abs".into(),
            oracle_type: OracleType::Assert,
            kind: None,
            setup: false,
            select: Some(ObservationSelector::Body(vec!["value".into()])),
            when_condition: None,
            expect: Some(ExpectCondition::Absent),
            record_reference: None,
        };
        assert!(matches!(
            evaluate_oracle(&oracle, &ctx),
            Verdict::Fail { .. }
        ));
    }

    #[test]
    fn test_record_absent_pass_on_404() {
        let step = make_step_result(404, br#"{"error":"not found"}"#);
        let ctx = make_ctx(&step);
        let oracle = OracleDecl {
            id: "ra".into(),
            oracle_type: OracleType::RecordAbsent,
            kind: Some(crate::experiment::types::OracleKind::Security),
            setup: false,
            select: None,
            when_condition: None,
            expect: None,
            record_reference: None,
        };
        assert_eq!(evaluate_oracle(&oracle, &ctx), Verdict::Pass);
    }

    #[test]
    fn test_record_absent_fail_on_200() {
        let step = make_step_result(200, br#"{"reference":"abc"}"#);
        let ctx = make_ctx(&step);
        let oracle = OracleDecl {
            id: "ra".into(),
            oracle_type: OracleType::RecordAbsent,
            kind: Some(crate::experiment::types::OracleKind::Security),
            setup: false,
            select: None,
            when_condition: None,
            expect: None,
            record_reference: None,
        };
        assert!(matches!(
            evaluate_oracle(&oracle, &ctx),
            Verdict::Fail { .. }
        ));
    }

    #[test]
    fn test_assert_skipped_when_false() {
        let step = make_step_result(200, br#"{"value": "x"}"#);
        let ctx = make_ctx(&step);
        let oracle = OracleDecl {
            id: "skip".into(),
            oracle_type: OracleType::Assert,
            kind: None,
            setup: false,
            select: Some(ObservationSelector::Body(vec!["value".into()])),
            when_condition: Some(WhenCondition {
                select: ObservationSelector::Status,
                equals: Value::Number(201.into()),
            }),
            expect: Some(ExpectCondition::Exists),
            record_reference: None,
        };
        assert_eq!(evaluate_oracle(&oracle, &ctx), Verdict::Skipped);
    }

    fn assertion(expect: ExpectCondition) -> OracleDecl {
        OracleDecl {
            id: "assertion".into(),
            oracle_type: OracleType::Assert,
            kind: None,
            setup: false,
            select: Some(ObservationSelector::Body(vec!["value".into()])),
            when_condition: None,
            expect: Some(expect),
            record_reference: None,
        }
    }

    #[test]
    fn unavailable_responses_cannot_pass_or_fail_an_assertion() {
        let mut no_response = make_step_result(200, b"{}");
        no_response.outcome = StepOutcome::TransportError {
            error: "unavailable".into(),
            transport_reason: None,
            transport_phase: None,
        };
        no_response.observation = None;
        let mut truncated = make_step_result(200, b"{}");
        let Some(StepObservation::Http(response)) = &mut truncated.observation else {
            panic!("expected an HTTP observation");
        };
        response.truncated = true;
        let invalid_json = make_step_result(200, b"incomplete");
        for step in [&no_response, &truncated, &invalid_json] {
            for expect in [
                ExpectCondition::Absent,
                ExpectCondition::Equals(Value::Null),
            ] {
                assert_eq!(
                    evaluate_oracle(&assertion(expect), &make_ctx(step)),
                    Verdict::Inconclusive
                );
            }
        }
    }

    #[test]
    fn null_is_present_and_distinct_from_missing() {
        let present = make_step_result(200, br#"{"value":null}"#);
        let missing = make_step_result(200, b"{}");
        let fail = Verdict::Fail {
            category: ViolationCategory::Assertion,
        };
        for (expect, present_verdict, missing_verdict) in [
            (ExpectCondition::Exists, Verdict::Pass, fail.clone()),
            (ExpectCondition::Absent, fail.clone(), Verdict::Pass),
            (
                ExpectCondition::Equals(Value::Null),
                Verdict::Pass,
                fail.clone(),
            ),
            (
                ExpectCondition::OneOf(vec![Value::Null]),
                Verdict::Pass,
                fail,
            ),
        ] {
            let oracle = assertion(expect);
            assert_eq!(
                evaluate_oracle(&oracle, &make_ctx(&present)),
                present_verdict
            );
            assert_eq!(
                evaluate_oracle(&oracle, &make_ctx(&missing)),
                missing_verdict
            );
        }
    }

    #[test]
    fn resolves_when_against_previous_response_and_distinguishes_inconclusive() {
        let step = make_step_result(200, br#"{"value":null}"#);
        let mut prior = make_step_result(201, b"{}");
        prior.step_id = "creation".into();
        let mut oracle = assertion(ExpectCondition::Exists);
        oracle.when_condition = Some(WhenCondition {
            select: ObservationSelector::Step {
                step_id: "creation".into(),
                selector: Box::new(ObservationSelector::Status),
            },
            equals: Value::from(201),
        });
        let evaluate_with = |previous_steps| {
            evaluate_oracle(
                &oracle,
                &OracleContext {
                    previous_steps,
                    ..make_ctx(&step)
                },
            )
        };
        assert_eq!(evaluate_with(std::slice::from_ref(&prior)), Verdict::Pass);
        let mut nonmatching = make_step_result(200, b"{}");
        nonmatching.step_id = "creation".into();
        assert_eq!(
            evaluate_with(std::slice::from_ref(&nonmatching)),
            Verdict::Skipped
        );
        assert_eq!(evaluate_with(&[]), Verdict::Inconclusive);
    }

    #[test]
    fn keeps_explicit_null_in_yaml_expectations() {
        let explicit: crate::config::RawExpectBlock = serde_yaml::from_str("equals: null").unwrap();
        let omitted: crate::config::RawExpectBlock = serde_yaml::from_str("exists: true").unwrap();
        assert_eq!(explicit.equals, Some(Value::Null));
        assert_eq!(omitted.equals, None);
    }
}
