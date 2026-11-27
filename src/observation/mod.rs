//! Select fields from a completed HTTP observation.

use crate::adapters::http::transport::HttpObservation;
use crate::formats::json;

#[derive(Debug)]
pub enum StepObservation {
    Http(HttpObservation),
    #[cfg(test)]
    InMemory(serde_json::Value),
}

impl StepObservation {
    pub fn as_http(&self) -> Option<&HttpObservation> {
        match self {
            Self::Http(response) => Some(response),
            #[cfg(test)]
            Self::InMemory(_) => None,
        }
    }

    pub fn http_status(&self) -> Option<u16> {
        self.as_http().map(|response| response.status)
    }

    pub(crate) fn is_truncated(&self) -> bool {
        self.as_http().is_some_and(|response| response.truncated)
    }

    pub(crate) fn select(&self, selector: &ObservationSelector) -> SelectedValue {
        match self {
            Self::Http(response) => select_response(selector, response),
            #[cfg(test)]
            Self::InMemory(value) => match selector {
                ObservationSelector::Body(path) => select_json_path(value, path),
                _ => SelectedValue::Unavailable(ObservationError::NoResponse),
            },
        }
    }
}
use crate::experiment::selectors::{JsonPathSegment, ObservationSelector};
use serde_json::Value;

/// A selector can fail to observe a response, or observe a response without the field.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SelectedValue {
    Unavailable(ObservationError),
    Missing,
    Present(Value),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ObservationError {
    NoResponse,
    ResponseLimit,
    InvalidJson,
}

impl ObservationError {
    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::NoResponse => "response unavailable",
            Self::ResponseLimit => "response limit exceeded",
            Self::InvalidJson => "response body is not valid JSON",
        }
    }
}

pub(crate) fn select_response(
    selector: &ObservationSelector,
    response: &HttpObservation,
) -> SelectedValue {
    if response.truncated {
        return SelectedValue::Unavailable(ObservationError::ResponseLimit);
    }
    let value = match selector {
        ObservationSelector::Status => Some(Value::from(response.status)),
        ObservationSelector::Header(name) => response
            .headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| Value::String(value.clone())),
        ObservationSelector::Body(_) if response.body.is_empty() => None,
        ObservationSelector::Body(path) => {
            let Ok(root) = json::decode(&response.body) else {
                return SelectedValue::Unavailable(ObservationError::InvalidJson);
            };
            return select_json_path(&root, path);
        }
        ObservationSelector::Step { .. } => {
            return SelectedValue::Unavailable(ObservationError::NoResponse)
        }
    };
    value.map_or(SelectedValue::Missing, SelectedValue::Present)
}

fn select_json_path(root: &Value, path: &[JsonPathSegment]) -> SelectedValue {
    json::get(root, path).cloned().map_or(SelectedValue::Missing, SelectedValue::Present)
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_obs(body: &[u8], status: u16) -> HttpObservation {
        HttpObservation {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: body.to_vec(),
            truncated: false,
        }
    }

    #[test]
    fn test_select_array_element() {
        let obs = make_obs(
            br#"{"items": [{"id": "a"}, {"id": "b"}]}"#,
            200,
        );
        let selector = ObservationSelector::Body(vec![
            JsonPathSegment::Key("items".into()),
            JsonPathSegment::Index(0),
            JsonPathSegment::Key("id".into()),
        ]);
        let result = select_response(&selector, &obs);
        assert_eq!(result, SelectedValue::Present(serde_json::json!("a")));
    }

    #[test]
    fn test_select_array_second_element() {
        let obs = make_obs(
            br#"{"items": [{"id": "a"}, {"id": "b"}]}"#,
            200,
        );
        let selector = ObservationSelector::Body(vec![
            JsonPathSegment::Key("items".into()),
            JsonPathSegment::Index(1),
            JsonPathSegment::Key("id".into()),
        ]);
        let result = select_response(&selector, &obs);
        assert_eq!(result, SelectedValue::Present(serde_json::json!("b")));
    }

    #[test]
    fn test_select_array_out_of_range() {
        let obs = make_obs(
            br#"{"items": [{"id": "a"}]}"#,
            200,
        );
        let selector = ObservationSelector::Body(vec![
            JsonPathSegment::Key("items".into()),
            JsonPathSegment::Index(99),
            JsonPathSegment::Key("id".into()),
        ]);
        let result = select_response(&selector, &obs);
        assert_eq!(result, SelectedValue::Missing);
    }

    #[test]
    fn test_select_array_index_on_non_array() {
        let obs = make_obs(br#"{"items": {"id": "a"}}"#, 200);
        let selector = ObservationSelector::Body(vec![
            JsonPathSegment::Key("items".into()),
            JsonPathSegment::Index(0),
            JsonPathSegment::Key("id".into()),
        ]);
        let result = select_response(&selector, &obs);
        // items is an object, not an array → Missing
        assert_eq!(result, SelectedValue::Missing);
    }

    #[test]
    fn test_select_missing_key() {
        let obs = make_obs(br#"{"items": []}"#, 200);
        let selector = ObservationSelector::Body(vec![
            JsonPathSegment::Key("nonexistent".into()),
        ]);
        let result = select_response(&selector, &obs);
        assert_eq!(result, SelectedValue::Missing);
    }

    #[test]
    fn test_select_explicit_null() {
        let obs = make_obs(br#"{"value": null}"#, 200);
        let selector = ObservationSelector::Body(vec![
            JsonPathSegment::Key("value".into()),
        ]);
        let result = select_response(&selector, &obs);
        // null is a present JSON value
        assert_eq!(result, SelectedValue::Present(serde_json::Value::Null));
    }

    #[test]
    fn test_select_object_value_is_present() {
        let obs = make_obs(br#"{"items": [{"id": 42}]}"#, 200);
        let selector = ObservationSelector::Body(vec![
            JsonPathSegment::Key("items".into()),
            JsonPathSegment::Index(0),
            JsonPathSegment::Key("id".into()),
        ]);
        let result = select_response(&selector, &obs);
        assert_eq!(result, SelectedValue::Present(serde_json::json!(42)));
    }
}
