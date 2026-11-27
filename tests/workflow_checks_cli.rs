use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{json, Value};

struct Workspace(PathBuf);

impl Workspace {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("nadir-plan1-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn prepare(&self, service: &Service, max_cases: u8, steps: Value, paths: Value) -> PathBuf {
        self.write("openapi.yaml", &json!({"openapi":"3.1.0", "paths": paths}));
        self.write(
            "experiment.yaml",
            &json!({
                "version": 1,
                "experiment": {
                    "name": "plan1-local", "max_cases": max_cases,
                    "adapters": {"http": {
                        "base_url": service.origin, "openapi": "openapi.yaml", "timeout_ms": 150
                    }},
                    "steps": steps
                }
            }),
        )
    }

    fn write(&self, name: &str, document: &Value) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, serde_json::to_vec(document).unwrap()).unwrap();
        path
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Request {
    path: String,
    body: Value,
}

struct Reply {
    status: u16,
    body: Vec<u8>,
    headers: Vec<(&'static str, String)>,
    delay_before_headers: Duration,
    delay_during_body: Duration,
}

impl Reply {
    fn empty(status: u16) -> Self {
        Self {
            status,
            body: Vec::new(),
            headers: vec![("Content-Type", "application/json".into())],
            delay_before_headers: Duration::ZERO,
            delay_during_body: Duration::ZERO,
        }
    }

    fn json(status: u16, body: Value) -> Self {
        Self {
            body: serde_json::to_vec(&body).unwrap(),
            ..Self::empty(status)
        }
    }

    fn send(&self, mut stream: TcpStream) -> std::io::Result<()> {
        thread::sleep(self.delay_before_headers);
        write!(
            stream,
            "HTTP/1.0 {} Fixture\r\nContent-Length: {}\r\n",
            self.status,
            self.body.len()
        )?;
        for (name, value) in &self.headers {
            write!(stream, "{name}: {value}\r\n")?;
        }
        stream.write_all(b"\r\n")?;
        thread::sleep(self.delay_during_body);
        stream.write_all(&self.body)
    }
}

fn read_request(stream: &mut TcpStream) -> Request {
    let mut reader = BufReader::new(stream);
    let mut first_line = String::new();
    reader.read_line(&mut first_line).unwrap();
    let path = first_line.split_whitespace().nth(1).unwrap().to_string();
    let mut content_length = 0;
    loop {
        let mut line = String::new();
        assert_ne!(reader.read_line(&mut line).unwrap(), 0);
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap();
            }
        }
    }
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body).unwrap();
    Request {
        path,
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
    }
}

struct Service {
    origin: String,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Service {
    fn start(mut respond: impl FnMut(Request) -> Option<Reply> + Send + 'static) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let signal = stop.clone();
        let worker = thread::spawn(move || {
            while !signal.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_secs(2)))
                            .unwrap();
                        let request = read_request(&mut stream);
                        if let Some(reply) = respond(request) {
                            // A client may have already closed the socket after its deadline.
                            let _ = reply.send(stream);
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("local fixture could not accept a request: {error}"),
                }
            }
        });
        Self {
            origin,
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let result = self.worker.take().unwrap().join();
        if !thread::panicking() {
            result.unwrap();
        }
    }
}

fn call(command: &str, experiment: &Path, record: Option<&Path>) -> Output {
    let mut cli = Command::new(env!("CARGO_BIN_EXE_nadir"));
    cli.arg(command).arg(experiment);
    if let Some(record) = record {
        cli.arg("--case").arg(record);
    }
    cli.output().unwrap()
}

fn stderr(output: Output, exit: i32) -> String {
    let text = String::from_utf8(output.stderr).unwrap();
    assert_eq!(output.status.code(), Some(exit), "{text}");
    text
}

fn record_path(text: &str, prefix: &str) -> PathBuf {
    text.lines()
        .find_map(|line| line.strip_prefix(prefix))
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("missing {prefix}: {text}"))
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn operation(method: &str, name: &str, responses: Value) -> Value {
    json!({(method): {
        "operationId": name,
        "requestBody": {"content": {"application/json": {
            "schema": {"type": "object"}, "example": {"title": "baseline"}
        }}},
        "responses": responses
    }})
}

#[test]
fn expected_status_is_shared_by_run_replay_and_case_evidence() {
    let workspace = Workspace::new();
    let fixed = Arc::new(AtomicBool::new(false));
    let response_fixed = fixed.clone();
    let service = Service::start(move |request| {
        let status = if request.body["title"] == "example" && response_fixed.load(Ordering::Relaxed)
        {
            201
        } else {
            200
        };
        Some(Reply::json(status, json!({"note":"preview"})))
    });
    let experiment = workspace.prepare(&service, 2, json!([{
        "id":"preview", "adapter":"http", "operation_id":"preview",
        "cases":{"examples":[{"id":"created","field":"title","value":"example","expect_status":201}]}
    }]), json!({"/preview":operation("post", "preview", json!({"200":{"description":"Preview"}, "201":{"description":"Created"}}))}));
    let text = stderr(call("run", &experiment, None), 1);
    assert!(text.contains("example:created"), "{text}");
    let path = record_path(&text, "case record: ");
    let mut case = read_json(&path);
    let check = &case["step_results"][0]["oracle_verdicts"][0];
    assert_eq!(check["oracle_id"], "example:created");
    assert_eq!(check["expected"], "201");
    assert_eq!(check["observed"], "200");
    assert!(stderr(call("replay", &experiment, Some(&path)), 1).contains("example:created"));

    case["regression_plan"]
        .as_object_mut()
        .unwrap()
        .remove("example_step_id");
    let legacy = workspace.write("legacy-example.json", &case);
    assert!(stderr(call("replay", &experiment, Some(&legacy)), 1).contains("example:created"));
    fixed.store(true, Ordering::Relaxed);
    stderr(call("replay", &experiment, Some(&path)), 0);
    let text = stderr(call("run", &experiment, None), 0);
    let record = read_json(&record_path(&text, "Run record: "));
    let check = &record["cases"][0]["step_results"][0]["oracle_verdicts"][0];
    assert_eq!(check["expected"], "201");
    assert_eq!(check["observed"], "201");

    case["step_results"][0]["transport_reason"] = json!("connection_failed");
    for old_phase in ["beforeheaders", "duringbodyread"] {
        case["step_results"][0]["transport_phase"] = json!(old_phase);
        let legacy = workspace.write("legacy-example.json", &case);
        stderr(call("replay", &experiment, Some(&legacy)), 0);
    }
}

#[test]
fn replay_uses_the_step_and_rejects_ambiguous_legacy_example_ids() {
    let workspace = Workspace::new();
    let (requests, seen) = std::sync::mpsc::channel();
    let service = Service::start(move |request| {
        requests
            .send((request.path, request.body["title"].clone()))
            .unwrap();
        Some(Reply::empty(200))
    });
    let steps: Vec<_> = ["first", "second"]
        .into_iter()
        .map(|id| {
            json!({
                "id":id, "adapter":"http", "operation_id":id,
                "cases":{"examples":[{"id":"same","field":"title","value":id,"expect_status":201}]}
            })
        })
        .collect();
    let experiment = workspace.prepare(
        &service,
        3,
        json!(steps),
        json!({
            "/first":operation("post", "first", json!({"200":{"description":"OK"}})),
            "/second":operation("post", "second", json!({"200":{"description":"OK"}}))
        }),
    );
    let run = stderr(call("run", &experiment, None), 1);
    let records = record_path(&run, "Run record: ");
    for (case_number, step, other) in [("0001", "first", "second"), ("0002", "second", "first")] {
        while seen.try_recv().is_ok() {}
        let path = records
            .parent()
            .unwrap()
            .join("cases")
            .join(format!("{case_number}.json"));
        let mut case = read_json(&path);
        assert_eq!(case["regression_plan"]["example_step_id"], step);
        stderr(call("replay", &experiment, Some(&path)), 1);
        let requests: Vec<_> = seen.try_iter().collect();
        assert_eq!(requests.len(), 2);
        assert!(requests.contains(&(format!("/{step}"), json!(step))));
        assert!(requests.contains(&(format!("/{other}"), json!("baseline"))));

        case["regression_plan"]
            .as_object_mut()
            .unwrap()
            .remove("example_step_id");
        let legacy = workspace.write("old.json", &case);
        let text = stderr(call("replay", &experiment, Some(&legacy)), 2);
        assert!(text.contains("ambiguous"), "{text}");
        assert!(seen.try_recv().is_err());
    }
}

#[test]
fn validates_schema_preparation_and_exact_response_contracts() {
    let workspace = Workspace::new();
    let response = Arc::new(std::sync::Mutex::new((200, Value::Null)));
    let reply = response.clone();
    let service = Service::start(move |_| {
        let (status, body) = reply.lock().unwrap().clone();
        let reply = if body.is_null() {
            Reply::empty(status)
        } else {
            Reply::json(status, body)
        };
        Some(reply)
    });
    let valid_schema =
        json!({"type":"object","required":["label"],"properties":{"label":{"type":"string"}}});
    let schemas = |schema: Value| {
        json!({
            "200":{"description":"Summary","content":{"application/json":{"schema":schema}}},
            "204":{"description":"No content"}
        })
    };
    let prepare = |schema| {
        workspace.prepare(&service, 1, json!([{
        "id":"read", "adapter":"http", "operation_id":"read", "oracles":[{"id":"schema","use":"openapi_schema"}]
    }]), json!({"/read":operation("get", "read", schemas(schema))}))
    };
    let bad = prepare(json!({"type":"not-a-type"}));
    let text = stderr(call("validate", &bad, None), 2);
    assert!(
        text.contains("read") && text.contains("200") && text.contains("invalid response schema"),
        "{text}"
    );

    let unsupported = workspace.prepare(&service, 1, json!([{
        "id":"read", "adapter":"http", "operation_id":"read", "oracles":[{"id":"schema","use":"openapi_schema"}]
    }]), json!({"/read":operation("get", "read", json!({
        "200":{"description":"Unsupported","content":{"text/plain":{"schema":{"type":"string"}}}}
    }))}));
    let text = stderr(call("validate", &unsupported, None), 2);
    assert!(text.contains("application/json"), "{text}");
    let experiment = prepare(valid_schema);
    let text = stderr(call("run", &experiment, None), 1);
    assert!(text.contains("requires a JSON body"), "{text}");
    *response.lock().unwrap() = (204, Value::Null);
    stderr(call("run", &experiment, None), 0);
    *response.lock().unwrap() = (203, json!({"label":"OK"}));
    assert!(stderr(call("run", &experiment, None), 1).contains("status is not declared"));
    *response.lock().unwrap() = (200, json!({"label":["fixture-instance-value"]}));
    let text = stderr(call("run", &experiment, None), 1);
    let case = record_path(&text, "case record: ");
    let json_record = fs::read_to_string(&case).unwrap();
    assert!(!text.contains("fixture-instance-value"));
    assert!(!json_record.contains("fixture-instance-value"));
    let check = &serde_json::from_str::<Value>(&json_record).unwrap()["step_results"][0]
        ["oracle_verdicts"][0];
    assert!(check["schema_location"]
        .as_str()
        .unwrap()
        .contains("/properties/label/type"));
    stderr(call("replay", &experiment, Some(&case)), 1);
    *response.lock().unwrap() = (200, json!({"label":"OK"}));
    stderr(call("replay", &experiment, Some(&case)), 0);

    *response.lock().unwrap() = (200, json!({"label":"a".repeat(256 * 1024)}));
    let text = stderr(call("run", &experiment, None), 2);
    let record = read_json(&record_path(&text, "Run record: "));
    let step = &record["baseline_case"]["step_results"][0];
    assert_eq!(step["outcome"], "response_limit");
    assert_eq!(step["oracle_verdicts"][0]["verdict"], "inconclusive");
}

#[test]
fn replays_array_and_header_bindings_and_redacts_derived_values() {
    let workspace = Workspace::new();
    let (forwarded, captured) = std::sync::mpsc::channel();
    let mut next = 0;
    let service = Service::start(move |request| match request.path.as_str() {
        "/source" => {
            next += 1;
            let mut reply = Reply::json(
                200,
                json!({
                    "items":[{"id": next, "note": null}],
                    "private": "fixture-source-secret"
                }),
            );
            reply.headers.push(("X-Preview", format!("preview-{next}")));
            Some(reply)
        }
        "/intermediate" => {
            forwarded.send(request.body).unwrap();
            Some(Reply::json(200, json!({"label": "fixture-derived-label"})))
        }
        "/summary" => {
            assert_eq!(request.body["incoming"], "fixture-derived-label");
            Some(Reply::json(
                200,
                json!({"label": "fixture-transitive-label"}),
            ))
        }
        _ => unreachable!(),
    });
    let mut steps = json!([
        {"id":"source", "adapter":"http", "operation_id":"source", "outputs":{
            "next_id":{"from":"response.body.items[0].id"},
            "note":{"from":"response.body.items[0].note"},
            "header":{"from":"response.header.x-preview", "when_status":200},
            "private":{"from":"response.body.private", "secret":true}
        }},
        {"id":"intermediate", "adapter":"http", "operation_id":"intermediate",
            "bind":{"body.id":"${steps.source.outputs.next_id}", "body.note":"${steps.source.outputs.note}",
                "body.preview":"${steps.source.outputs.header}", "body.source":"example-${steps.source.outputs.private}"},
            "outputs":{"label":{"from":"response.body.label"}},
            "oracles":[{"id":"expected-label","use":"assert","select":"response.body.label","expect":{"equals":"expected"}}]
        },
        {"id":"summary", "adapter":"http", "operation_id":"summary",
            "bind":{"body.incoming":"${steps.intermediate.outputs.label}"},
            "oracles":[{"id":"summary-label","use":"assert","select":"response.body.label","expect":{"equals":"expected"}},
                {"id":"summary-status","use":"assert","select":"response.status","expect":{"equals":201}}]
        }
    ]);
    let paths = json!({
        "/source": operation("get", "source", json!({"200":{"description":"OK"}})),
        "/intermediate": operation("post", "intermediate", json!({"200":{"description":"OK"}})),
        "/summary": operation("post", "summary", json!({"200":{"description":"OK"}}))
    });
    let experiment = workspace.prepare(&service, 1, steps.clone(), paths.clone());
    let text = stderr(call("run", &experiment, None), 1);
    let path = record_path(&text, "case record: ");
    let persisted = fs::read_to_string(&path).unwrap();
    let replay = stderr(call("replay", &experiment, Some(&path)), 1);
    for surface in [&text, &persisted, &replay] {
        for value in [
            "fixture-source-secret",
            "fixture-derived-label",
            "fixture-transitive-label",
        ] {
            assert!(
                !surface.contains(value),
                "private fixture value appeared in diagnostics"
            );
        }
        assert!(surface.contains("[redacted]"));
    }
    assert!(replay.contains("summary-status") && replay.contains("observed 200"));
    for index in 1..=2 {
        let body = captured.try_recv().unwrap();
        assert_eq!(body["id"], index);
        assert_eq!(body["preview"], format!("preview-{index}"));
        assert_eq!(body["note"], Value::Null);
        assert!(body.get("note").is_some());
    }

    for (output, declaration) in [
        ("next_id", json!({"from":"response.body.items[9].id"})),
        (
            "header",
            json!({"from":"response.header.x-preview", "when_status":201}),
        ),
    ] {
        let original = steps[0]["outputs"][output].clone();
        steps[0]["outputs"][output] = declaration;
        let experiment = workspace.prepare(&service, 1, steps.clone(), paths.clone());
        let text = stderr(call("run", &experiment, None), 2);
        let run = read_json(&record_path(&text, "Run record: "));
        let step = &run["baseline_case"]["step_results"][1];
        assert_eq!(step["step_id"], "intermediate");
        assert!(step["outcome"].as_str().unwrap().starts_with("skipped"));
        assert!(captured.try_recv().is_err());
        steps[0]["outputs"][output] = original;
    }
}

#[test]
fn reports_transport_reason_and_phase_in_terminal_and_json() {
    let workspace = Workspace::new();
    for (scenario, exit, reason, phase) in [
        ("header-timeout", 2, Some("timeout"), Some("before_headers")),
        ("body-timeout", 2, Some("timeout"), Some("during_body_read")),
        (
            "closed",
            2,
            Some("connection_closed"),
            Some("before_headers"),
        ),
        ("normal", 0, None, None),
    ] {
        let service = Service::start(move |_| {
            let delay = Duration::from_millis(400);
            let mut reply = Reply::json(200, json!({"label":"preview"}));
            match scenario {
                "header-timeout" => reply.delay_before_headers = delay,
                "body-timeout" => reply.delay_during_body = delay,
                "closed" => return None,
                _ => {}
            }
            Some(reply)
        });
        let experiment = workspace.prepare(
            &service,
            1,
            json!([{
                "id":"read", "adapter":"http", "operation_id":"read", "oracles":[{"use":"openapi"}]
            }]),
            json!({"/read":operation("post", "read", json!({"200":{"description":"OK"}}))}),
        );
        let text = stderr(call("run", &experiment, None), exit);
        let run = read_json(&record_path(&text, "Run record: "));
        let step = &run["baseline_case"]["step_results"][0];
        assert_eq!(
            step["transport_reason"].as_str(),
            reason,
            "{scenario}: {text}"
        );
        assert_eq!(
            step["transport_phase"].as_str(),
            phase,
            "{scenario}: {text}"
        );
        if let (Some(reason), Some(phase)) = (reason, phase) {
            assert!(text.contains(&format!("({reason}, {phase})")), "{text}");
            assert_eq!(step["oracle_verdicts"][0]["verdict"], "inconclusive");
        }
    }
}

#[test]
fn reports_confirmed_finding_alongside_inconclusive_transport() {
    let workspace = Workspace::new();
    let service = Service::start(|request| {
        (request.path == "/check").then(|| Reply::json(200, json!({"label":"preview"})))
    });
    let experiment = workspace.prepare(&service, 1, json!([
        {"id":"check","adapter":"http","operation_id":"check",
            "oracles":[{"id":"ordinary-status","use":"assert","select":"response.status","expect":{"equals":201}}]},
        {"id":"later","adapter":"http","operation_id":"later","oracles":[{"use":"openapi"}]}
    ]), json!({
        "/check":operation("post", "check", json!({"200":{"description":"OK"}})),
        "/later":operation("post", "later", json!({"200":{"description":"OK"}}))
    }));
    let text = stderr(call("run", &experiment, None), 1);
    assert!(
        text.contains("ordinary-status") && text.contains("connection_closed"),
        "{text}"
    );
    let run = read_json(&record_path(&text, "Run record: "));
    assert_eq!(run["findings"][0]["oracle_id"], "ordinary-status");
    assert_eq!(
        run["baseline_case"]["step_results"][1]["oracle_verdicts"][0]["verdict"],
        "inconclusive"
    );
    assert_eq!(
        run["baseline_case"]["step_results"][1]["transport_reason"],
        "connection_closed"
    );
}
