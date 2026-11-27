//! Execution and workflow metadata for the HTTP adapter.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use url::Url;

use super::contract::{build_baseline_input_with_example, ResolvedContract};
use super::request;
use super::template;
use super::transport::HttpClient;
use crate::inspect::{Control, RequestTemplate, ResponseView};

/// Response bytes passed to `debug`; the display shows one row anyway.
const INSPECTED_BODY_BYTES: usize = 4096;
use crate::adapters::{AdapterExecution, StepProtocol, StepInput};
use crate::execution::runner::{PrepareError, RunError, StepOutcome};
use crate::experiment::selectors::{parse_input_selector, InputSelector};
use crate::experiment::types::{Binding, BindingValue, Experiment, ReplacementValue, Step};
use crate::observation::StepObservation;

pub(crate) struct HttpStepProtocol {
    client: HttpClient,
    contracts: Arc<HashMap<String, ResolvedContract>>,
    contract_path: String,
}

impl HttpStepProtocol {
    pub(crate) fn prepare(
        experiment: &Experiment,
        contracts: Arc<HashMap<String, ResolvedContract>>,
    ) -> Result<Self, PrepareError> {
        let config = experiment
            .protocols
            .http
            .as_ref()
            .ok_or(PrepareError::NoHttpProtocol)?;
        let base_url = Url::parse(&config.base_url).map_err(|_| PrepareError::InvalidBaseUrl)?;
        let client = HttpClient::new(base_url, Duration::from_millis(config.timeout_ms))
            .map_err(PrepareError::HttpClient)?;
        Ok(Self {
            client,
            contracts,
            contract_path: config.openapi_path.clone(),
        })
    }

    pub(crate) fn target_port(&self) -> Option<u16> {
        self.client.base_url().port_or_known_default()
    }

    fn contract(&self) -> Option<&ResolvedContract> {
        self.contracts.get(&self.contract_path)
    }

    pub(crate) fn bearer_header(token: &str) -> Option<String> {
        if token.is_empty() {
            return None;
        }
        let header = format!("Bearer {token}");
        http::HeaderValue::from_str(&header).ok()?;
        Some(header)
    }
}

/// The step's bindings plus `Authorization: Bearer <token>` when the step
/// authenticates. Shared with `http_raw`.
pub(crate) fn bindings_with_auth<'a>(
    bindings: &'a [Binding],
    authorization: Option<&Value>,
) -> Result<Cow<'a, [Binding]>, String> {
    let mut bindings = Cow::Borrowed(bindings);
    if let Some(token) = authorization {
        let Some(token) = token.as_str().filter(|text| !text.is_empty()) else {
            return Err(request::InputBuildError::UnresolvedRequiredField {
                field: "bearer token".into(),
            }
            .to_string());
        };
        bindings.to_mut().push(Binding {
            selector: InputSelector::HeaderParam("Authorization".into()),
            value: BindingValue::Literal(Value::String(format!("Bearer {token}"))),
        });
    }
    Ok(bindings)
}

impl StepProtocol for HttpStepProtocol {
    fn execute(&mut self, step: &Step, input: StepInput<'_>) -> Result<AdapterExecution, RunError> {
        let operation = self
            .contract()
            .and_then(|contract| contract.operation(&step.operation_key))
            .ok_or(RunError::MissingContract)?;
        let bindings = match bindings_with_auth(input.bindings, input.authorization.as_ref()) {
            Ok(bindings) => bindings,
            Err(error) => return Ok(AdapterExecution::input_error(error)),
        };
        let request = match request::build_step_request_with_example(
            operation,
            &bindings,
            input.replacements,
            self.client.base_url(),
            step.body_example.as_ref(),
            step.format,
            input.json_example,
        ) {
            Ok(request) => request,
            Err(error) => return Ok(AdapterExecution::input_error(error.to_string())),
        };
        let mut inspection = input.inspection;
        if let Some(inspection) = inspection.as_mut() {
            let view = template::request_view(&request);
            if inspection.inspector.before_send(step, &view, &inspection.secrets) == Control::Stop {
                return Err(RunError::StoppedByUser);
            }
        }
        let before = self.client.requests_sent();
        let result = self
            .client
            .execute(&request)
            .map(StepObservation::Http)
            .map_err(|error| StepOutcome::TransportError {
                error: error.to_string(),
                transport_reason: Some(error.reason()),
                transport_phase: error.phase(),
            });
        if let Some(inspection) = inspection.as_mut() {
            let response = match &result {
                Ok(StepObservation::Http(observation)) => ResponseView::Received {
                    status: observation.status,
                    body: observation.body[..observation.body.len().min(INSPECTED_BODY_BYTES)].to_vec(),
                },
                Err(StepOutcome::TransportError { error, .. }) => ResponseView::Failed { error: error.clone() },
                _ => ResponseView::Failed { error: "no response".into() },
            };
            inspection.inspector.after_send(step, &response, &inspection.secrets);
        }
        Ok(AdapterExecution {
            request_diagnostic: result.is_err().then(|| request::request_diagnostic(operation, Some(&request))),
            result,
            requests_sent: self.client.requests_sent().wrapping_sub(before),
        })
    }

    fn accepts_authorization(&self, value: &str) -> bool {
        Self::bearer_header(value).is_some()
    }

    fn baseline_value(&self, step: &Step, selector: &str) -> Option<ReplacementValue> {
        baseline_value(self.contract()?, step, selector)
    }

    fn accepts_variant(&self, step: &Step, selector: &str, value: &ReplacementValue) -> bool {
        let operation = self
            .contract()
            .and_then(|contract| contract.operation(&step.operation_key));
        let Some(operation) = operation else {
            return false;
        };
        match (parse_input_selector(selector), value) {
            // The header must still be a declared parameter, or one the step
            // binds (an API key is a security scheme, not a parameter). Values
            // are not checked against the enum: mutated values leave it on
            // purpose.
            (Ok(InputSelector::HeaderParam(header)), ReplacementValue::HeaderAscii(_) | ReplacementValue::Absent) => {
                operation.header_params.iter().any(|param| param.name.eq_ignore_ascii_case(&header))
                    || step.bindings.iter().any(|binding| matches!(
                        &binding.selector,
                        InputSelector::HeaderParam(bound) if bound.eq_ignore_ascii_case(&header)
                    ))
            }
            // A path value needs its parameter in the path template.
            (Ok(InputSelector::PathParam(name)), ReplacementValue::JsonString(_) | ReplacementValue::Absent) => {
                operation.path_params.iter().any(|param| param.name == name)
            }
            // Typed values and removals come from the field's schema or the
            // experiment itself, so the field only has to still exist.
            (Ok(InputSelector::BodyField(path)), ReplacementValue::JsonValue(_))
            | (Ok(InputSelector::BodyField(path)), ReplacementValue::Absent) => {
                operation.request_body.as_ref().is_some_and(|body| {
                    path.iter()
                        .try_fold(&body.schema.0, |schema, field| {
                            schema.get("properties")?.get(field)
                        })
                        .is_some()
                })
            }
            (Ok(InputSelector::BodyField(path)), ReplacementValue::JsonString(_)) => {
                operation.request_body.as_ref().is_some_and(|body| {
                    path.iter()
                        .try_fold(&body.schema.0, |schema, field| {
                            schema.get("properties")?.get(field)
                        })
                        .is_some_and(|schema| {
                            schema.get("type").is_none_or(|kind| {
                                kind.as_str() == Some("string")
                                    || kind.as_array().is_some_and(|types| {
                                        types.iter().any(|kind| kind.as_str() == Some("string"))
                                    })
                            })
                        })
                })
            }
            _ => false,
        }
    }

    fn contract_fingerprint(&self) -> Option<[u8; 32]> {
        self.contract().map(|contract| contract.fingerprint)
    }

    fn request_template(&self, step: &Step) -> Option<RequestTemplate> {
        let operation = self.contract()?.operation(&step.operation_key)?;
        template::request_template(operation, step, self.client.base_url())
    }

}

pub(crate) fn baseline_value(
    contract: &ResolvedContract,
    step: &Step,
    selector: &str,
) -> Option<ReplacementValue> {
    // Header and path values: the literal `bind`, when there is one.
    if let Ok(target @ (InputSelector::HeaderParam(_) | InputSelector::PathParam(_))) =
        parse_input_selector(selector)
    {
        let literal = step.bindings.iter().find_map(|binding| {
            let same = match (&binding.selector, &target) {
                (InputSelector::HeaderParam(bound), InputSelector::HeaderParam(name)) => {
                    bound.eq_ignore_ascii_case(name)
                }
                (InputSelector::PathParam(bound), InputSelector::PathParam(name)) => bound == name,
                _ => false,
            };
            match (&binding.value, same) {
                (BindingValue::Literal(Value::String(text)), true) => Some(text.clone()),
                (BindingValue::Literal(other), true) => Some(other.to_string()),
                _ => None,
            }
        })?;
        return Some(match target {
            InputSelector::HeaderParam(_) => ReplacementValue::HeaderAscii(literal),
            _ => ReplacementValue::JsonString(literal),
        });
    }
    let path = selector.strip_prefix("body.")?;
    let operation = contract.operation(&step.operation_key)?;
    let body =
        build_baseline_input_with_example(operation, &step.bindings, step.body_example.as_ref())
            .ok()?;
    let value = crate::formats::json::get_object_path(&body, path)?;
    let text = value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_owned);
    Some(ReplacementValue::JsonString(text))
}

