//! `protocol: http_raw`: the HTTP layer itself. Requests are built like
//! `http` ones, written byte for byte to a loopback socket, and optionally
//! broken by a `malform` mutator to test the server's HTTP parser.

pub mod malform;
pub mod transport;

pub use malform::{Malformation, RawRequest};

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use url::Url;

use crate::adapters::http::contract::ResolvedContract;
use crate::adapters::http::execution::bindings_with_auth;
use crate::adapters::http::request::{self, ResolvedStepInput};
use crate::adapters::http::template;
use crate::adapters::{AdapterExecution, StepInput, StepProtocol};
use crate::execution::runner::{PrepareError, RunError, StepOutcome};
use crate::experiment::types::{Experiment, ReplacementValue, Step};
use crate::inspect::{Control, RequestTemplate, RequestView, ResponseView};
use crate::observation::StepObservation;

/// Response bytes passed to `debug`; the display shows one row anyway.
const INSPECTED_BODY_BYTES: usize = 4096;

pub(crate) struct HttpRawStepProtocol {
    client: transport::RawClient,
    base_url: Url,
    contracts: Arc<HashMap<String, ResolvedContract>>,
    contract_path: String,
}

impl HttpRawStepProtocol {
    /// Uses the `protocols.http` configuration: same service, same contract.
    pub(crate) fn prepare(
        experiment: &Experiment,
        contracts: Arc<HashMap<String, ResolvedContract>>,
    ) -> Result<Self, PrepareError> {
        let config = experiment.protocols.http.as_ref().ok_or(PrepareError::NoHttpProtocol)?;
        let base_url = Url::parse(&config.base_url).map_err(|_| PrepareError::InvalidBaseUrl)?;
        let client = transport::RawClient::new(&base_url, Duration::from_millis(config.timeout_ms))
            .map_err(PrepareError::HttpClient)?;
        Ok(Self { client, base_url, contracts, contract_path: config.openapi_path.clone() })
    }

    fn contract(&self) -> Option<&ResolvedContract> {
        self.contracts.get(&self.contract_path)
    }
}

/// The well-formed request on the wire: `Host` first, the request's
/// headers, `Content-Length` for a body, and `Connection: close` so the
/// server ends the exchange.
pub(crate) fn raw_request(request: &ResolvedStepInput, host: &str) -> RawRequest {
    let view = template::request_view(request);
    let mut headers = vec![("Host".to_string(), host.to_string())];
    headers.extend(view.headers.into_iter().filter(|(name, _)| {
        !["host", "content-length", "connection"].contains(&name.to_ascii_lowercase().as_str())
    }));
    let body = request.body.clone().unwrap_or_default();
    if request.body.is_some() {
        headers.push(("Content-Length".into(), body.len().to_string()));
    }
    headers.push(("Connection".into(), "close".into()));
    RawRequest { method: view.method, target: view.target, version: "HTTP/1.1".into(), headers, body }
}

impl StepProtocol for HttpRawStepProtocol {
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
            operation, &bindings, input.replacements, &self.base_url,
            step.body_example.as_ref(), step.format, input.json_example,
        ) {
            Ok(request) => request,
            Err(error) => return Ok(AdapterExecution::input_error(error.to_string())),
        };
        let raw = raw_request(&request, self.client.host());
        let bytes = match input.malformation {
            Some(malformation) => malformation.apply(&raw),
            None => raw.render(),
        };
        let mut inspection = input.inspection;
        if let Some(inspection) = inspection.as_mut() {
            let view = RequestView {
                method: raw.method.clone(),
                target: raw.target.clone(),
                host: self.client.host().to_owned(),
                headers: raw.headers.clone(),
                body: Some(raw.body.clone()),
                raw: Some(bytes.clone()),
            };
            if inspection.inspector.before_send(step, &view, &inspection.secrets) == Control::Stop {
                return Err(RunError::StoppedByUser);
            }
        }
        let before = self.client.requests_sent();
        let result = self.client.send(&bytes).map(StepObservation::Http).map_err(|error| {
            StepOutcome::TransportError {
                error: error.to_string(),
                transport_reason: Some(error.reason()),
                transport_phase: error.phase(),
            }
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
            request_diagnostic: result.is_err().then(|| request::request_diagnostic(operation, None)),
            result,
            requests_sent: self.client.requests_sent().wrapping_sub(before),
        })
    }

    fn accepts_authorization(&self, value: &str) -> bool {
        crate::adapters::http::execution::HttpStepProtocol::bearer_header(value).is_some()
    }

    fn baseline_value(&self, _: &Step, _: &str) -> Option<ReplacementValue> {
        None
    }

    fn accepts_variant(&self, _: &Step, _: &str, _: &ReplacementValue) -> bool {
        false
    }

    fn contract_fingerprint(&self) -> Option<[u8; 32]> {
        self.contract().map(|contract| contract.fingerprint)
    }

    fn request_template(&self, step: &Step) -> Option<RequestTemplate> {
        let operation = self.contract()?.operation(&step.operation_key)?;
        let mut template = template::request_template(operation, step, &self.base_url)?;
        template.request.headers.retain(|(name, _)| !name.eq_ignore_ascii_case("connection"));
        template.request.headers.push(("Connection".into(), "close".into()));
        Some(template)
    }
}
