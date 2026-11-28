//! Input and observation selectors, and `${…}` reference parsing.
//!
//! # Design notes
//!
//! **Chosen:** hand-written parser for `${…}` references. A pest grammar
//! would add a build dependency and make compilation slower. The grammar
//! is simple enough: `${<kind>.<path>}` with no nesting beyond the
//! step-output variant. Edge cases (empty `${}`, `${`, nested braces)
//! are handled explicitly in the parser.
//!
//! **Alternative considered:** a pest PEG grammar. Rejected because the
//! grammar is trivial and a hand-written parser avoids a proc-macro
//! build dependency.

use std::fmt;

use serde_json::Value;

// ── Selectors ──────────────────────────────────────────────────────

/// Identifies a mutable input location.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum InputSelector {
    /// `body.user.name` → `BodyField(["user", "name"])`
    BodyField(Vec<String>),
    /// `path.id`
    PathParam(String),
    /// `header.Accept`
    HeaderParam(String),
    /// A `protocol: cli` step's environment variable whose `env` value holds
    /// `${…}` references, resolved like any binding before each run.
    EnvVar(String),
}

/// Identifies a location in a step's observation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ObservationSelector {
    /// `response.status` → `Status`
    Status,
    /// `response.header.Content-Type` → `Header("Content-Type")`
    Header(String),
    /// `response.body.items[0].id` → `Body([Key("items"), Index(0), Key("id")])`
    Body(Vec<JsonPathSegment>),
    Step { step_id: String, selector: Box<ObservationSelector> },
    /// `process.exit_code` → process exit code (CLI steps).
    ProcessExitCode,
    /// `process.stdout` → process stdout (CLI steps).
    ProcessStdout,
    /// `process.stderr` → process stderr (CLI steps).
    ProcessStderr,
    /// `process.timed_out` → whether the process timed out (CLI steps).
    ProcessTimedOut,
    /// `process.ready` → whether a `service` reached its ready URL.
    ProcessReady,
    /// `process.stopped_code` → the exit code after Nadir stopped a ready service.
    ProcessStoppedCode,
}

/// A single segment in a JSON observation path.
///
/// Object keys (e.g. `access_token`) and array indices (e.g. `[0]`) are
/// represented as distinct typed variants.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum JsonPathSegment {
    /// An object property key (dot-separated, e.g. `items`).
    Key(String),
    /// An array index (bracket notation, e.g. `[0]`).
    Index(usize),
}

impl JsonPathSegment {
    /// Return the key name if this is a `Key` segment.
    pub fn as_key(&self) -> Option<&str> {
        match self {
            Self::Key(k) => Some(k),
            Self::Index(_) => None,
        }
    }
}

impl From<&str> for JsonPathSegment {
    fn from(s: &str) -> Self {
        Self::Key(s.to_string())
    }
}

// ── References ─────────────────────────────────────────────────────

/// A parsed `${…}` reference.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Reference {
    /// `${env.NAME}`, or `${env.NAME:-default}` to use `default` when the
    /// variable is unset or empty.
    Env {
        name: String,
        default: Option<String>,
    },
    /// `${case.id}`
    CaseId,
    /// `${steps.<id>.outputs.<name>}`
    StepOutput {
        step_id: String,
        output_name: String,
    },
    /// `${target.port}`
    TargetPort,
}

/// Error returned when reference parsing fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReferenceParseError {
    UnclosedBrace,
    EmptyReference,
    UnknownPrefix(String),
    Malformed(String),
}

impl fmt::Display for ReferenceParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnclosedBrace => write!(f, "unclosed ${{…}} reference"),
            Self::EmptyReference => write!(f, "empty ${{}} reference"),
            Self::UnknownPrefix(prefix) => write!(f, "unknown reference prefix: {}", prefix),
            Self::Malformed(detail) => write!(f, "malformed reference: {}", detail),
        }
    }
}

// ── Parsing ────────────────────────────────────────────────────────

/// Parse an input selector string.
///
/// Supported formats:
/// - `body.<path>`  → `InputSelector::BodyField`
/// - `path.<name>`  → `InputSelector::PathParam`
/// - `header.<name>` → `InputSelector::HeaderParam`
pub fn parse_input_selector(raw: &str) -> Result<InputSelector, String> {
    if let Some(rest) = raw.strip_prefix("body.") {
        if rest.is_empty() {
            return Err("empty body path".into());
        }
        let path: Vec<String> = rest.split('.').map(|s| s.to_string()).collect();
        Ok(InputSelector::BodyField(path))
    } else if let Some(rest) = raw.strip_prefix("path.") {
        if rest.is_empty() {
            return Err("empty path param name".into());
        }
        if rest.contains('.') {
            return Err(format!("path param must be a single segment: '{}'", rest));
        }
        Ok(InputSelector::PathParam(rest.to_string()))
    } else if let Some(rest) = raw.strip_prefix("header.") {
        if rest.is_empty() {
            return Err("empty header name".into());
        }
        Ok(InputSelector::HeaderParam(rest.to_string()))
    } else {
        Err(format!("unknown input selector: '{}'", raw))
    }
}

/// Parse an observation selector string.
///
/// The leading `response.` prefix is stripped first (or `steps.X.response.`
/// is handled by the caller before calling this).
///
/// Supported formats:
/// - `status`        → `ObservationSelector::Status`
/// - `header.<name>` → `ObservationSelector::Header`
/// - `body.<path>`   → `ObservationSelector::Body`
/// - `process.exit_code` → `ObservationSelector::ProcessExitCode`
/// - `process.stdout`    → `ObservationSelector::ProcessStdout`
/// - `process.stderr`    → `ObservationSelector::ProcessStderr`
/// - `process.timed_out` → `ObservationSelector::ProcessTimedOut`
pub fn parse_observation_selector(raw: &str) -> Result<ObservationSelector, String> {
    if raw == "status" {
        Ok(ObservationSelector::Status)
    } else if let Some(rest) = raw.strip_prefix("header.") {
        if rest.is_empty() {
            return Err("empty header name".into());
        }
        Ok(ObservationSelector::Header(rest.to_string()))
    } else if let Some(rest) = raw.strip_prefix("body.") {
        if rest.is_empty() {
            return Err("empty body path".into());
        }
        let path = parse_json_path(rest)?;
        Ok(ObservationSelector::Body(path))
    } else if raw == "process.exit_code" {
        Ok(ObservationSelector::ProcessExitCode)
    } else if raw == "process.stdout" {
        Ok(ObservationSelector::ProcessStdout)
    } else if raw == "process.stderr" {
        Ok(ObservationSelector::ProcessStderr)
    } else if raw == "process.timed_out" {
        Ok(ObservationSelector::ProcessTimedOut)
    } else if raw == "process.ready" {
        Ok(ObservationSelector::ProcessReady)
    } else if raw == "process.stopped_code" {
        Ok(ObservationSelector::ProcessStoppedCode)
    } else {
        Err(format!("unknown observation selector: '{}'", raw))
    }
}

/// Parse a JSON body path with optional array index segments.
///
/// E.g. `items[0].id` → `[Key("items"), Index(0), Key("id")]`
///
/// Each dot-separated segment may optionally end with `[N]` where N is
/// a non-negative decimal integer. A segment without brackets is an
/// object key. An empty bracket (`[]`) or malformed index is an error.
fn parse_json_path(raw: &str) -> Result<Vec<JsonPathSegment>, String> {
    let mut segments = Vec::new();
    for part in raw.split('.') {
        if part.is_empty() {
            return Err("empty path segment".into());
        }
        // Check for [N] suffix
        if let Some(bracket_pos) = part.find('[') {
            let key = &part[..bracket_pos];
            let rest = &part[bracket_pos..];
            if !rest.ends_with(']') {
                return Err(format!("unclosed '[' in path segment '{}'", part));
            }
            let index_str = &rest[1..rest.len() - 1];
            if index_str.is_empty() {
                return Err(format!("empty index in path segment '{}'", part));
            }
            let index: usize = index_str
                .parse()
                .map_err(|_| format!("invalid array index '{}' in '{}'", index_str, part))?;
            if !key.is_empty() {
                segments.push(JsonPathSegment::Key(key.to_string()));
            }
            segments.push(JsonPathSegment::Index(index));
        } else {
            segments.push(JsonPathSegment::Key(part.to_string()));
        }
    }
    Ok(segments)
}

/// Parse a full observation selector, which may include a
/// `steps.<id>.response.` prefix referencing another step.
///
/// Returns `(Some(step_id), selector)` when a step reference is present,
/// or `(None, selector)` when it's the current step.
pub fn parse_full_observation_selector(
    raw: &str,
) -> Result<(Option<String>, ObservationSelector), String> {
    if let Some(rest) = raw.strip_prefix("steps.") {
        // steps.<id>.response.<selector>
        let mut parts = rest.splitn(3, '.');
        let step_id = parts
            .next()
            .ok_or_else(|| format!("invalid step reference: '{}'", raw))?
            .to_string();
        let keyword = parts
            .next()
            .ok_or_else(|| format!("invalid step reference: '{}'", raw))?;
        if keyword != "response" {
            return Err(format!(
                "expected 'response' after step id, got '{}' in '{}'",
                keyword, raw
            ));
        }
        let selector_raw = parts
            .next()
            .ok_or_else(|| format!("missing selector after steps.{}.response", step_id))?;
        let selector = parse_observation_selector(selector_raw)?;
        Ok((Some(step_id), selector))
    } else if let Some(rest) = raw.strip_prefix("response.") {
        let selector = parse_observation_selector(rest)?;
        Ok((None, selector))
    } else {
        let selector = parse_observation_selector(raw)?;
        Ok((None, selector))
    }
}

// ── Reference parsing ──────────────────────────────────────────────

/// Parse a single `${…}` reference from a string that is entirely a reference.
///
/// Returns `None` if the string is not a reference.
pub fn parse_reference(raw: &str) -> Option<Result<Reference, ReferenceParseError>> {
    if !raw.starts_with("${") || !raw.ends_with('}') {
        return None;
    }
    let inner = &raw[2..raw.len() - 1];
    // `${a}-${b}` is a composite, not one reference.
    if inner.contains('}') {
        return None;
    }
    Some(parse_reference_inner(inner))
}

/// Parse the content inside `${…}`.
fn parse_reference_inner(inner: &str) -> Result<Reference, ReferenceParseError> {
    if inner.is_empty() {
        return Err(ReferenceParseError::EmptyReference);
    }

    // Check for ${ prefix (escaped dollar)
    if inner.starts_with('$') {
        return Err(ReferenceParseError::Malformed(
            "nested '${{' is not supported".into(),
        ));
    }

    // ${env.NAME} or ${env.NAME:-default}
    if let Some(rest) = inner.strip_prefix("env.") {
        let (name, default) = match rest.split_once(":-") {
            Some((name, default)) => (name, Some(default.to_string())),
            None => (rest, None),
        };
        if name.is_empty() {
            return Err(ReferenceParseError::Malformed("empty env name".into()));
        }
        if !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return Err(ReferenceParseError::Malformed(format!(
                "invalid env name '{name}' (use letters, digits and '_'; defaults use ':-')"
            )));
        }
        return Ok(Reference::Env {
            name: name.to_string(),
            default,
        });
    }

    // ${case.id}
    if inner == "case.id" {
        return Ok(Reference::CaseId);
    }

    // ${steps.<id>.outputs.<name>}
    if let Some(rest) = inner.strip_prefix("steps.") {
        let mut parts = rest.splitn(3, '.');
        let step_id = parts
            .next()
            .ok_or(ReferenceParseError::Malformed("missing step id".into()))?;
        let outputs_kw = parts
            .next()
            .ok_or(ReferenceParseError::Malformed("missing 'outputs'".into()))?;
        if outputs_kw != "outputs" {
            return Err(ReferenceParseError::Malformed(format!(
                "expected 'outputs', got '{}'",
                outputs_kw
            )));
        }
        let output_name = parts
            .next()
            .ok_or(ReferenceParseError::Malformed("missing output name".into()))?;
        if step_id.is_empty() || output_name.is_empty() {
            return Err(ReferenceParseError::Malformed(
                "empty step id or output name".into(),
            ));
        }
        return Ok(Reference::StepOutput {
            step_id: step_id.to_string(),
            output_name: output_name.to_string(),
        });
    }

    // ${target.port}
    if inner == "target.port" {
        return Ok(Reference::TargetPort);
    }

    Err(ReferenceParseError::UnknownPrefix(
        inner
            .split('.')
            .next()
            .unwrap_or(inner)
            .to_string(),
    ))
}

/// Parse a value that may be a reference, a composite (literal + refs),
/// or a plain literal.
///
/// This is the entry point for resolving bind values. A string that is
/// *exactly* a `${…}` reference is parsed as a `Reference`. A string
/// that *contains* `${…}` references is parsed as a `Composite`.
/// Everything else is a `Literal`.
pub fn parse_binding_value(raw: &Value) -> BindingParseResult {
    match raw {
        Value::String(s) => {
            // Check if it's an exact reference
            if let Some(ref_result) = parse_reference(s) {
                match ref_result {
                    Ok(r) => BindingParseResult::Reference(r),
                    Err(e) => BindingParseResult::Error(e),
                }
            } else if s.contains("${") {
                // Composite: parse out the parts
                parse_composite(s)
            } else {
                BindingParseResult::Literal(s.clone())
            }
        }
        other => BindingParseResult::LiteralValue(other.clone()),
    }
}

/// Result of parsing a binding value string.
#[derive(Debug, Clone)]
pub enum BindingParseResult {
    Literal(String),
    LiteralValue(Value),
    Reference(Reference),
    Composite(Vec<CompositePart>),
    Error(ReferenceParseError),
}

impl BindingParseResult {
    pub(crate) fn into_binding_value(
        self,
    ) -> Result<crate::experiment::types::BindingValue, ReferenceParseError> {
        use crate::experiment::types::BindingValue;
        match self {
            Self::Literal(text) => Ok(BindingValue::Literal(Value::String(text))),
            Self::LiteralValue(value) => Ok(BindingValue::Literal(value)),
            Self::Reference(reference) => Ok(BindingValue::Reference(reference)),
            Self::Composite(parts) => Ok(BindingValue::Composite(parts)),
            Self::Error(error) => Err(error),
        }
    }
}

use crate::experiment::types::CompositePart;

/// Parse a string containing embedded `${…}` references into a composite.
fn parse_composite(s: &str) -> BindingParseResult {
    let mut parts: Vec<CompositePart> = Vec::new();
    let mut remaining = s;

    while let Some(dollar_pos) = remaining.find("${") {
        // Literal before the ${
        if dollar_pos > 0 {
            parts.push(CompositePart::Literal(remaining[..dollar_pos].to_string()));
        }

        // Find the closing brace
        let after_dollar = &remaining[dollar_pos..];
        if let Some(close_pos) = after_dollar.find('}') {
            let inner = &after_dollar[2..close_pos]; // skip ${
            match parse_reference_inner(inner) {
                Ok(r) => parts.push(CompositePart::Reference(r)),
                Err(e) => return BindingParseResult::Error(e),
            }
            remaining = &after_dollar[close_pos + 1..]; // skip }
        } else {
            return BindingParseResult::Error(ReferenceParseError::UnclosedBrace);
        }
    }

    // Remaining literal at the end
    if !remaining.is_empty() {
        parts.push(CompositePart::Literal(remaining.to_string()));
    }

    BindingParseResult::Composite(parts)
}

// ── Display ────────────────────────────────────────────────────────

impl fmt::Display for InputSelector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BodyField(path) => write!(f, "body.{}", path.join(".")),
            Self::PathParam(name) => write!(f, "path.{}", name),
            Self::HeaderParam(name) => write!(f, "header.{}", name),
            Self::EnvVar(name) => write!(f, "env.{}", name),
        }
    }
}

impl fmt::Display for ObservationSelector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Status => write!(f, "response.status"),
            Self::Header(name) => write!(f, "response.header.{}", name),
            Self::Body(path) => {
                write!(f, "response.body")?;
                for seg in path {
                    match seg {
                        JsonPathSegment::Key(k) => write!(f, ".{}", k)?,
                        JsonPathSegment::Index(idx) => write!(f, "[{}]", idx)?,
                    }
                }
                Ok(())
            }
            Self::Step { step_id, selector } => write!(f, "steps.{step_id}.{selector}"),
            Self::ProcessExitCode => write!(f, "process.exit_code"),
            Self::ProcessStdout => write!(f, "process.stdout"),
            Self::ProcessStderr => write!(f, "process.stderr"),
            Self::ProcessTimedOut => write!(f, "process.timed_out"),
            Self::ProcessReady => write!(f, "process.ready"),
            Self::ProcessStoppedCode => write!(f, "process.stopped_code"),
        }
    }
}

impl fmt::Display for Reference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Env {
                name,
                default: None,
            } => write!(f, "${{env.{}}}", name),
            Self::Env {
                name,
                default: Some(default),
            } => write!(f, "${{env.{}:-{}}}", name, default),
            Self::CaseId => write!(f, "${{case.id}}"),
            Self::StepOutput {
                step_id,
                output_name,
            } => write!(f, "${{steps.{}.outputs.{}}}", step_id, output_name),
            Self::TargetPort => write!(f, "${{target.port}}"),
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_input_selector_body() {
        let s = parse_input_selector("body.user.name").unwrap();
        assert_eq!(
            s,
            InputSelector::BodyField(vec!["user".into(), "name".into()])
        );
    }

    #[test]
    fn test_parse_input_selector_path() {
        let s = parse_input_selector("path.id").unwrap();
        assert_eq!(s, InputSelector::PathParam("id".into()));
    }

    #[test]
    fn test_parse_input_selector_header() {
        let s = parse_input_selector("header.Accept").unwrap();
        assert_eq!(s, InputSelector::HeaderParam("Accept".into()));
    }

    #[test]
    fn test_parse_input_selector_unknown() {
        assert!(parse_input_selector("unknown.field").is_err());
    }

    #[test]
    fn test_parse_observation_selector_status() {
        let s = parse_observation_selector("status").unwrap();
        assert_eq!(s, ObservationSelector::Status);
    }

    #[test]
    fn test_parse_observation_selector_body() {
        let s = parse_observation_selector("body.access_token").unwrap();
        assert_eq!(s, ObservationSelector::Body(vec!["access_token".into()]));
    }

    #[test]
    fn test_parse_observation_selector_array_index() {
        let s = parse_observation_selector("body.items[0].id").unwrap();
        let expected = ObservationSelector::Body(vec![
            JsonPathSegment::Key("items".into()),
            JsonPathSegment::Index(0),
            JsonPathSegment::Key("id".into()),
        ]);
        assert_eq!(s, expected);
    }

    #[test]
    fn test_parse_observation_selector_bare_index() {
        // [0] without a parent key (e.g., top-level array)
        let s = parse_observation_selector("body.[0]").unwrap();
        let expected = ObservationSelector::Body(vec![JsonPathSegment::Index(0)]);
        assert_eq!(s, expected);
    }

    #[test]
    fn test_parse_observation_selector_first_element_key() {
        let s = parse_observation_selector("body.[0].name").unwrap();
        let expected = ObservationSelector::Body(vec![
            JsonPathSegment::Index(0),
            JsonPathSegment::Key("name".into()),
        ]);
        assert_eq!(s, expected);
    }

    #[test]
    fn test_parse_observation_selector_invalid_index() {
        assert!(parse_observation_selector("body.items[abc].id").is_err());
    }

    #[test]
    fn test_parse_observation_selector_empty_index() {
        assert!(parse_observation_selector("body.items[].id").is_err());
    }

    #[test]
    fn test_parse_observation_selector_unclosed_bracket() {
        assert!(parse_observation_selector("body.items[0.id").is_err());
    }

    #[test]
    fn test_parse_observation_selector_display_roundtrip() {
        let s = parse_observation_selector("body.items[0].id").unwrap();
        let displayed = format!("{s}");
        assert_eq!(displayed, "response.body.items[0].id");
    }

    #[test]
    fn test_parse_reference_env() {
        let r = parse_reference("${env.FOO}").unwrap().unwrap();
        assert_eq!(r, Reference::Env { name: "FOO".into(), default: None });
    }

    #[test]
    fn test_parse_reference_env_default() {
        let r = parse_reference("${env.FOO:-bar baz}").unwrap().unwrap();
        assert_eq!(
            r,
            Reference::Env { name: "FOO".into(), default: Some("bar baz".into()) }
        );
        assert_eq!(r.to_string(), "${env.FOO:-bar baz}");
        let r = parse_reference("${env.FOO:-}").unwrap().unwrap();
        assert_eq!(r, Reference::Env { name: "FOO".into(), default: Some(String::new()) });
        assert!(parse_reference("${env.FOO-bar}").unwrap().is_err());
    }

    #[test]
    fn test_two_references_are_a_composite() {
        assert!(parse_reference("${env.A:-x}-${case.id}").is_none());
        let BindingParseResult::Composite(parts) =
            parse_binding_value(&Value::String("${env.A:-x}-${case.id}".into()))
        else {
            panic!("expected composite");
        };
        assert_eq!(parts.len(), 3);
        assert!(matches!(
            &parts[0],
            CompositePart::Reference(Reference::Env { name, default: Some(d) }) if name == "A" && d == "x"
        ));
        assert!(matches!(&parts[2], CompositePart::Reference(Reference::CaseId)));
    }

    #[test]
    fn test_parse_reference_case_id() {
        let r = parse_reference("${case.id}").unwrap().unwrap();
        assert_eq!(r, Reference::CaseId);
    }

    #[test]
    fn test_parse_reference_step_output() {
        let r = parse_reference("${steps.login.outputs.token}")
            .unwrap()
            .unwrap();
        assert_eq!(
            r,
            Reference::StepOutput {
                step_id: "login".into(),
                output_name: "token".into()
            }
        );
    }

    #[test]
    fn test_parse_reference_target_port() {
        let r = parse_reference("${target.port}").unwrap().unwrap();
        assert_eq!(r, Reference::TargetPort);
    }

    #[test]
    fn test_parse_reference_empty() {
        let r = parse_reference("${}").unwrap();
        assert!(matches!(r, Err(ReferenceParseError::EmptyReference)));
    }

    #[test]
    fn test_parse_reference_unknown_prefix() {
        let r = parse_reference("${unknown.thing}").unwrap();
        assert!(matches!(r, Err(ReferenceParseError::UnknownPrefix(_))));
    }

    #[test]
    fn test_parse_reference_not_a_ref() {
        assert!(parse_reference("plain string").is_none());
    }

    #[test]
    fn test_parse_binding_literal() {
        let v = Value::String("fixture-user".into());
        let r = parse_binding_value(&v);
        assert!(matches!(r, BindingParseResult::Literal(s) if s == "fixture-user"));
    }

    #[test]
    fn test_parse_binding_ref() {
        let v = Value::String("${env.FOO}".into());
        let r = parse_binding_value(&v);
        assert!(matches!(r, BindingParseResult::Reference(Reference::Env { name: n, .. }) if n == "FOO"));
    }

    #[test]
    fn test_parse_binding_composite() {
        let v = Value::String("prefix-${env.FOO}-suffix".into());
        let r = parse_binding_value(&v);
        match r {
            BindingParseResult::Composite(parts) => {
                assert_eq!(parts.len(), 3);
                assert!(matches!(&parts[0], CompositePart::Literal(s) if s == "prefix-"));
                assert!(matches!(&parts[1], CompositePart::Reference(Reference::Env { name: n, .. }) if n == "FOO"));
                assert!(matches!(&parts[2], CompositePart::Literal(s) if s == "-suffix"));
            }
            other => panic!("expected composite, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_binding_number() {
        let v = Value::Number(serde_json::Number::from(42));
        let r = parse_binding_value(&v);
        assert!(matches!(r, BindingParseResult::LiteralValue(_)));
    }

    #[test]
    fn test_parse_full_observation_with_step() {
        let (step_id, sel) =
            parse_full_observation_selector("steps.post_record.response.status").unwrap();
        assert_eq!(step_id, Some("post_record".into()));
        assert_eq!(sel, ObservationSelector::Status);
    }

    #[test]
    fn test_parse_full_observation_current_step() {
        let (step_id, sel) = parse_full_observation_selector("response.status").unwrap();
        assert_eq!(step_id, None);
        assert_eq!(sel, ObservationSelector::Status);
    }
}
