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

/// A test server's `http://127.0.0.1:PORT`.
trait HasOrigin {
    fn origin(&self) -> &str;
}

impl HasOrigin for Service {
    fn origin(&self) -> &str {
        &self.origin
    }
}

impl Workspace {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("nadir-plan1-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn prepare(&self, service: &impl HasOrigin, max_cases: u8, steps: Value, paths: Value) -> PathBuf {
        self.write("openapi.yaml", &json!({"openapi":"3.1.0", "paths": paths}));
        self.write(
            "experiment.yaml",
            &json!({
                "version": 1,
                "experiment": {
                    "name": "plan1-local", "max_cases": max_cases,
                    "adapters": {"http": {
                        "base_url": service.origin(), "openapi": "openapi.yaml", "timeout_ms": 150
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
    raw_body: Vec<u8>,
    content_type: Option<String>,
    content_type_count: usize,
    headers: Vec<(String, String)>,
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
    let mut content_type = None;
    let mut content_type_count = 0;
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        assert_ne!(reader.read_line(&mut line).unwrap(), 0);
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.to_owned(), value.trim().to_owned()));
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap();
            }
            if name.eq_ignore_ascii_case("content-type") {
                content_type = Some(value.trim().to_owned());
                content_type_count += 1;
            }
        }
    }
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body).unwrap();
    Request {
        path,
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
        raw_body: body,
        content_type,
        content_type_count,
        headers,
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
fn timeout_diagnostics_are_redacted_linked_and_deduplicated() {
    let workspace = Workspace::new();
    let service = Service::start(|request| {
        let mut reply = Reply::json(200, json!({"ok": true}));
        if request.body["title"] != "baseline" {
            reply.delay_before_headers = Duration::from_millis(375);
        }
        Some(reply)
    });
    let mut path_item = operation("post", "preview", json!({"200":{"description":"OK"}}));
    path_item["post"]["parameters"] = json!([{
        "in":"path", "name":"kid", "required":true, "schema":{"type":"string"}
    }]);
    let experiment = workspace.prepare(&service, 3, json!([{
        "id":"preview", "protocol":"http", "operation_id":"preview",
        "bind":{"path.kid":"dynamic-route-value","header.X-API-Key":"sensitive-header-value"},
        "cases":{"mutators":[{"use":"json","mode":"alphanum","fields":["title"],
                              "values":["private message one","private message two"]}]}
    }]), json!({"/preview/{kid}":path_item}));

    let output = stderr(call("run", &experiment, None), 2);
    assert!(output.contains("Findings: 0") && output.contains("Transport errors: 2"), "{output}");
    assert!(output.contains("transport: transport_error · preview"), "{output}");
    assert!(output.contains("── transport errors") && !output.contains("── crashes"), "{output}");
    let run = read_json(&record_path(&output, "Run record: "));
    assert_eq!(run["counters"]["transport_errors"], 2);
    let directory = workspace.0.join("artifacts/transport-errors");
    assert!(!workspace.0.join("artifacts/crashes").exists());
    let yaml: Vec<_> = fs::read_dir(&directory).unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "yaml"))
        .collect();
    assert_eq!(yaml.len(), 1);
    let text = fs::read_to_string(&yaml[0]).unwrap();
    for private_value in ["dynamic-route-value", "sensitive-header-value", "private message", "X-API-Key"] {
        assert!(!text.contains(private_value), "{text}");
    }
    let evidence: Value = serde_yaml::from_str(&text).unwrap();
    assert_eq!(evidence["artifact_version"], 1);
    assert_eq!(evidence["step_id"], "preview");
    assert_eq!(evidence["request"]["path_template"], "/preview/{kid}");
    assert_eq!(evidence["request"]["content_type"], "application/json");
    assert_eq!(evidence["request"]["body_shape"], "object");
    assert_eq!(evidence["outcome"]["kind"], "transport_error");
    assert_eq!(evidence["outcome"]["reason"], "timeout");
    assert_eq!(evidence["outcome"]["phase"], "before_headers");
    let json_path = directory.join(evidence["case_record"].as_str().unwrap());
    assert_eq!(json_path, yaml[0].with_extension("json"));
    let record = read_json(&json_path);
    assert_eq!(record["case_id"], evidence["case_id"]);
    assert!(record.get("regression_plan").is_none());
}

#[test]
fn oracle_failure_alone_keeps_the_existing_json_artifact() {
    let workspace = Workspace::new();
    let service = Service::start(|_| Some(Reply::json(200, json!({"ok": true}))));
    let experiment = workspace.prepare(&service, 1, json!([{
        "id":"preview", "protocol":"http", "operation_id":"preview",
        "oracles":[{"id":"created","use":"assert","select":"response.status","expect":{"equals":201}}]
    }]), json!({"/preview":operation("post", "preview", json!({"200":{"description":"OK"}}))}));
    let output = stderr(call("run", &experiment, None), 1);
    assert!(output.contains("Findings: 1"), "{output}");
    let files: Vec<_> = fs::read_dir(workspace.0.join("artifacts/crashes")).unwrap()
        .map(|entry| entry.unwrap().path()).collect();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].extension().unwrap(), "json");
}

#[test]
fn generated_json_keys_rename_one_property_and_replay_from_the_seed() {
    let workspace = Workspace::new();
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = requests.clone();
    let service = Service::start(move |request| {
        captured.lock().unwrap().push(request.body);
        Some(Reply::json(200, json!({"ok": true})))
    });
    let experiment = workspace.prepare(&service, 1, json!([{
        "id":"preview", "protocol":"http", "operation_id":"preview",
        "body_example":{"hash":{"alg":"SHA-256", "hex":"demo"}},
        "cases":{"mutators":[{"use":"json", "keys":["hash.alg","hash.hex"], "mode":"alpha", "expect_status":400}]}
    }]), json!({"/preview":operation("post", "preview", json!({"200":{"description":"OK"}}))}));
    let mut document = read_json(&experiment);
    document["experiment"]["exploration"] = json!({"strategy":"fuzz","duration_s":10,"max_execs":20,"seed":9});
    fs::write(&experiment, document.to_string()).unwrap();
    let output = stderr(call("run", &experiment, None), 1);
    assert!(output.contains("Enumeration cases: 0"), "{output}");
    assert!(output.contains("Cases: 21/21"), "{output}");
    let path = record_path(&output, "Run record: ");
    let run = read_json(&path);
    assert_eq!(run["fuzz"]["executed"], 20);
    assert_eq!(run["counters"]["findings_count"], 2);

    let original = requests.lock().unwrap().clone();
    assert_eq!(original[0], json!({"hash":{"alg":"SHA-256","hex":"demo"}}));
    for request in &original[1..] {
        let hash = request["hash"].as_object().unwrap();
        assert_eq!(hash.len(), 2);
        let (unchanged, changed) = if hash.contains_key("alg") {
            (("alg", "SHA-256"), "demo")
        } else {
            (("hex", "demo"), "SHA-256")
        };
        assert_eq!(hash[unchanged.0], unchanged.1);
        let (generated, value) = hash.iter().find(|(key, _)| *key != unchanged.0).unwrap();
        assert!((4..=24).contains(&generated.len()));
        assert!(generated.bytes().all(|byte| byte.is_ascii_alphabetic()));
        assert_eq!(value, changed);
    }
    let kept = path.parent().unwrap().join("cases/0001.json");
    let record = read_json(&kept);
    let iteration = record["regression_plan"]["fuzz"]["iteration"].as_u64().unwrap() as usize;
    stderr(call("replay", &experiment, Some(&kept)), 1);
    assert_eq!(requests.lock().unwrap().last(), Some(&original[iteration + 1]));
}

#[test]
fn raw_mode_generates_any_characters_for_values_and_keys_and_replays() {
    let workspace = Workspace::new();
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = requests.clone();
    let service = Service::start(move |request| {
        captured.lock().unwrap().push(request.body);
        Some(Reply::json(200, json!({"ok": true})))
    });
    let experiment = workspace.prepare(&service, 1, json!([{
        "id":"preview", "protocol":"http", "operation_id":"preview",
        "body_example":{"hash":{"alg":"SHA-256", "hex":"demo"}},
        "cases":{"mutators":[
            {"use":"json", "fields":["hash.hex"], "mode":"raw", "length":{"reference":4, "max":40}},
            {"use":"json", "keys":["hash.alg"], "mode":"raw", "expect_status":400}
        ]}
    }]), json!({"/preview":operation("post", "preview", json!({"200":{"description":"OK"}}))}));
    let mut document = read_json(&experiment);
    document["experiment"]["exploration"] = json!({"strategy":"fuzz","duration_s":10,"max_execs":60,"seed":3});
    fs::write(&experiment, document.to_string()).unwrap();
    stderr(call("validate", &experiment, None), 0);
    let output = stderr(call("run", &experiment, None), 1);
    let path = record_path(&output, "Run record: ");

    let original = requests.lock().unwrap().clone();
    let (mut values, mut keys, mut unusual) = (0, 0, 0);
    for request in &original[1..] {
        let hash = request["hash"].as_object().unwrap();
        let text = match hash.get("alg") {
            Some(_) => {
                values += 1;
                let hex = hash["hex"].as_str().unwrap();
                let length = hex.chars().count();
                assert!(length < 4 || (5..=40).contains(&length), "{length}");
                hex.to_owned()
            }
            None => {
                keys += 1;
                let (name, value) = hash.iter().find(|(key, _)| *key != "hex").unwrap();
                assert_eq!(value, "SHA-256");
                assert!((4..=24).contains(&name.chars().count()));
                name.clone()
            }
        };
        if text.chars().any(|character| !character.is_ascii_alphanumeric()) {
            unusual += 1;
        }
    }
    assert!(values > 0 && keys > 0, "values {values}, keys {keys}");
    assert!(unusual > values / 2, "{unusual} of {}", original.len() - 1);

    let kept = path.parent().unwrap().join("cases/0001.json");
    let iteration = read_json(&kept)["regression_plan"]["fuzz"]["iteration"].as_u64().unwrap() as usize;
    stderr(call("replay", &experiment, Some(&kept)), 1);
    assert_eq!(requests.lock().unwrap().last(), Some(&original[iteration + 1]));
}

#[test]
fn json_key_renames_and_syntax_examples_run_and_replay() {
    let workspace = Workspace::new();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = seen.clone();
    let service = Service::start(move |request| {
        assert_eq!(request.path, "/preview");
        assert_eq!(request.content_type.as_deref(), Some("application/json"));
        let status = match &request.body["message_hash"] {
            hash if hash.get("alg").is_some() && hash.get("hex").is_some() => 200,
            _ => 400,
        };
        captured.lock().unwrap().push(request.raw_body);
        Some(Reply::json(status, json!({"status": status})))
    });
    let experiment = workspace.prepare(&service, 7, json!([{
        "id":"preview", "protocol":"http", "operation_id":"preview",
        "body_example":{"message_hash":{"alg":"SHA-256", "hex":"demo"}, "unchanged":true},
        "cases":{
            "mutators":[{"use":"json", "keys":["message_hash.alg","message_hash.hex"],
                        "rename_to":["alias"], "expect_status":400}],
            "examples":[
                {"id":"before", "syntax":"leading_text", "expect_status":400},
                {"id":"inside", "syntax":"trailing_comma", "expect_status":400},
                {"id":"after", "syntax":"trailing_text", "expect_status":400},
                {"id":"incomplete", "syntax":"incomplete_object", "expect_status":400}
            ]
        }
    }]), json!({"/preview":operation("post", "preview", json!({"200":{"description":"OK"}, "400":{"description":"Rejected"}}))}));
    stderr(call("validate", &experiment, None), 0);
    let output = stderr(call("run", &experiment, None), 0);
    assert!(output.contains("Cases: 7/7") && output.contains("Findings: 0"), "{output}");
    let path = record_path(&output, "Run record: ");
    let run = read_json(&path);
    assert_eq!(run["baseline_case"]["step_results"][0]["status"], 200);
    for case in run["cases"].as_array().unwrap() {
        assert_eq!(case["artifact_version"], 4);
        assert_eq!(case["step_results"][0]["status"], 400);
        assert_eq!(case["step_results"][0]["oracle_verdicts"][0]["verdict"], "pass");
    }
    let original = seen.lock().unwrap().clone();
    for (index, expected) in [b"x{}".as_slice(), b"{\"sample\":1,}", b"{}x", b"{\"sample\":"].into_iter().enumerate() {
        assert_eq!(original[index + 1], expected);
    }
    assert_eq!(
        serde_json::from_slice::<Value>(&original[5]).unwrap(),
        json!({"message_hash":{"alias":"SHA-256","hex":"demo"},"unchanged":true}),
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&original[6]).unwrap(),
        json!({"message_hash":{"alg":"SHA-256","alias":"demo"},"unchanged":true}),
    );

    for index in [1, 5] {
        let saved = path.parent().unwrap().join(format!("cases/{index:04}.json"));
        let replay = stderr(call("replay", &experiment, Some(&saved)), 0);
        assert!(replay.contains("Replayed case:"), "{replay}");
        assert_eq!(seen.lock().unwrap().last(), Some(&original[index]));
    }

    let mut saved = read_json(&path.parent().unwrap().join("cases/0005.json"));
    saved["regression_plan"]["json_example"]["destination"] = json!("changed");
    let changed = workspace.write("changed-replay.json", &saved);
    let output = stderr(call("replay", &experiment, Some(&changed)), 2);
    assert!(output.contains("saved JSON example differs"), "{output}");
    assert_eq!(seen.lock().unwrap().len(), 9);
}

#[test]
fn json_examples_validate_key_targets_and_expected_statuses() {
    let workspace = Workspace::new();
    let service = Service::start(|_| panic!("validate must not send a request"));
    let path = workspace.prepare(&service, 2, json!([{
        "id":"preview", "protocol":"http", "operation_id":"preview",
        "body_example":{"hash":{"alg":"SHA-256","hex":"demo"}}
    }]), json!({"/preview":operation("post", "preview", json!({"200":{"description":"OK"}}))}));
    let original = read_json(&path);
    for (key, destination, binding, message) in [
        ("hash.unknown", "other", None, "source key is missing"),
        ("hash.alg", "hex", None, "destination key already exists"),
        ("hash.alg", "other", Some("body.hash.alg"), "overlaps bound path 'hash.alg'"),
        ("hash.alg", "other", Some("body.hash.other"), "overlaps bound path 'hash.other'"),
    ] {
        let mut document = original.clone();
        let step = &mut document["experiment"]["steps"][0];
        step["cases"] = json!({"mutators":[{"use":"json","keys":[key],"rename_to":[destination],"expect_status":400}]});
        if let Some(binding) = binding {
            step["bind"] = json!({(binding):"bound"});
        }
        let invalid = workspace.write("invalid.json", &document);
        let output = stderr(call("validate", &invalid, None), 2);
        assert!(output.contains(message), "{output}");
    }
    let mut document = original;
    document["experiment"]["steps"][0]["cases"] = json!({"examples":[
        {"id":"syntax","syntax":"trailing_comma","expect_status":200}
    ]});
    let invalid = workspace.write("invalid-syntax.json", &document);
    let output = stderr(call("validate", &invalid, None), 2);
    assert!(output.contains("4xx expect_status"), "{output}");
    document["experiment"]["steps"][0]["cases"] = json!({"examples":[
        {"id":"legacy-null","field":"hash.alg","value":null,"expect_status":200}
    ]});
    let legacy = workspace.write("legacy-null.json", &document);
    stderr(call("validate", &legacy, None), 0);
}

#[test]
fn corpus_cycles_replays_and_checks_budget_and_fingerprint() {
    use std::sync::Mutex;
    let workspace = Workspace::new();
    let received = Arc::new(Mutex::new(Vec::new()));
    let captured = received.clone();
    let service = Service::start(move |request| {
        captured.lock().unwrap().push(request.body["title"].as_str().unwrap().to_string());
        Some(Reply::json(200, json!({"ok": true})))
    });
    let corpus_path = workspace.0.join("lines.txt");
    let mut lines: Vec<String> = (1..=30).map(|index| format!("Line {index}")).collect();
    lines[1] = String::new();
    lines[2] = "  Line 3  ".into();
    let contents = lines.join("\r\n") + "\r\n";
    fs::write(&corpus_path, &contents).unwrap();
    let experiment = workspace.prepare(&service, 101, json!([{
        "id":"preview", "protocol":"http", "operation_id":"preview",
        "cases":{"mutators":[{"use":"file", "fields":["title"], "path":"lines.txt", "repetitions":100}]}
    }]), json!({"/preview":operation("post", "preview", json!({"200":{"description":"OK"}}))}));
    stderr(call("validate", &experiment, None), 0);
    let output = stderr(call("run", &experiment, None), 0);
    assert!(output.contains("Cases: 101/101"), "{output}");
    let path = record_path(&output, "Run record: ");
    let case_path = path.parent().unwrap().join("cases/0100.json");
    let case = read_json(&case_path);
    assert_eq!(case["artifact_version"], 3);
    assert_eq!(case["regression_plan"]["corpus"]["line"], 10);
    assert_eq!(case["regression_plan"]["corpus"]["iteration"], 99);
    assert_eq!(received.lock().unwrap()[1..], lines.iter().cycle().take(100).cloned().collect::<Vec<_>>());
    stderr(call("replay", &experiment, Some(&case_path)), 0);
    assert_eq!(received.lock().unwrap().last(), Some(&lines[9]));

    fs::write(&corpus_path, "Changed\n").unwrap();
    let output = stderr(call("replay", &experiment, Some(&case_path)), 2);
    assert!(output.contains("corpus fingerprint differs"), "{output}");
    assert_eq!(received.lock().unwrap().len(), 102);
    fs::write(&corpus_path, "").unwrap();
    let output = stderr(call("validate", &experiment, None), 2);
    assert!(output.contains("corpus has no lines"), "{output}");
    fs::write(&corpus_path, "Line.\n").unwrap();
    let output = stderr(call("validate", &experiment, None), 2);
    assert!(output.contains("requires letters, digits, or horizontal whitespace"), "{output}");
    fs::write(&corpus_path, &contents).unwrap();

    let mut limited = read_json(&experiment);
    limited["experiment"]["max_cases"] = json!(3);
    let limited = workspace.write("limited.yaml", &limited);
    let output = stderr(call("run", &limited, None), 0);
    assert!(output.contains("Cases: 3/3") && output.contains("MaxCasesReached"), "{output}");
}

#[test]
fn corpus_generated_text_preserves_the_line_order_and_replays() {
    use std::sync::Mutex;
    let workspace = Workspace::new();
    let received = Arc::new(Mutex::new(Vec::new()));
    let captured = received.clone();
    let service = Service::start(move |request| {
        captured.lock().unwrap().push(request.body["title"].as_str().unwrap().to_string());
        Some(Reply::json(200, json!({"ok": true})))
    });
    fs::write(workspace.0.join("lines.txt"), "First\nSecond\n").unwrap();
    let experiment = workspace.prepare(&service, 5, json!([{
        "id":"preview", "protocol":"http", "operation_id":"preview",
        "cases":{"mutators":[{"use":"file_fuzz", "fields":["title"], "path":"lines.txt", "repetitions":4,
                              "mode":"alpha", "seed":7, "length":{"reference":4, "max":12}}]}
    }]), json!({"/preview":operation("post", "preview", json!({"200":{"description":"OK"}}))}));
    let output = stderr(call("run", &experiment, None), 0);
    let path = record_path(&output, "Run record: ");
    let run = read_json(&path);
    for (case, line) in run["cases"].as_array().unwrap().iter().zip([1, 2, 1, 2]) {
        assert_eq!(case["regression_plan"]["corpus"]["line"], line);
        assert_eq!(case["regression_plan"]["corpus"]["seed"], 7);
    }
    let original = received.lock().unwrap().clone();
    assert!(original[1..].iter().all(|text| {
        text.len() != 4 && text.len() <= 12 && text.bytes().all(|byte| byte.is_ascii_alphabetic())
    }));
    let saved = path.parent().unwrap().join("cases/0003.json");
    stderr(call("replay", &experiment, Some(&saved)), 0);
    assert_eq!(received.lock().unwrap().last(), Some(&original[3]));
}

#[test]
fn protocol_names_and_json_format_preserve_legacy_experiments() {
    let workspace = Workspace::new();
    let service = Service::start(|request| {
        assert_eq!(request.path, "/preview");
        assert_eq!(request.body, json!({"title": "baseline"}));
        Some(Reply::json(200, json!({"ok": true})))
    });
    let experiment = workspace.prepare(&service, 1, json!([{
        "id":"preview", "adapter":"http", "operation_id":"preview",
        "oracles":[{"id":"json-body","use":"assert","select":"response.body.ok","expect":{"equals":true}}]
    }]), json!({"/preview":operation("post", "preview", json!({"200":{"description":"OK"}}))}));
    let old = read_json(&experiment);
    let mut current = old.clone();
    let block = current["experiment"].as_object_mut().unwrap();
    let protocols = block.remove("adapters").unwrap();
    block.insert("protocols".into(), protocols);
    let step = block["steps"][0].as_object_mut().unwrap();
    let protocol = step.remove("adapter").unwrap();
    step.insert("protocol".into(), protocol);
    step.insert("format".into(), json!("json"));
    let explicit = workspace.write("protocol.yaml", &current);

    for path in [&experiment, &explicit] {
        stderr(call("validate", path, None), 0);
        let report = stderr(call("run", path, None), 0);
        let record = read_json(&record_path(&report, "Run record: "));
        assert_eq!(record["baseline_case"]["step_results"][0]["status"], 200);
        assert_eq!(record["baseline_case"]["step_results"][0]["oracle_verdicts"][0]["verdict"], "pass");
    }

    let mut invalid = current.clone();
    invalid["experiment"]["adapters"] = old["experiment"]["adapters"].clone();
    let file = workspace.write("duplicate-protocols.yaml", &invalid);
    let text = stderr(call("validate", &file, None), 2);
    assert!(text.contains("duplicate field `protocols`"), "{text}");

    let mut invalid = current.clone();
    invalid["experiment"]["steps"][0]["adapter"] = json!("http");
    let file = workspace.write("duplicate-protocol.yaml", &invalid);
    let text = stderr(call("validate", &file, None), 2);
    assert!(text.contains("duplicate field `protocol`"), "{text}");

    current["experiment"]["steps"][0]["format"] = json!("yaml");
    let file = workspace.write("unknown-format.yaml", &current);
    let text = stderr(call("validate", &file, None), 2);
    assert!(text.contains("step 'preview'") && text.contains("unknown format 'yaml'"), "{text}");
}

#[test]
fn operation_path_runs_and_replays_a_case_with_a_body_example() {
    let workspace = Workspace::new();
    let service = Service::start(|request| {
        assert_eq!(request.path, "/v1.0/report");
        let status = match (request.body["title"].as_str(), request.body["fixed"].as_str()) {
            (Some("initial"), Some("bound")) => 200,
            (Some("variant"), Some("bound")) => 400,
            _ => 500,
        };
        Some(Reply::json(status, json!({"status": status})))
    });
    let experiment = workspace.prepare(&service, 2, json!([
        {"id":"post", "adapter":"http", "operation_path":"/v1.0/report.pOsT",
         "body_example":{"title":"initial", "fixed":"before binding"},
         "bind":{"body.fixed":"bound"},
         "cases":{"examples":[{"id":"changed","field":"title","value":"variant","expect_status":201}]}}
    ]), json!({
        "/v1.0/report": {
            "get":{"operationId":"getReport", "responses":{"200":{"description":"Read"}}},
            "post":{"requestBody":{"content":{"application/json":{"schema":{"type":"object"}}}},
                    "responses":{"200":{"description":"OK"}, "400":{"description":"Rejected"}}}
        }
    }));
    stderr(call("validate", &experiment, None), 0);
    let text = stderr(call("run", &experiment, None), 1);
    assert!(text.contains("Endpoints: 1"), "{text}");
    let run = read_json(&record_path(&text, "Run record: "));
    assert_eq!(run["baseline_case"]["step_results"][0]["status"], 200);
    assert_eq!(run["cases"][0]["step_results"][0]["status"], 400);
    let case = record_path(&text, "case record: ");
    let replay = stderr(call("replay", &experiment, Some(&case)), 1);
    assert!(replay.contains("example:changed"), "{replay}");
    assert!(replay.contains("400"), "{replay}");

    let original = read_json(&experiment);
    for (selector, message) in [
        (json!({}), "specify exactly one"),
        (json!({"operation_id":"getReport", "operation_path":"/v1.0/report.get"}), "specify exactly one"),
        (json!({"operation_path":"/v1.0/report"}), "invalid operation_path"),
        (json!({"operation_path":"/v1.0/report.delete"}), "not found in contract"),
    ] {
        let mut document = original.clone();
        let step = document["experiment"]["steps"][0].as_object_mut().unwrap();
        step.remove("operation_path");
        step.extend(selector.as_object().unwrap().clone());
        let invalid = workspace.write("invalid-selector.yaml", &document);
        let output = stderr(call("validate", &invalid, None), 2);
        assert!(output.contains("step 'post'") && output.contains(message), "{output}");
    }
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
        assert!(surface.contains("[hidden]"));
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

fn preview_operation() -> Value {
    json!({"/preview": {"post": {
        "operationId": "preview",
        "parameters": [{"name": "X-Token", "in": "header", "schema": {"type": "string"}}],
        "requestBody": {"content": {"application/json": {"schema": {"type": "object"}}}},
        "responses": {"200": {"description": "OK"}}
    }}})
}

#[test]
fn print_shows_the_template_and_debug_samples_without_slowing_the_run() {
    let workspace = Workspace::new();
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = requests.clone();
    let service = Service::start(move |request| {
        captured.lock().unwrap().push(request.content_type_count);
        Some(Reply::json(200, json!({"ok": true})))
    });
    let experiment = workspace.prepare(&service, 1, json!([{
        "id":"preview", "protocol":"http", "operation_id":"preview",
        "print": true, "debug": {"auto": 60},
        "body_example":{"hash":{"alg":"SHA-256", "hex":"demo"}, "ref":"x"},
        "bind":{"body.ref":"${case.id}", "header.X-Token":"${env.NADIR_PRINT_TEST_TOKEN:-token-value-1}"},
        "cases":{"mutators":[
            {"use":"json", "fields":["hash.hex"], "mode":"raw", "length":{"reference":4, "max":12}},
            {"use":"json", "keys":["hash.alg"], "mode":"raw", "expect_status":400}
        ]}
    }]), preview_operation());
    let mut document = read_json(&experiment);
    document["experiment"]["exploration"] = json!({"strategy":"fuzz","duration_s":30,"max_execs":30,"seed":5});
    document["experiment"]["hidden"] = json!(["NADIR_PRINT_TEST_TOKEN"]);
    fs::write(&experiment, document.to_string()).unwrap();

    let started = std::time::Instant::now();
    let output = call("run", &experiment, None);
    let elapsed = started.elapsed();
    let text = String::from_utf8(output.stderr).unwrap();
    assert!(matches!(output.status.code(), Some(0 | 1)), "{text}");
    let port = service.origin.rsplit(':').next().unwrap();
    let template = format!("POST /preview HTTP/1.1
Host: 127.0.0.1:{port}
X-Token: {{{{env.NADIR_PRINT_TEST_TOKEN}}}}
Content-Type: application/json
Content-Length: {{{{len}}}}

{{\"hash\":{{\"{{{{raw}}}}\":\"SHA-256\",\"hex\":\"{{{{raw}}}}\"}},\"ref\":\"{{{{case.id}}}}\"}}
  hash.hex: raw, length 0-3 or 5-12
  keys hash.alg: raw, one rename per case, expect 400
");
    assert!(text.contains("── print: preview ─"), "{text}");
    assert!(text.contains(&template), "{text}");
    // `auto: 60` shows the first request, with its own response, and never
    // waits: all 31 requests finish quickly.
    assert_eq!(text.matches("── debug: preview · ").count(), 1, "{text}");
    assert!(text.contains("── debug: preview · case 1 "), "{text}");
    assert_eq!(text.matches("X-Token: [hidden]").count(), 1, "{text}");
    assert!(!text.contains("token-value-1"), "{text}");
    assert_eq!(text.matches("response: 200 {\"ok\":true}").count(), 1, "{text}");
    assert!(!text.contains("previous response"), "{text}");
    assert!(elapsed < Duration::from_secs(10), "{elapsed:?}");
    assert!(text.contains("Random cases: 30 "), "{text}");
    // Every request carries one Content-Type header.
    let counts = requests.lock().unwrap().clone();
    assert_eq!(counts.len(), 31);
    assert!(counts.iter().all(|count| *count == 1), "{counts:?}");
}

#[test]
fn debug_true_runs_without_a_terminal_and_bad_forms_are_rejected() {
    let workspace = Workspace::new();
    let service = Service::start(move |_| Some(Reply::json(200, json!({}))));
    let step = |debug: Value| json!([{
        "id":"preview", "protocol":"http", "operation_id":"preview",
        "body_example":{"title":"x"}, "debug": debug
    }]);
    let experiment = workspace.prepare(&service, 1, step(json!(true)), preview_operation());
    stderr(call("validate", &experiment, None), 0);
    // Without a terminal nothing can refresh it: the first request is shown.
    let output = stderr(call("run", &experiment, None), 0);
    assert_eq!(output.matches("── debug: preview · case 1 ").count(), 1, "{output}");
    assert!(output.contains("response: 200 {}"), "{output}");

    let experiment = workspace.prepare(&service, 1, step(json!("yes")), preview_operation());
    let output = stderr(call("validate", &experiment, None), 2);
    assert!(output.contains("debug must be true, false or {auto: N}"), "{output}");
    let experiment = workspace.prepare(&service, 1, step(json!({"auto": 0})), preview_operation());
    let output = stderr(call("validate", &experiment, None), 2);
    assert!(output.contains("debug.auto must be between 1 and 3600"), "{output}");
    let experiment = workspace.prepare(&service, 1, step(json!({"auto": 2, "every": 1})), preview_operation());
    stderr(call("validate", &experiment, None), 2);
    let experiment = workspace.prepare(&service, 1, step(json!(false)), preview_operation());
    let output = stderr(call("run", &experiment, None), 0);
    assert!(!output.contains("── debug"), "{output}");
}

#[test]
fn crashes_are_written_next_to_the_experiment_as_they_happen_and_replay() {
    let workspace = Workspace::new();
    let service = Service::start(move |request| {
        let status = if request.body["title"] == "RESERVED" { 500 } else { 200 };
        Some(Reply::json(status, json!({"status": status})))
    });
    let experiment = workspace.prepare(&service, 20, json!([{
        "id":"preview", "protocol":"http", "operation_id":"preview",
        "body_example":{"title":"x"},
        "cases":{"mutators":[{"use":"json", "fields":["title"]}]},
        "oracles":[{"use":"openapi"}]
    }]), preview_operation());
    // Run from another directory: the crash folder follows the experiment.
    let elsewhere = workspace.0.join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_nadir"))
        .current_dir(&elsewhere)
        .arg("run")
        .arg(&experiment)
        .output()
        .unwrap();
    let text = stderr(output, 1);
    let crashes: Vec<PathBuf> = fs::read_dir(workspace.0.join("artifacts/crashes"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(crashes.len(), 1, "{crashes:?}");
    assert!(!elsewhere.join("artifacts").exists());
    let crash = &crashes[0];
    let name = crash.file_name().unwrap().to_str().unwrap();
    assert_eq!(name.len(), "27-11-2026-184512-a3f9c1.json".len(), "{name}");
    // The run's duration sits between the findings and the end of the report.
    let duration = text.lines().find_map(|line| line.strip_prefix("Duration: ")).expect(&text);
    let seconds = duration.strip_suffix('s').unwrap();
    assert!(seconds.parse::<f64>().is_ok() && seconds.contains('.'), "{duration}");
    assert!(text.find("Findings: ").unwrap() < text.find("Duration: ").unwrap());
    assert!(text.find("Duration: ").unwrap() < text.find("Stop reason: ").unwrap());
    let line = format!("crash: openapi · preview · HTTP 500 → {}", crash.display());
    assert!(text.contains(&line), "{text}");
    assert!(text.contains("── crashes "), "{text}");
    let record = read_json(crash);
    assert!(record["regression_plan"]["variant"].is_object(), "{record}");
    let replayed = stderr(call("replay", &experiment, Some(crash)), 1);
    assert!(replayed.contains("HTTP 500"), "{replayed}");
}

#[test]
fn stopping_from_debug_discards_the_pending_case_and_keeps_the_rest() {
    use nadir::execution::WorkflowRunner;
    use nadir::exploration::{run_enumeration, RunBudget, RunStopReason};
    use nadir::inspect::{Control, RequestInspector, RequestView, ResponseView};

    struct StopAtThird(usize);
    impl RequestInspector for StopAtThird {
        fn before_send(&mut self, _: &nadir::experiment::types::Step, _: &RequestView, _: &[String]) -> Control {
            self.0 += 1;
            if self.0 == 3 { Control::Stop } else { Control::Send }
        }
        fn after_send(&mut self, _: &nadir::experiment::types::Step, _: &ResponseView, _: &[String]) {}
    }

    let workspace = Workspace::new();
    let requests = Arc::new(std::sync::Mutex::new(0));
    let counted = requests.clone();
    let service = Service::start(move |_| {
        *counted.lock().unwrap() += 1;
        Some(Reply::json(200, json!({})))
    });
    let experiment = workspace.prepare(&service, 20, json!([{
        "id":"preview", "protocol":"http", "operation_id":"preview",
        "body_example":{"title":"x"}, "debug": {"auto": 1},
        "cases":{"mutators":[{"use":"json", "fields":["title"]}]}
    }]), preview_operation());
    let configuration = nadir::config::load_resolved(&experiment).unwrap();
    let mut runner = WorkflowRunner::new(configuration.experiment.clone(), configuration.contracts).unwrap();
    runner.set_inspector(Some(Box::new(StopAtThird(0))));
    let result = run_enumeration(&mut runner, &configuration.experiment, &mut RunBudget::new(20, None, None)).unwrap();
    assert_eq!(result.stop_reason, RunStopReason::StoppedByUser);
    assert_eq!(result.cases.len(), 1);
    assert_eq!(*requests.lock().unwrap(), 2);
}

fn items_operation() -> Value {
    json!({"/items/{id}": {"get": {
        "operationId": "getItem",
        "parameters": [{"name": "id", "in": "path", "required": true, "schema": {"type": "string"}}],
        "responses": {"200": {"description": "OK"}, "401": {"description": "Unauthorized"}}
    }}})
}

#[test]
fn http_mutator_replaces_bound_headers_removes_them_and_encodes_path_values() {
    let workspace = Workspace::new();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = seen.clone();
    let service = Service::start(move |request| {
        let keys: Vec<String> = request.headers.iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("x-key"))
            .map(|(_, value)| value.clone())
            .collect();
        let status = if keys == ["good-key"] { 200 } else { 401 };
        captured.lock().unwrap().push((request.path, keys));
        Some(Reply::json(status, json!({"status": status})))
    });
    let experiment = workspace.prepare(&service, 50, json!([{
        "id":"item", "protocol":"http", "operation_id":"getItem", "print": true,
        "bind":{"path.id":"abc", "header.X-Key":"good-key"},
        "cases":{"mutators":[
            {"use":"http", "fields":["header.X-Key"], "mode":"raw", "length":{"reference":8, "max":20}},
            {"use":"http", "fields":["header.X-Key"], "mode":"absent"},
            {"use":"http", "fields":["path.id"], "mode":"unconstrained"}
        ]},
        "oracles":[{"id":"accepted", "use":"assert", "select":"response.status", "expect":{"equals":200}}]
    }]), items_operation());
    let mut document = read_json(&experiment);
    document["experiment"]["exploration"] = json!({"strategy":"fuzz","duration_s":10,"max_execs":20,"seed":3});
    fs::write(&experiment, document.to_string()).unwrap();
    stderr(call("validate", &experiment, None), 0);
    let text = stderr(call("run", &experiment, None), 1);

    // The template shows every mutation of each part.
    assert!(text.contains("GET /items/{{unconstrained}} HTTP/1.1"), "{text}");
    assert!(text.contains("X-Key: {{raw|absent}}"), "{text}");
    assert!(text.contains("  header.X-Key: raw, length 0-7 or 9-20"), "{text}");

    let requests = seen.lock().unwrap().clone();
    assert_eq!(requests[0], ("/items/abc".to_owned(), vec!["good-key".to_owned()]));
    // A mutated header replaces the bound one: never two X-Key headers.
    assert!(requests.iter().all(|(_, keys)| keys.len() <= 1), "{requests:?}");
    assert!(requests.iter().any(|(_, keys)| keys.is_empty()), "absent case missing");
    assert!(requests.iter().any(|(_, keys)| keys.len() == 1 && keys[0] != "good-key"), "raw case missing");
    // Path values travel encoded as one segment, with the bound key.
    assert!(requests.iter().any(|(path, keys)| path == "/items/%3C%2Fscript%3E" && keys == &["good-key"]), "{requests:?}");
    assert!(requests.iter().any(|(path, _)| path == "/items/%00"), "{requests:?}");

    let crashes: Vec<PathBuf> = fs::read_dir(workspace.0.join("artifacts/crashes"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert!(!crashes.is_empty());
    for crash in &crashes {
        stderr(call("replay", &experiment, Some(crash)), 1);
    }
}

#[test]
fn json_and_http_take_num_and_value_files() {
    let workspace = Workspace::new();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = seen.clone();
    let service = Service::start(move |request| {
        let key = request.headers.iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("x-key"))
            .map(|(_, value)| value.clone());
        let ok = request.path == "/items/abc" && key.as_deref() == Some("good-key");
        captured.lock().unwrap().push((request.path, key));
        Some(Reply::json(if ok { 200 } else { 401 }, json!({})))
    });
    // A control character cannot travel in a header: that line only goes to the path.
    fs::write(workspace.0.join("payloads.txt"), "../etc\n\nctl\u{1}x\n").unwrap();
    let experiment = workspace.prepare(&service, 50, json!([{
        "id":"item", "protocol":"http", "operation_id":"getItem", "print": true,
        "bind":{"path.id":"abc", "header.X-Key":"good-key"},
        "cases":{"mutators":[
            {"use":"http", "fields":["header.X-Key", "path.id"], "path":"./payloads.txt"},
            {"use":"http", "fields":["path.id"], "mode":"num", "length":{"reference":3, "max":12}}
        ]},
        "oracles":[{"id":"accepted", "use":"assert", "select":"response.status", "expect":{"equals":200}}]
    }]), items_operation());
    let mut document = read_json(&experiment);
    document["experiment"]["exploration"] = json!({"strategy":"fuzz","duration_s":10,"max_execs":20,"seed":5});
    fs::write(&experiment, document.to_string()).unwrap();
    stderr(call("validate", &experiment, None), 0);
    let text = stderr(call("run", &experiment, None), 1);

    assert!(text.contains("GET /items/{{file|num}} HTTP/1.1") && text.contains("X-Key: {{file}}"), "{text}");
    assert!(text.contains("header.X-Key, path.id: file, 3 lines from ./payloads.txt"), "{text}");
    assert!(text.contains("path.id: num, length 0-2 or 4-12"), "{text}");
    let requests = seen.lock().unwrap().clone();
    let key = |value: &str| Some(value.to_owned());
    // Lines, encoded as one path segment, and as header values.
    assert!(requests.contains(&("/items/..%2Fetc".to_owned(), key("good-key"))), "{requests:?}");
    assert!(requests.contains(&("/items/ctl%01x".to_owned(), key("good-key"))), "{requests:?}");
    assert!(requests.contains(&("/items/abc".to_owned(), key("../etc"))), "{requests:?}");
    assert!(!requests.iter().any(|(_, value)| value.as_deref().is_some_and(|v| v.contains('\u{1}'))), "{requests:?}");
    // num: digits only, with the length policy.
    let digits: Vec<&str> = requests.iter()
        .filter_map(|(path, _)| path.strip_prefix("/items/"))
        .filter(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
        .collect();
    assert!(!digits.is_empty() && digits.iter().all(|id| id.len() <= 2 || (4..=12).contains(&id.len())), "{digits:?}");

    // A line case replays; once the file changes it no longer does.
    let crashes: Vec<PathBuf> = fs::read_dir(workspace.0.join("artifacts/crashes")).unwrap().map(|entry| entry.unwrap().path()).collect();
    let line_case = crashes.iter()
        .find(|path| read_json(path).to_string().contains("\"kind\":\"line\""))
        .expect("a saved line case");
    stderr(call("replay", &experiment, Some(line_case)), 1);
    fs::write(workspace.0.join("payloads.txt"), "other\n").unwrap();
    let refused = stderr(call("replay", &experiment, Some(line_case)), 2);
    assert!(refused.contains("changed since line"), "{refused}");

    // json: lines (NUL included, escaped in the body) and num.
    fs::write(workspace.0.join("payloads.txt"), "a\u{0}b\n'\n").unwrap();
    let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = bodies.clone();
    let service = Service::start(move |request| {
        captured.lock().unwrap().push(request.body.clone());
        Some(Reply::json(200, json!({"ok": true})))
    });
    let experiment = workspace.prepare(&service, 50, json!([{
        "id":"preview", "protocol":"http", "operation_id":"preview",
        "cases":{"mutators":[
            {"use":"json", "fields":["title"], "path":"./payloads.txt"},
            {"use":"json", "fields":["count"], "mode":"num", "length":{"reference":2, "max":8}},
            {"use":"json", "fields":["title"], "mode":"edge_cases"}
        ]},
        "print": true
    }]), json!({"/preview":operation("post", "preview", json!({"200":{"description":"OK"}}))}));
    let mut document = read_json(&experiment);
    document["experiment"]["exploration"] = json!({"strategy":"fuzz","duration_s":10,"max_execs":10,"seed":5});
    fs::write(&experiment, document.to_string()).unwrap();
    let text = stderr(call("run", &experiment, None), 0);
    // Several mutators on one body field show all their modes.
    assert!(text.contains(r#""title":"{{file|edge_cases}}""#), "{text}");
    let bodies = bodies.lock().unwrap().clone();
    let titles: Vec<&Value> = bodies.iter().map(|body| &body["title"]).collect();
    assert!(titles.contains(&&json!("a\u{0}b")) && titles.contains(&&json!("'")), "{bodies:?}");
    // num travels as a text of digits.
    let counts: Vec<&str> = bodies.iter().filter_map(|body| body["count"].as_str()).collect();
    assert!(!counts.is_empty() && counts.iter().all(|count| count.bytes().all(|byte| byte.is_ascii_digit())), "{bodies:?}");
}

#[test]
fn http_mutator_fields_are_checked() {
    let workspace = Workspace::new();
    let service = Service::start(move |_| Some(Reply::json(200, json!({}))));
    let check = |mutator: Value, expected: &str| {
        let experiment = workspace.prepare(&service, 5, json!([{
            "id":"item", "protocol":"http", "operation_id":"getItem",
            "bind":{"path.id":"abc"}, "cases":{"mutators":[mutator]}
        }]), items_operation());
        let output = stderr(call("validate", &experiment, None), 2);
        assert!(output.contains(expected), "{expected}: {output}");
    };
    check(json!({"use":"http", "fields":["X-Key"]}), "must start with header. or path.");
    check(json!({"use":"http", "fields":["query.q"]}), "must start with header. or path.");
    check(json!({"use":"http", "fields":["path.nope"]}), "path parameter 'nope' is not in the path");
    check(json!({"use":"http", "fields":["header.Bad Name"]}), "invalid header name 'Bad Name'");
    check(json!({"use":"http", "fields":["header.X-Key"], "values":[1]}), "values must be strings");
    check(json!({"use":"http", "fields":["header.A", "header.B"], "mode":"raw", "length":{"reference":4}}),
        "require exactly one field");
    check(json!({"use":"http", "fields":["header.X-Key"], "mode":"schema"}),
        "mode: schema needs 'header.X-Key' declared as a parameter");
    check(json!({"use":"headers", "fields":["X-Key"], "mode":"raw"}), "use: headers only takes fields and values");
    check(json!({"use":"json", "fields":["title"], "mode":"absent"}), "unknown mutator mode 'absent'");
    check(json!({"use":"http", "fields":["header.X-Key"], "mode":"nope"}), "unknown mutator mode 'nope'");
    check(json!({"use":"http", "fields":["header.X-Key"], "path":"./missing.txt"}), "path './missing.txt'");
    check(json!({"use":"http", "fields":["header.X-Key"], "path":"./x.txt", "mode":"raw"}), "path takes no mode or length");
    check(json!({"use":"headers", "fields":["X-Key"], "path":"./x.txt"}), "path is not an option of use: headers");
}

#[test]
fn only_variables_listed_in_hidden_are_hidden() {
    let workspace = Workspace::new();
    let service = Service::start(move |request| {
        // Echo the key back, as a careless service might.
        let key = request.headers.iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("x-key"))
            .map(|(_, value)| value.clone())
            .unwrap_or_default();
        Some(Reply::json(200, json!({"echo": key})))
    });
    let steps = json!([{
        "id":"item", "protocol":"http", "operation_id":"getItem", "debug": true,
        "bind":{"path.id":"abc", "header.X-Key":"${env.NADIR_HIDDEN_TEST_KEY:-hidden-key-value}"},
        "cases":{"mutators":[{"use":"http", "fields":["header.X-Key"], "mode":"values", "values":["fuzzed-key-value"]}]},
        "oracles":[{"id":"echo", "use":"assert", "select":"response.body.echo", "expect":{"equals":"never"}}]
    }]);
    let experiment = workspace.prepare(&service, 5, steps, items_operation());

    // Without `hidden` nothing is hidden: the key shows on screen and in records.
    let text = stderr(call("run", &experiment, None), 1);
    assert!(text.contains("X-Key: hidden-key-value"), "{text}");
    assert!(!text.contains("[hidden]"), "{text}");

    // The first run wrote its (visible) crash files; start the second clean.
    fs::remove_dir_all(workspace.0.join("artifacts")).unwrap();
    let mut document = read_json(&experiment);
    document["experiment"]["hidden"] = json!(["NADIR_HIDDEN_TEST_KEY"]);
    fs::write(&experiment, document.to_string()).unwrap();
    let text = stderr(call("run", &experiment, None), 1);
    assert!(text.contains("X-Key: [hidden]"), "{text}");
    assert!(text.contains("response: 200 {\"echo\":\"[hidden]\"}"), "{text}");
    let run = record_path(&text, "Run record: ");
    let mut evidence = fs::read_to_string(&run).unwrap();
    for entry in fs::read_dir(run.parent().unwrap().join("cases")).unwrap() {
        evidence.push_str(&fs::read_to_string(entry.unwrap().path()).unwrap());
    }
    for entry in fs::read_dir(workspace.0.join("artifacts/crashes")).unwrap() {
        evidence.push_str(&fs::read_to_string(entry.unwrap().path()).unwrap());
    }
    for surface in [&text, &evidence] {
        assert!(!surface.contains("hidden-key-value"), "{surface}");
    }
    // The fuzzed key is not the hidden value: it stays visible in the record.
    assert!(evidence.contains("fuzzed-key-value"), "{evidence}");
    assert!(evidence.contains("[hidden]"), "{evidence}");

    // Names must be used by some ${env.NAME}, once; short values only warn.
    for (hidden, expected) in [
        (json!(["NADIR_NOT_USED"]), "hidden variable 'NADIR_NOT_USED' is not used in any ${env.NADIR_NOT_USED} reference"),
        (json!(["NADIR_HIDDEN_TEST_KEY", "NADIR_HIDDEN_TEST_KEY"]), "hidden variable 'NADIR_HIDDEN_TEST_KEY' is listed twice"),
    ] {
        document["experiment"]["hidden"] = hidden;
        fs::write(&experiment, document.to_string()).unwrap();
        let output = stderr(call("validate", &experiment, None), 2);
        assert!(output.contains(expected), "{output}");
    }
    document["experiment"]["hidden"] = json!(["NADIR_HIDDEN_TEST_KEY"]);
    document["experiment"]["steps"][0]["bind"]["header.X-Key"] = json!("${env.NADIR_HIDDEN_TEST_KEY:-abc}");
    fs::write(&experiment, document.to_string()).unwrap();
    let output = stderr(call("run", &experiment, None), 1);
    assert!(output.contains("warning: hidden variable 'NADIR_HIDDEN_TEST_KEY' has fewer than 4 characters"), "{output}");
}

/// A TCP server that reads each request as raw bytes (until the request is
/// complete, the client ends it, or a short wait) and answers with `reply`.
/// `None` keeps the connection open without answering, then closes it.
struct RawService {
    origin: String,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    seen: Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
}

impl RawService {
    fn start(reply: fn(&[u8]) -> Option<Vec<u8>>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (signal, log) = (stop.clone(), seen.clone());
        let worker = thread::spawn(move || {
            while !signal.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let log = log.clone();
                        thread::spawn(move || {
                            stream.set_nonblocking(false).unwrap();
                            stream.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
                            let mut request = Vec::new();
                            let mut chunk = [0u8; 4096];
                            loop {
                                if complete(&request) {
                                    break;
                                }
                                match stream.read(&mut chunk) {
                                    Ok(0) | Err(_) => break,
                                    Ok(read) => request.extend_from_slice(&chunk[..read]),
                                }
                            }
                            log.lock().unwrap().push(request.clone());
                            match reply(&request) {
                                Some(response) => {
                                    let _ = stream.write_all(&response);
                                }
                                None => thread::sleep(Duration::from_millis(600)),
                            }
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("raw fixture could not accept: {error}"),
                }
            }
        });
        Self { origin, stop, worker: Some(worker), seen }
    }
}

/// A well-formed request whose `Content-Length` body arrived.
fn complete(request: &[u8]) -> bool {
    let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
        return false;
    };
    let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
    let length = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    request.len() >= end + 4 + length
}

impl Drop for RawService {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.worker.take().unwrap().join();
    }
}

fn wire_operation() -> Value {
    json!({"/sign/{kid}": {"post": {
        "operationId": "sign",
        "parameters": [{"name": "kid", "in": "path", "required": true, "schema": {"type": "string"}}],
        "requestBody": {"content": {"application/json": {"schema": {"type": "object"}}}},
        "responses": {"200": {"description": "OK"}, "400": {"description": "Bad"}}
    }}})
}

#[test]
fn http_raw_steps_send_malformed_requests_and_judge_how_they_are_handled() {
    let workspace = Workspace::new();
    let service = RawService::start(|request| {
        let text = String::from_utf8_lossy(request);
        let json = |status: u16| {
            let body = format!("{{\"status\":{status}}}");
            Some(format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).into_bytes())
        };
        if text.contains("Transfer-Encoding: chunked") {
            json(500)
        } else if text.contains("Content-Length: 7\r\n") {
            None // short_length: never answers
        } else if !text.contains("\r\n") {
            json(400) // bare_lf
        } else {
            json(200)
        }
    });
    let experiment = workspace.prepare(&RawOrigin(&service.origin), 20, json!([
        {"id":"sign", "protocol":"http", "operation_path":"/sign/{kid}.post",
         "body_example":{"hex":"abcd"}, "bind":{"path.kid":"k1"}},
        {"id":"axum", "protocol":"http_raw", "operation_path":"/sign/{kid}.post", "print": true,
         "body_example":{"hex":"abcd"}, "bind":{"path.kid":"k1"},
         "cases":{"mutators":[{"use":"malform", "only":["short_length","length_and_chunked","bare_lf"]}]},
         "oracles":[{"id":"base-ok", "use":"assert", "select":"response.status", "expect":{"equals":200}}]}
    ]), wire_operation());
    stderr(call("validate", &experiment, None), 0);
    let text = stderr(call("run", &experiment, None), 1);
    assert!(text.contains("Endpoints: 1"), "{text}");
    assert!(text.contains("  malform: catalog, 3 cases: short_length, length_and_chunked, bare_lf"), "{text}");
    assert!(text.contains("Connection: close"), "{text}");
    assert!(text.contains("Findings: 2"), "{text}");
    assert!(text.contains("wire:handled · axum · length_and_chunked · HTTP 500 → "), "{text}");
    assert!(text.contains("wire:handled · axum · short_length · no answer and no close before the timeout → "), "{text}");
    assert!(!text.contains("bare_lf ·"), "{text}");

    // Each case runs `sign` then `axum`, in order: baseline plus three malformations.
    let requests = service.seen.lock().unwrap().clone();
    assert_eq!(requests.len(), 8, "{requests:?}");
    for pair in requests.chunks(2) {
        let (sign, axum) = (String::from_utf8_lossy(&pair[0]), String::from_utf8_lossy(&pair[1]));
        assert!(sign.starts_with("POST /sign/k1 HTTP/1.1\r\n") && !sign.contains("Connection: close"), "{sign}");
        assert!(axum.contains("Connection: close") || !axum.contains("\r\n"), "{axum}");
    }
    let baseline = String::from_utf8_lossy(&requests[1]);
    assert!(baseline.starts_with("POST /sign/k1 HTTP/1.1\r\nHost: 127.0.0.1:"), "{baseline}");
    assert!(baseline.ends_with("Content-Length: 14\r\nConnection: close\r\n\r\n{\"hex\":\"abcd\"}"), "{baseline}");

    let crashes: Vec<PathBuf> = fs::read_dir(workspace.0.join("artifacts/crashes"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(crashes.len(), 2);
    for crash in &crashes {
        let record = read_json(crash);
        assert_eq!(record["artifact_version"], 5);
        assert_eq!(record["regression_plan"]["malform"]["step_id"], "axum");
        let replayed = stderr(call("replay", &experiment, Some(crash)), 1);
        assert!(replayed.contains("wire:handled"), "{replayed}");
    }
}

/// Lets `Workspace::prepare` take a raw service's origin.
struct RawOrigin<'a>(&'a str);

impl HasOrigin for RawOrigin<'_> {
    fn origin(&self) -> &str {
        self.0
    }
}

#[test]
fn http_raw_and_malform_only_go_together() {
    let workspace = Workspace::new();
    let service = Service::start(move |_| Some(Reply::json(200, json!({}))));
    let check = |step: Value, expected: &str| {
        let experiment = workspace.prepare(&service, 5, json!([step]), wire_operation());
        let output = stderr(call("validate", &experiment, None), 2);
        assert!(output.contains(expected), "{expected}: {output}");
    };
    let base = |protocol: &str, extra: Value| {
        let mut step = json!({"id":"s", "protocol":protocol, "operation_path":"/sign/{kid}.post",
            "body_example":{"hex":"abcd"}, "bind":{"path.kid":"k1"}});
        for (key, value) in extra.as_object().unwrap() {
            step[key] = value.clone();
        }
        step
    };
    check(base("http", json!({"cases":{"mutators":[{"use":"malform"}]}})), "use: malform requires protocol: http_raw");
    check(base("http_raw", json!({"cases":{"mutators":[{"use":"json", "fields":["hex"]}]}})),
        "protocol: http_raw only takes use: malform, found use: json");
    check(base("http_raw", json!({"outputs":{"x":{"from":"response.body.x"}}})), "protocol: http_raw does not take outputs");
    check(base("http_raw", json!({"cases":{"mutators":[{"use":"malform", "only":["nope"]}]}})),
        "unknown malformation 'nope' (expected one of: bad_method,");
    check(base("http_raw", json!({"cases":{"mutators":[{"use":"malform", "mode":"random"}]}})),
        "malform mode: random is available in a later phase");
}

#[test]
fn cli_env_expands_references_and_hides_listed_variables() {
    let workspace = Workspace::new();
    let step = json!({
        "id":"echo", "protocol":"cli",
        "command":["/bin/sh", "-c", "printf '%s|%s' \"$KEY\" \"$CASE\""],
        "env":{"KEY":"${env.NADIR_CLI_TEST_KEY:-cli-secret-value}", "CASE":"case-${case.id}"},
        "oracles":[{"id":"shows-output", "use":"assert", "select":"process.stdout", "expect":{"equals":"never"}}]
    });
    let experiment = workspace.prepare(&RawOrigin("http://127.0.0.1:9"), 3, json!([step.clone()]), json!({}));
    // Without `hidden` the expanded value is visible in the evidence.
    let text = stderr(call("run", &experiment, None), 1);
    assert!(text.contains("cli-secret-value|case-"), "{text}");
    assert!(!text.contains("${env.") && !text.contains("${case.id}"), "{text}");

    let mut document = read_json(&experiment);
    document["experiment"]["hidden"] = json!(["NADIR_CLI_TEST_KEY"]);
    fs::write(&experiment, document.to_string()).unwrap();
    let text = stderr(call("run", &experiment, None), 1);
    // Records hide the whole observed value when it holds a hidden one.
    assert!(text.contains(r#"expected "never", observed [hidden]"#), "{text}");
    assert!(!text.contains("cli-secret-value"), "{text}");

    // Step outputs cannot feed a cli step's environment.
    let mut bad = step;
    bad["env"]["KEY"] = json!("${steps.other.outputs.token}");
    let experiment = workspace.prepare(&RawOrigin("http://127.0.0.1:9"), 3, json!([bad]), json!({}));
    let output = stderr(call("validate", &experiment, None), 2);
    assert!(output.contains("is not available in a cli step"), "{output}");
}

#[test]
fn envar_modes_generate_random_cases_that_replay() {
    let workspace = Workspace::new();
    let experiment = workspace.prepare(&RawOrigin("http://127.0.0.1:9"), 3, json!([{
        "id":"echo", "protocol":"cli",
        "command":["/bin/sh", "-c", "printf '%s' \"$NUMBER\""],
        "env":{"NUMBER":"7"},
        "cases":{"mutators":[{"use":"envar", "variables":["NUMBER"], "mode":"num"}]},
        "oracles":[{"id":"stays-seven", "use":"assert", "select":"process.stdout", "expect":{"equals":"7"}}]
    }]), json!({}));
    let mut document = read_json(&experiment);
    document["experiment"]["exploration"] = json!({"strategy":"fuzz","duration_s":10,"max_execs":20,"seed":4});
    fs::write(&experiment, document.to_string()).unwrap();
    let text = stderr(call("run", &experiment, None), 1);
    assert!(text.contains("Random cases: 20 "), "{text}");
    let run = read_json(&record_path(&text, "Run record: "));
    let kept = run["cases"].as_array().unwrap();
    assert!(!kept.is_empty(), "{run}");
    for case in kept {
        let observed = case["step_results"][0]["oracle_verdicts"][0]["observed"].as_str().unwrap().trim_matches('"').to_owned();
        assert!((1..=64).contains(&observed.len()) && observed.bytes().all(|byte| byte.is_ascii_digit()), "{observed}");
    }
    let path = record_path(&text, "Run record: ");
    let first = path.parent().unwrap().join("cases/0001.json");
    let replay = stderr(call("replay", &experiment, Some(&first)), 1);
    assert!(replay.contains("stays-seven"), "{replay}");
}

#[test]
fn envar_raw_length_and_value_files_generate_cases_that_replay() {
    let workspace = Workspace::new();
    fs::write(workspace.0.join("values.txt"), "a\n\nx;y $(id)\n").unwrap();
    let experiment = workspace.prepare(&RawOrigin("http://127.0.0.1:9"), 40, json!([
        {
            "id":"lengths", "protocol":"cli", "print": true,
            "command":["/usr/bin/env", "python3", "-I", "-c",
                "import os; v = os.environ['V']; print(len(v), int(any(c < ' ' or c == '\\x7f' for c in v)))"],
            "env":{"V":"base", "PATH":"/usr/bin:/bin"},
            "cases":{"mutators":[{"use":"envar", "variables":["V"], "mode":"raw", "length":{"reference":8, "max":20}}]},
            "oracles":[{"id":"never", "use":"assert", "select":"process.stdout", "expect":{"equals":"never"}}]
        },
        {
            "id":"lines", "protocol":"cli", "print": true,
            "command":["/bin/sh", "-c", "printf '[%s]' \"$PICK\""],
            "env":{"PICK":"base"},
            "cases":{"mutators":[{"use":"envar", "variables":{"PICK":{"path":"./values.txt"}}}]},
            "oracles":[{"id":"stays-base", "use":"assert", "select":"process.stdout", "expect":{"equals":"[base]"}}]
        }
    ]), json!({}));
    let mut document = read_json(&experiment);
    document["experiment"].as_object_mut().unwrap().remove("adapters");
    document["experiment"]["protocols"] = json!({});
    document["experiment"]["exploration"] = json!({"strategy":"fuzz","duration_s":10,"max_execs":30,"seed":11});
    fs::write(&experiment, document.to_string()).unwrap();
    stderr(call("validate", &experiment, None), 0);
    let text = stderr(call("run", &experiment, None), 1);

    assert!(text.contains("V={{raw}}   (random values, length ~8, max 20)"), "{text}");
    assert!(text.contains("PICK={{file}}   (3 cases, from ./values.txt)"), "{text}");

    // Raw values follow the length policy (0-7 or 9-20 characters), carry
    // control characters now and then, and never NUL (the process started).
    let run = read_json(&record_path(&text, "Run record: "));
    let mut lengths = Vec::new();
    let mut controls = 0;
    for case in run["cases"].as_array().unwrap() {
        let result = &case["step_results"][0];
        let Some(observed) = result["oracle_verdicts"][0]["observed"].as_str() else { continue };
        let observed = observed.trim_matches('"').replace("\\n", "");
        let Some((length, control)) = observed.split_once(' ') else { continue };
        let length: usize = length.parse().unwrap();
        assert!(length <= 7 || (9..=20).contains(&length), "{observed}");
        lengths.push(length);
        controls += usize::from(control == "1");
    }
    // Cases where only the other step changed keep the base value (4).
    assert!(lengths.iter().filter(|length| **length != 4).count() >= 3 && controls > 0, "{lengths:?} {controls}");

    // Each line is a case (the blank one sends an empty value), saved with
    // its line number.
    assert!(text.contains(r#"observed "[a]""#), "{text}");
    let crashes: Vec<PathBuf> = fs::read_dir(workspace.0.join("artifacts/crashes"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    let mut lines: Vec<u64> = crashes
        .iter()
        .filter_map(|path| read_json(path)["regression_plan"]["envar"]["case"]["line"]["line"].as_u64())
        .collect();
    lines.sort();
    assert_eq!(lines, [1, 2, 3], "{crashes:?}");
    let line_case = crashes
        .iter()
        .find(|path| read_json(path)["regression_plan"]["envar"]["case"]["line"]["line"] == 3)
        .expect("a crash record for line 3");
    assert_eq!(read_json(line_case)["artifact_version"], 6);
    let replay = stderr(call("replay", &experiment, Some(line_case)), 1);
    assert!(replay.contains("stays-base"), "{replay}");

    // A changed file no longer replays the saved line.
    fs::write(workspace.0.join("values.txt"), "a\n\nother\n").unwrap();
    let refused = stderr(call("replay", &experiment, Some(line_case)), 2);
    assert!(refused.contains("the file of PICK in step 'lines' changed since line 3 was saved"), "{refused}");
}

#[test]
fn envar_absent_mode_starts_the_command_without_the_variable() {
    let workspace = Workspace::new();
    let experiment = workspace.prepare(&RawOrigin("http://127.0.0.1:9"), 5, json!([{
        "id":"show", "protocol":"cli", "print": true,
        "command":["/bin/sh", "-c", "printf '%s' \"${GONE-unset}\""],
        "env":{"GONE":"here"},
        "cases":{"mutators":[{"use":"envar", "variables":["GONE"], "mode":"absent"}]},
        "oracles":[{"id":"stays-here", "use":"assert", "select":"process.stdout", "expect":{"equals":"here"}}]
    }]), json!({}));
    let text = stderr(call("run", &experiment, None), 1);
    assert!(text.contains("GONE={{absent}}   (1 case)"), "{text}");
    assert!(text.contains(r#"expected "here", observed "unset""#), "{text}");
    let crash = fs::read_dir(workspace.0.join("artifacts/crashes")).unwrap().next().unwrap().unwrap().path();
    assert_eq!(read_json(&crash)["regression_plan"]["envar"]["case"], "unset");
    let replay = stderr(call("replay", &experiment, Some(&crash)), 1);
    assert!(replay.contains("stays-here"), "{replay}");
}

#[test]
fn envar_catalogs_and_list_values_enumerate_named_cases_that_replay() {
    let workspace = Workspace::new();
    let experiment = workspace.prepare(&RawOrigin("http://127.0.0.1:9"), 40, json!([{
        "id":"show", "protocol":"cli", "print": true,
        "command":["/bin/sh", "-c", "printf '%s|%s|%s' \"${#EDGE}\" \"$WILD\" \"$SAME_A$SAME_B\""],
        "env":{"EDGE":"e", "WILD":"w", "SAME_A":"a", "SAME_B":"b"},
        "cases":{"mutators":[
            {"use":"envar", "variables":["EDGE"], "mode":"edge_cases"},
            {"use":"envar", "variables":["WILD"], "mode":"unconstrained"},
            {"use":"envar", "variables":["SAME_A", "SAME_B"], "values":["x", "y"]}
        ]},
        "oracles":[{"id":"unchanged", "use":"assert", "select":"process.stdout", "expect":{"equals":"1|w|ab"}}]
    }]), json!({}));
    stderr(call("validate", &experiment, None), 0);
    let text = stderr(call("run", &experiment, None), 1);
    assert!(text.contains("EDGE={{edge_cases}}   (5 cases)"), "{text}");
    assert!(text.contains("WILD={{unconstrained}}   (8 cases)"), "{text}");
    assert!(text.contains("for VAR in 2 variables, one per case (4 cases):\n  VAR={{values}}"), "{text}");
    // 5 + 8 (with non_unicode) + 4 enumerated cases, every one changes the output.
    assert!(text.contains("Enumeration cases: 17"), "{text}");
    let crashes: Vec<PathBuf> = fs::read_dir(workspace.0.join("artifacts/crashes"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(crashes.len(), 17, "{crashes:?}");
    let script = crashes
        .iter()
        .find(|path| read_json(path)["regression_plan"]["envar"]["case"]["catalog"] == "script_tag")
        .expect("a saved script_tag case");
    let replay = stderr(call("replay", &experiment, Some(script)), 1);
    assert!(replay.contains("</script>"), "{replay}");
}

#[test]
fn envar_all_and_spec_vary_the_variables_no_other_mutator_takes() {
    let workspace = Workspace::new();
    fs::write(workspace.0.join("app.spec.yaml"), "B: { bool: {} }\nD: { bool: {} }\nE: { bool: {}, used_when: { C: prod } }\n").unwrap();
    let step = |mutators: Value| json!([{
        "id":"show", "protocol":"cli", "print": true,
        "command":["/bin/sh", "-c", "printf '%s|%s|%s|%s' \"${A-}\" \"${B-}\" \"${C-}\" \"${D-}\""],
        "env":{"A":"a", "B":"b", "C":"c"},
        "cases":{"mutators": mutators},
        "oracles":[{"id":"unchanged", "use":"assert", "select":"process.stdout", "expect":{"equals":"a|b|c|"}}]
    }]);

    // `all` is every base variable but A, which the first mutator varies.
    let experiment = workspace.prepare(&RawOrigin("http://127.0.0.1:9"), 20, step(json!([
        {"use":"envar", "variables":["A"], "values":["x"]},
        {"use":"envar", "variables":"all", "mode":"absent"}
    ])), json!({}));
    let text = stderr(call("run", &experiment, None), 1);
    assert!(text.contains("for VAR in 2 variables, one per case (2 cases):\n  VAR={{absent}}"), "{text}");
    assert!(text.contains("Enumeration cases: 3"), "{text}");

    // `spec` is the spec's variables the base allows (E needs C=prod), D
    // injected though the base lacks it.
    let experiment = workspace.prepare(&RawOrigin("http://127.0.0.1:9"), 20, step(json!([
        {"use":"envar", "variables":"spec", "spec":"./app.spec.yaml", "mode":"edge_cases"}
    ])), json!({}));
    let text = stderr(call("run", &experiment, None), 1);
    assert!(text.contains("for VAR in 2 variables, one per case (10 cases):\n  VAR={{edge_cases}}"), "{text}");
    assert!(text.contains("skipped: E (used when C=prod)"), "{text}");
    let crashes: Vec<PathBuf> = fs::read_dir(workspace.0.join("artifacts/crashes")).unwrap().map(|entry| entry.unwrap().path()).collect();
    let injected = crashes
        .iter()
        .find(|path| read_json(path)["regression_plan"]["envar"]["variable"] == "D")
        .expect("a case for D");
    stderr(call("replay", &experiment, Some(injected)), 1);

    // `only` narrows `spec`; a variable in two mutators is an error.
    let experiment = workspace.prepare(&RawOrigin("http://127.0.0.1:9"), 20, step(json!([
        {"use":"envar", "variables":"spec", "spec":"./app.spec.yaml", "only":["D"], "mode":"absent"}
    ])), json!({}));
    let text = stderr(call("run", &experiment, None), 0);
    assert!(text.contains("D={{absent}}   (1 case)"), "{text}");
    let experiment = workspace.prepare(&RawOrigin("http://127.0.0.1:9"), 20, step(json!([
        {"use":"envar", "variables":["A"], "values":["x"]},
        {"use":"envar", "variables":["A"], "mode":"absent"}
    ])), json!({}));
    assert!(stderr(call("validate", &experiment, None), 2).contains("'A' appears in more than one mutator"));
}

#[test]
fn envar_rejects_fields_it_would_ignore() {
    let workspace = Workspace::new();
    fs::write(workspace.0.join("values.txt"), "a\n").unwrap();
    let cases = [
        (json!({"use":"envar", "variables":["V"], "mode":"alpha", "length":{"max":20}}), "requires 'reference'"),
        (json!({"use":"envar", "variables":["V"], "path":"./values.txt", "length":{"reference":4}}), "'length' requires 'mode'"),
        (json!({"use":"envar", "variables":["V"], "mode":"num", "path":"./values.txt"}), "only one of 'mode', 'path' or 'values'"),
        (json!({"use":"envar", "variables":["V"], "values":["a"], "mode":"num"}), "only one of 'mode', 'path' or 'values'"),
        (json!({"use":"envar", "variables":["V"], "values":[]}), "'values' must contain at least one entry"),
        (json!({"use":"envar", "variables":["V"], "mode":"edge_cases", "length":{"reference":4}}), "edge_cases takes no 'length'"),
        (json!({"use":"envar", "variables":{"V":{"value":"x"}}, "values":["a"]}), "per variable"),
        (json!({"use":"envar", "variables":["V"], "mode":"num", "fields":["V"]}), "'fields' does not apply"),
        (json!({"use":"envar", "variables":["V"], "mode":"num", "seed":3}), "'seed' does not apply"),
        (json!({"use":"envar", "variables":{"V":{"value":"x"}}, "mode":"num"}), "per variable"),
        (json!({"use":"envar", "variables":{"V":{"value":"x", "path":"./values.txt"}}}), "only one of"),
        (json!({"use":"envar", "variables":["V"], "mode":"alpha", "only":["V"]}), "'only' requires mode: spec"),
        (json!({"use":"envar", "variables":"spec", "mode":"absent"}), "variables: spec requires 'spec'"),
        (json!({"use":"envar", "variables":"some", "mode":"absent"}), "expected a list, a map, 'all' or 'spec'"),
        (json!({"use":"envfile"}), "use: envfile requires 'file'"),
        (json!({"use":"envfile", "file":".env"}), "must be a destination of workdir.copy"),
        (json!({"use":"envfile", "file":".env", "fields":["x"]}), "'fields' does not apply to 'use: envfile'"),
        (json!({"use":"envar", "variables":["V"], "mode":"absent", "file":".env"}), "options of use: envfile"),
        (json!({"use":"envar", "variables":["V"], "mode":"absent", "length":{"reference":4}}), "absent takes no 'length'"),
    ];
    for (mutator, expected) in cases {
        let experiment = workspace.prepare(&RawOrigin("http://127.0.0.1:9"), 5, json!([{
            "id":"s", "protocol":"cli", "command":["/bin/true"], "env":{"V":"x"},
            "cases":{"mutators":[mutator]}
        }]), json!({}));
        let output = stderr(call("validate", &experiment, None), 2);
        assert!(output.contains(expected), "{expected}: {output}");
    }
}

/// A program that checks its key file, but lets the group read it.
const KEY_FILE_APP: &str = r##"
import os, stat, sys
path = os.environ.get("KEY_FILE", "key.txt")
if not path:
    print("KEY_FILE (process): must not be empty", file=sys.stderr); sys.exit(1)
try:
    info = os.stat(path)
except OSError:
    print("KEY_FILE (process): missing", file=sys.stderr); sys.exit(1)
if not stat.S_ISREG(info.st_mode):
    print("KEY_FILE (process): not a regular file", file=sys.stderr); sys.exit(1)
if info.st_mode & 0o037:                       # 0o077 would be right
    print("KEY_FILE (process): permissions too open", file=sys.stderr); sys.exit(1)
if info.st_size > 16:
    print("KEY_FILE (process): too large", file=sys.stderr); sys.exit(1)
try:
    open(path).read()
except OSError:
    print("KEY_FILE (process): unreadable", file=sys.stderr); sys.exit(1)
"##;

#[test]
fn file_specs_check_permissions_and_size_from_the_variable_file() {
    use std::os::unix::fs::PermissionsExt;
    let workspace = Workspace::new();
    fs::write(workspace.0.join("app.py"), KEY_FILE_APP).unwrap();
    fs::write(workspace.0.join("key.txt"), "abc\n").unwrap();
    fs::set_permissions(workspace.0.join("key.txt"), fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(workspace.0.join("app.spec.yaml"), "KEY_FILE: { file: { readable: true, max_mode: \"0600\", max_size: 16 } }\n").unwrap();
    let experiment = workspace.prepare(&RawOrigin("http://127.0.0.1:9"), 40, json!([{
        "id":"app", "protocol":"cli", "print": true,
        "command":["/usr/bin/env", "python3", "-I", "app.py"],
        "env":{"PATH":"/usr/bin:/bin", "KEY_FILE":"key.txt"},
        "workdir":{"copy":["app.py", "key.txt"]},
        "cases":{"mutators":[{"use":"envar", "mode":"spec", "spec":"./app.spec.yaml"}]},
        "oracles":[{"id":"base-runs", "use":"assert", "select":"process.exit_code", "expect":{"equals":0}}]
    }]), json!({}));
    let mut document = read_json(&experiment);
    document["experiment"].as_object_mut().unwrap().remove("adapters");
    document["experiment"]["protocols"] = json!({});
    fs::write(&experiment, document.to_string()).unwrap();
    let text = stderr(call("run", &experiment, None), 1);

    // 5 catalog cases (non_unicode too), 4 permission bits beyond 0600, too_large and unset.
    assert!(text.contains("KEY_FILE={{spec}}   (11 cases)"), "{text}");
    assert!(text.contains("startup:refuses · app · KEY_FILE=group_readable · exited 0"), "{text}");
    for refused in ["other_readable", "group_writable", "other_writable", "too_large", "missing", "unreadable"] {
        assert!(!text.contains(&format!("KEY_FILE={refused} ·")), "{refused}\n{text}");
    }
    let crashes: Vec<PathBuf> = fs::read_dir(workspace.0.join("artifacts/crashes")).unwrap().map(|entry| entry.unwrap().path()).collect();
    assert_eq!(crashes.len(), 1, "{crashes:?}");
    stderr(call("replay", &experiment, Some(&crashes[0])), 1);
}

/// A program that reads its `.env` with a few mistakes on purpose.
const DOTENV_APP: &str = r##"
import sys
try:
    data = open(".env", "rb").read()
except IsADirectoryError:
    print(".env must be a file", file=sys.stderr); sys.exit(1)
except PermissionError:
    print(".env could not be read", file=sys.stderr); sys.exit(1)
if len(data) > 2000:
    print(".env is too large", file=sys.stderr); sys.exit(1)
try:
    text = data.decode()
except UnicodeDecodeError:
    print(".env is not UTF-8", file=sys.stderr); sys.exit(1)
seen = set()
for number, line in enumerate(text.split("\n"), 1):
    if not line or line.startswith("#"):
        continue
    if "\0" in line:
        sys.exit(101)                                              # a panic
    if "\r" in line:
        print("bad character", file=sys.stderr); sys.exit(1)       # no line
    if "=" not in line:
        print(f".env line {number}: expected NAME=value", file=sys.stderr); sys.exit(1)
    name, value = line.split("=", 1)
    if value.startswith('"') and not value.endswith('"'):
        print(f".env line {number}: bad quotes in {value}", file=sys.stderr); sys.exit(1)  # echoes
    if name in seen:
        print(f".env line {number}: duplicate", file=sys.stderr); sys.exit(1)
    if not name.startswith("APP_") or name != name.upper():
        print(f".env line {number}: invalid name", file=sys.stderr); sys.exit(1)
    seen.add(name)
# Unknown and prohibited names are accepted: wrong on purpose.
"##;

#[test]
fn envfile_breaks_the_copied_file_one_way_per_case_and_judges_the_refusal() {
    let workspace = Workspace::new();
    fs::write(workspace.0.join("app.py"), DOTENV_APP).unwrap();
    fs::write(workspace.0.join("base.env"), "APP_MODE=dev\nAPP_LEVEL=info\n").unwrap();
    let experiment = workspace.prepare(&RawOrigin("http://127.0.0.1:9"), 40, json!([{
        "id":"app", "protocol":"cli", "print": true,
        "command":["/usr/bin/env", "python3", "-I", "app.py"],
        "env":{"PATH":"/usr/bin:/bin"},
        "workdir":{"copy":["app.py", {"from":"base.env", "to":".env"}]},
        "cases":{"mutators":[{"use":"envfile", "file":".env", "limits":{"file_bytes":2000}, "prohibited":["APP_SECRET"]}]},
        "oracles":[{"id":"base-runs", "use":"assert", "select":"process.exit_code", "expect":{"equals":0}}]
    }]), json!({}));
    let mut document = read_json(&experiment);
    document["experiment"].as_object_mut().unwrap().remove("adapters");
    document["experiment"]["protocols"] = json!({});
    fs::write(&experiment, document.to_string()).unwrap();
    stderr(call("validate", &experiment, None), 0);
    let text = stderr(call("run", &experiment, None), 1);

    assert!(text.contains(".env: {{envfile}}   (16 cases)"), "{text}");
    for expected in [
        "envfile:refuses · app · unknown_name · exited 0",
        "envfile:refuses · app · prohibited:APP_SECRET · exited 0",
        "envfile:refuses · app · nul_byte · exited 101 (panic or signal)",
        "envfile:refuses · app · lone_cr · exited 1 without naming line 2",
        "envfile:refuses · app · unclosed_quote · exited 1, printing the value",
        "envfile:refuses · app · text_after_quote · exited 1, printing the value",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
    // The rest refuse, naming the line or the file.
    for passed in ["duplicate_same", "lowercase_name", "export_prefix", "invalid_utf8", "unreadable", "directory", "file_too_large"] {
        assert!(!text.contains(&format!("· app · {passed} ·")), "{passed}\n{text}");
    }
    let crashes: Vec<PathBuf> = fs::read_dir(workspace.0.join("artifacts/crashes")).unwrap().map(|entry| entry.unwrap().path()).collect();
    assert_eq!(crashes.len(), 6, "{crashes:?}");
    for crash in &crashes {
        let record = read_json(crash);
        assert_eq!(record["artifact_version"], 7, "{record}");
        stderr(call("replay", &experiment, Some(crash)), 1);
    }
    // The copied file next to the experiment is never touched.
    assert_eq!(fs::read_to_string(workspace.0.join("base.env")).unwrap(), "APP_MODE=dev\nAPP_LEVEL=info\n");
}

/// A small service for `service` steps: it validates a few variables the
/// way a real one would (and gets some wrong on purpose), then serves
/// `/ready` on `PORT`.
const ENV_SERVICE: &str = r#"
import os, sys, http.server
if os.path.exists(".env"):
    sys.exit(4)                      # the experiment's .env must not leak in
if not os.path.exists("data.txt"):
    sys.exit(3)                      # workdir.copy must be in place
open("touched", "w").close()
mode = os.environ.get("MODE", "")
if mode not in ("dev", "prod"):
    print("MODE (process): must be dev or prod", file=sys.stderr); sys.exit(1)
token = os.environ.get("TOKEN", "")
if token != "abcd":
    print("bad TOKEN " + token, file=sys.stderr); sys.exit(1)   # echoes the value
if os.environ.get("PORT_HINT", "") == "":
    sys.exit(101)                    # a panic
# LEVEL is never checked: invalid values start anyway.
class Ready(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200); self.end_headers(); self.wfile.write(b"ok")
    def log_message(self, *args):
        pass
http.server.HTTPServer(("127.0.0.1", int(os.environ["PORT"])), Ready).serve_forever()
"#;

#[test]
fn service_steps_start_with_spec_violations_and_judge_the_refusal() {
    let workspace = Workspace::new();
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    fs::write(workspace.0.join("service.py"), ENV_SERVICE).unwrap();
    fs::write(workspace.0.join("data-source.txt"), "data").unwrap();
    fs::write(workspace.0.join(".env"), "MODE=dev\nLEVEL=info\nTOKEN=abcd\nPORT_HINT=x\nSECRET=super-secret-value\n").unwrap();
    fs::write(workspace.0.join("app.spec.yaml"), "MODE: { enum: [dev, prod], required: true }\nLEVEL: { enum: [info, warn] }\nTOKEN: { enum: [abcd] }\nPORT_HINT: { nonempty: {} }\n").unwrap();
    let experiment = workspace.prepare(&RawOrigin("http://127.0.0.1:9"), 50, json!([{
        "id":"serve", "protocol":"cli", "print": true,
        "command":["/usr/bin/env", "python3", "-I", "service.py"],
        "env_file":"./.env",
        "env":{"PORT": port.to_string(), "PATH": "/usr/bin:/bin", "API_TOKEN": "literal-token-value"},
        // `{from, to}` copies under another name: the service needs data.txt.
        "workdir":{"copy":["service.py", {"from":"data-source.txt", "to":"data.txt"}]},
        "service":{"ready":{"url": format!("http://127.0.0.1:{port}/ready"), "timeout_ms": 3000}},
        "cases":{"mutators":[{"use":"envar", "mode":"spec", "spec":"./app.spec.yaml"}]},
        "oracles":[{"id":"base-starts", "use":"assert", "select":"process.ready", "expect":{"equals":true}}]
    }]), json!({}));
    let mut document = read_json(&experiment);
    document["experiment"]["hidden"] = json!(["SECRET", "API_TOKEN"]);
    // Only cli steps, no OpenAPI: records carry no fingerprints and must replay.
    document["experiment"].as_object_mut().unwrap().remove("adapters");
    document["experiment"]["protocols"] = json!({});
    fs::write(&experiment, document.to_string()).unwrap();
    stderr(call("validate", &experiment, None), 0);
    let text = stderr(call("run", &experiment, None), 1);

    // The spec rule is one `for` line, not one line per variable; the YAML
    // values are shown (hidden ones redacted) and the .env ones counted.
    assert!(text.contains("for VAR in 4 variables, one per case (21 cases):\n  VAR={{spec}}"), "{text}");
    assert!(!text.contains("MODE={{spec}}"), "{text}");
    assert!(text.contains(&format!("PORT=\"{port}\"")) && text.contains("(+5 variables from ./.env)"), "{text}");
    assert!(text.contains("API_TOKEN=\"[hidden]\"") && !text.contains("literal-token-value"), "{text}");
    // The baseline started and was stopped.
    let run = read_json(&record_path(&text, "Run record: "));
    assert!(run["baseline_case"]["step_results"][0]["oracle_verdicts"][0]["verdict"] == "pass", "{run}");
    // One finding per broken promise; MODE is handled correctly.
    for expected in [
        "startup:refuses · serve · LEVEL=other_word · started (ready)",
        "startup:refuses · serve · TOKEN=other_word · exited 1, echoing the value",
        "startup:refuses · serve · PORT_HINT=empty · exited 101 (panic or signal)",
        // Without an optional variable the default must work.
        "startup:defaults · serve · TOKEN unset · exited 1 without TOKEN, which has a default: bad TOKEN",
        "startup:defaults · serve · PORT_HINT unset · exited 101 (panic or signal) without PORT_HINT",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
    // MODE is required and refused by name when missing; LEVEL starts with its default.
    assert!(!text.contains("· MODE") && !text.contains("LEVEL unset"), "{text}");
    assert!(!text.contains("super-secret-value"), "{text}");
    // Each case ran in its own temporary directory, now gone.
    assert!(!workspace.0.join("touched").exists());

    let crashes: Vec<PathBuf> = fs::read_dir(workspace.0.join("artifacts/crashes"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(crashes.len(), 11, "{crashes:?}"); // 5 LEVEL, 3 TOKEN, 3 PORT_HINT
    // Bytes that are not UTF-8: echoed by their readable prefix, accepted where unchecked.
    for expected in ["TOKEN=non_unicode · exited 1, echoing the value", "PORT_HINT=non_unicode · started (ready)"] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
    for crash in &crashes {
        let record = read_json(crash);
        assert_eq!(record["artifact_version"], 6, "{record}");
        assert_eq!(record["regression_plan"]["openapi_fingerprints"], json!({}), "{record}");
        let replay = stderr(call("replay", &experiment, Some(crash)), 1);
        assert!(replay.contains("Replayed case:"), "{replay}");
    }
}
