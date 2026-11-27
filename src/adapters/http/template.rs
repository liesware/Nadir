//! The request template shown by `print`: the step's request with each
//! mutated part replaced by `{{mode}}` and each reference by `{{name}}`.

use serde_json::Value;
use url::Url;

use super::contract::ResolvedOperation;
use super::request::{build_step_request_with_example, ResolvedStepInput};
use crate::experiment::selectors::{InputSelector, Reference};
use crate::experiment::types::{
    Binding, BindingValue, CompositePart, ExampleAction, JsonExample, JsonMutatorMode,
    MutatorDecl, ReplacementValue, Step,
};
use crate::inspect::{percent_decode, RequestTemplate, RequestView};

/// Build the template of `step`, or `None` when its request cannot be built.
pub(crate) fn request_template(
    operation: &ResolvedOperation,
    step: &Step,
    base_url: &Url,
) -> Option<RequestTemplate> {
    let mut bindings: Vec<Binding> = step
        .bindings
        .iter()
        .map(|binding| Binding {
            selector: binding.selector.clone(),
            value: BindingValue::Literal(placeholder_value(&binding.value)),
        })
        .collect();
    if let Some(auth) = &step.auth {
        bindings.push(Binding {
            selector: InputSelector::HeaderParam("Authorization".into()),
            value: BindingValue::Literal(Value::String(format!("Bearer {}", placeholder(&auth.bearer)))),
        });
    }

    let mut replacements = Vec::new();
    for mutator in &step.mutators {
        match mutator {
            // The template shows the well-formed request; notes list the catalog.
            MutatorDecl::Malform { .. } => {}
            MutatorDecl::Json { fields, mode, .. } => {
                for field in fields.iter().flatten() {
                    replacements.push((format!("body.{field}"), ReplacementValue::JsonString(marker(mode_name(mode)))));
                }
            }
            MutatorDecl::Http { fields, mode, .. } => {
                for field in fields {
                    let selector = field.selector();
                    // Several mutators on one part show as `{{raw|absent}}`.
                    let shown = match replacements.iter().position(|(existing, _)| *existing == selector) {
                        Some(index) => {
                            let (_, previous) = replacements.remove(index);
                            let previous = match previous {
                                ReplacementValue::HeaderAscii(text) | ReplacementValue::JsonString(text) => text,
                                _ => String::new(),
                            };
                            format!("{}|{}}}}}", previous.trim_end_matches('}'), mode_name(mode))
                        }
                        None => marker(mode_name(mode)),
                    };
                    let value = if field.is_header() {
                        ReplacementValue::HeaderAscii(shown)
                    } else {
                        ReplacementValue::JsonString(shown)
                    };
                    replacements.push((selector, value));
                }
            }
        }
    }
    for corpus in &step.corpora {
        let kind = if corpus.generator.is_some() { "file_fuzz" } else { "file" };
        replacements.push((format!("body.{}", corpus.field), ReplacementValue::JsonString(marker(kind))));
    }

    let request = build_step_request_with_example(
        operation,
        &bindings,
        &replacements,
        base_url,
        step.body_example.as_ref(),
        step.format,
        None,
    )
    .ok()?;
    let mut view = request_view(&request);
    view.target = percent_decode(&view.target);
    if let Some(body) = view.body.take() {
        view.body = Some(rename_keys(step, body));
    }
    Some(RequestTemplate { step_id: step.id.clone(), request: view, notes: notes(step) })
}

/// The view of a request exactly as the transport sends it: the headers
/// Nadir sets, then the content type the transport adds.
pub(crate) fn request_view(request: &ResolvedStepInput) -> RequestView {
    let url = &request.url;
    let mut target = url.path().to_owned();
    if let Some(query) = url.query() {
        target.push('?');
        target.push_str(query);
    }
    let host = match (url.host_str(), url.port()) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) => host.to_owned(),
        _ => String::new(),
    };
    let mut headers = request.headers.clone();
    if let Some(content_type) = request.content_type.as_ref().filter(|_| !super::transport::has_content_type(&headers)) {
        headers.push(("Content-Type".into(), content_type.clone()));
    }
    RequestView { method: request.method.clone(), target, host, headers, body: request.body.clone(), raw: None }
}

/// Rename each key a key mutator changes to `{{mode}}`. Several keys can
/// share the marker, so unique names are replaced after serializing.
fn rename_keys(step: &Step, body: Vec<u8>) -> Vec<u8> {
    if step.key_mutators.is_empty() {
        return body;
    }
    let Ok(mut value) = serde_json::from_slice::<Value>(&body) else {
        return body;
    };
    let mut names = Vec::new();
    for (index, key) in step.key_mutators.iter().enumerate() {
        let unique = format!("__nadir_key_{index}__");
        if crate::formats::json::rename(&mut value, &key.key, &unique).is_ok() {
            names.push((unique, marker(key.alphabet.name())));
        }
    }
    let Ok(mut text) = serde_json::to_string(&value) else {
        return body;
    };
    for (unique, shown) in names {
        text = text.replace(&format!("\"{unique}\""), &format!("\"{shown}\""));
    }
    text.into_bytes()
}

/// One line per mutator, corpus and example: what each placeholder becomes.
fn notes(step: &Step) -> Vec<String> {
    let mut notes = Vec::new();
    for mutator in &step.mutators {
        match mutator {
            MutatorDecl::Malform { malformations } => {
                let all = crate::adapters::http_raw::Malformation::ALL.len();
                notes.push(if malformations.len() == all {
                    format!("malform: catalog, {all} cases")
                } else {
                    let names: Vec<&str> = malformations.iter().map(|malformation| malformation.name()).collect();
                    format!("malform: catalog, {} cases: {}", names.len(), names.join(", "))
                });
            }
            MutatorDecl::Json { fields, mode, values, .. } => {
                let fields = fields.as_deref().unwrap_or_default();
                if fields.is_empty() {
                    continue;
                }
                let mut note = format!("{}: {}", fields.join(", "), mode_name(mode));
                if let JsonMutatorMode::Text(generator) = mode {
                    note.push_str(&format!(
                        ", length 0-{} or {}-{}",
                        generator.below.saturating_sub(1),
                        generator.above + 1,
                        generator.max,
                    ));
                }
                if !values.is_empty() {
                    note.push_str(&format!(", plus {} custom values", values.len()));
                }
                notes.push(note);
            }
            MutatorDecl::Http { fields, mode, values, .. } => {
                let fields: Vec<String> = fields.iter().map(|field| field.selector()).collect();
                let mut note = format!("{}: {}", fields.join(", "), mode_name(mode));
                if let JsonMutatorMode::Text(generator) = mode {
                    note.push_str(&format!(
                        ", length 0-{} or {}-{}",
                        generator.below.saturating_sub(1),
                        generator.above + 1,
                        generator.max,
                    ));
                }
                if !values.is_empty() {
                    let label = if *mode == JsonMutatorMode::Values { "" } else { "plus " };
                    note.push_str(&format!(", {label}{} custom values", values.len()));
                }
                notes.push(note);
            }
        }
    }
    for corpus in &step.corpora {
        let kind = match &corpus.generator {
            Some((generator, _)) => format!("file_fuzz {}", generator.alphabet.name()),
            None => "file".into(),
        };
        notes.push(format!("{}: {kind}, {} lines", corpus.field, corpus.repetitions));
    }
    if !step.key_mutators.is_empty() {
        let keys: Vec<&str> = step.key_mutators.iter().map(|key| key.key.as_str()).collect();
        let mut note = format!(
            "keys {}: {}, one rename per case",
            keys.join(", "),
            step.key_mutators[0].alphabet.name()
        );
        if let Some(status) = step.key_mutators[0].expect_status {
            note.push_str(&format!(", expect {status}"));
        }
        notes.push(note);
    }
    for example in &step.examples {
        let what = match &example.action {
            ExampleAction::Field { path, .. } => format!("field {path}"),
            ExampleAction::Json(JsonExample::Rename { key, destination }) => {
                format!("rename {key} to {destination}")
            }
            ExampleAction::Json(JsonExample::Syntax { fixture }) => {
                format!("syntax {}", serde_json::to_value(fixture).ok()
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .unwrap_or_default())
            }
        };
        notes.push(format!("example {}: {what}, expect {}", example.id, example.expect_status));
    }
    notes
}

fn mode_name(mode: &JsonMutatorMode) -> &'static str {
    match mode {
        JsonMutatorMode::EdgeCases => "edge_cases",
        JsonMutatorMode::Unconstrained => "unconstrained",
        JsonMutatorMode::Schema => "schema",
        JsonMutatorMode::Alphanumeric => "alphanum",
        JsonMutatorMode::Text(generator) => generator.alphabet.name(),
        JsonMutatorMode::Absent => "absent",
        JsonMutatorMode::Values => "values",
    }
}

fn marker(name: &str) -> String {
    format!("{{{{{name}}}}}")
}

/// `{{env.NAME}}`, `{{case.id}}`, … Defaults are not shown: they may be secrets.
fn placeholder(reference: &Reference) -> String {
    match reference {
        Reference::Env { name, .. } => marker(&format!("env.{name}")),
        Reference::CaseId => marker("case.id"),
        Reference::StepOutput { step_id, output_name } => {
            marker(&format!("steps.{step_id}.outputs.{output_name}"))
        }
        Reference::TargetPort => marker("target.port"),
    }
}

fn placeholder_value(value: &BindingValue) -> Value {
    match value {
        BindingValue::Literal(value) => value.clone(),
        BindingValue::Reference(reference) => Value::String(placeholder(reference)),
        BindingValue::Composite(parts) => Value::String(
            parts
                .iter()
                .map(|part| match part {
                    CompositePart::Literal(text) => text.clone(),
                    CompositePart::Reference(reference) => placeholder(reference),
                })
                .collect(),
        ),
    }
}
