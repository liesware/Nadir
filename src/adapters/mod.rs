//! Protocol adapters: `http` (application layer) and `http_raw` (the HTTP
//! layer, byte for byte).
pub mod cli;
pub mod http;
pub mod http_raw;

use serde_json::Value;

use crate::execution::runner::{RunError, StepOutcome};
use crate::experiment::types::{Binding, JsonExample, ReplacementValue, Step};
use crate::inspect::{RequestInspector, RequestTemplate};
use crate::observation::StepObservation;

pub(crate) struct StepInput<'a> {
    pub bindings: &'a [Binding],
    pub authorization: Option<Value>,
    pub replacements: &'a [(String, ReplacementValue)],
    pub json_example: Option<&'a JsonExample>,
    /// Set for steps with `debug`.
    pub inspection: Option<Inspection<'a>>,
    /// Break the request (`http_raw`) or a working-directory file (`cli`).
    pub breakage: Option<Breakage>,
}

/// What a broken-input case breaks in the step it targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Breakage {
    /// `use: malform`: the HTTP request.
    Request(http_raw::Malformation),
    /// `use: envfile`: the named case of the step's `.env`-style file.
    EnvFile(String),
}

/// The `debug` hook for one request and the values to redact in it.
pub(crate) struct Inspection<'a> {
    pub inspector: &'a mut (dyn RequestInspector + 'static),
    pub secrets: Vec<String>,
}

pub(crate) struct AdapterExecution {
    pub result: Result<StepObservation, StepOutcome>,
    pub requests_sent: u32,
    pub request_diagnostic: Option<crate::execution::diagnostic::RequestDiagnostic>,
}

impl AdapterExecution {
    pub(crate) fn input_error(error: String) -> Self {
        Self {
            result: Err(StepOutcome::InputError { error }),
            requests_sent: 0,
            request_diagnostic: None,
        }
    }
}

pub(crate) trait StepProtocol {
    fn execute(&mut self, step: &Step, input: StepInput<'_>) -> Result<AdapterExecution, RunError>;

    fn accepts_authorization(&self, value: &str) -> bool;
    fn baseline_value(&self, step: &Step, selector: &str) -> Option<ReplacementValue>;
    fn accepts_variant(&self, step: &Step, selector: &str, value: &ReplacementValue) -> bool;
    fn contract_fingerprint(&self) -> Option<[u8; 32]>;

    /// The request template shown by `print`.
    fn request_template(&self, _step: &Step) -> Option<RequestTemplate> {
        None
    }
}
