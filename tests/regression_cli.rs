use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

use serde_json::Value;

const PASSWORD: &str = "fixture-secret-sentinel";

/// Remove an example's `target` block so a test can run its own fixture.
fn without_target(source: &str) -> String {
    let Some((before, rest)) = source.split_once("  target:\n") else {
        return source.to_owned();
    };
    let after = &rest[rest.find("\n  adapters:").unwrap() + 1..];
    format!("{before}{after}")
}

const CREATE_RECORD: &str = include_str!("../examples/create-record/experiment.yaml");

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("nadir-cli-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn experiment(&self, port: u16) -> PathBuf {
        let source = without_target(CREATE_RECORD);
        let destination = self.0.join("experiment.yaml");
        fs::write(
            &destination,
            source.replace("127.0.0.1:8080", &format!("127.0.0.1:{port}")),
        )
        .unwrap();
        fs::write(
            self.0.join("openapi.yaml"),
            include_str!("../examples/create-record/openapi.yaml"),
        )
        .unwrap();
        destination
    }

    /// The auth-matrix example against an externally started fixture: the
    /// `target` block is removed so the test controls the fixture's passwords.
    fn auth_experiment(&self, port: u16) -> String {
        let source = include_str!("../examples/auth-matrix/experiment.yaml");
        let destination = self.0.join("auth-experiment.yaml");
        let adjusted = without_target(source)
            .replace("127.0.0.1:8091", &format!("127.0.0.1:{port}"))
            .replace("../create-record/openapi.yaml", "openapi.yaml");
        fs::write(&destination, adjusted).unwrap();
        fs::write(
            self.0.join("openapi.yaml"),
            include_str!("../examples/create-record/openapi.yaml"),
        )
        .unwrap();
        destination.to_string_lossy().into_owned()
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Fixture(Child);

impl Fixture {
    fn start(port: u16, mode: &str) -> (Self, u16) {
        let script =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/python_api/server.py");
        let mut fixture = Self(
            Command::new("python3")
                .arg("-I")
                .arg(script)
                .args(["--port", &port.to_string(), "--mode", mode])
                .env("NADIR_FIXTURE_PASSWORD", PASSWORD)
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let port = Fixture::read_port(&mut fixture);
        (fixture, port)
    }

    fn auth_start(port: u16, mode: &str, admin_password: &str, user_password: &str) -> (Self, u16) {
        let script =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/python_api/server.py");
        let mut fixture = Self(
            Command::new("python3")
                .arg("-I")
                .arg(script)
                .args(["--port", &port.to_string(), "--mode", mode])
                .env("NADIR_ADMIN_PASSWORD", admin_password)
                .env("NADIR_USER_PASSWORD", user_password)
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let port = Fixture::read_port(&mut fixture);
        (fixture, port)
    }

    fn read_port(fixture: &mut Self) -> u16 {
        let mut stderr = BufReader::new(fixture.0.stderr.take().unwrap());
        let mut startup = String::new();
        stderr.read_line(&mut startup).unwrap();
        let address = startup
            .split_whitespace()
            .nth(3)
            .filter(|value| value.starts_with("127.0.0.1:"))
            .unwrap_or_else(|| panic!("fixture failed to start: {startup}"));
        let port: u16 = address.split_once(':').unwrap().1.parse().unwrap();
        // Drain the fixture's request logs while the CLI is active.
        std::thread::spawn(move || {
            let _ = std::io::copy(&mut stderr, &mut std::io::sink());
        });
        port
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn cli(args: &[&str], password: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_nadir"))
        .args(args)
        .env("NADIR_FIXTURE_PASSWORD", password)
        .output()
        .unwrap()
}

fn stderr(output: &Output, expected: i32) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(expected), "{stderr}");
    assert!(!stderr.contains(PASSWORD), "{stderr}");
    assert!(!stderr.contains("tok-fixture-"), "{stderr}");
    stderr.into_owned()
}

fn path_from_output(output: &str, prefix: &str) -> PathBuf {
    output
        .lines()
        .find_map(|line| line.strip_prefix(prefix))
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("missing {prefix} in {output}"))
}

fn read_record(path: &Path) -> Value {
    let source = fs::read_to_string(path).unwrap();
    assert!(!source.contains(PASSWORD));
    assert!(!source.contains("tok-fixture-"));
    serde_json::from_str(&source).unwrap()
}

#[test]
fn cli_runs_reports_and_replays_a_local_regression() {
    let directory = TestDirectory::new();
    let (normal, port) = Fixture::start(0, "corrected");
    let experiment_file = directory.experiment(port);
    let experiment = experiment_file.to_str().unwrap();

    let mut unauthenticated = TcpStream::connect(("127.0.0.1", port)).unwrap();
    unauthenticated
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    unauthenticated
        .write_all(b"GET /templates/tpl-123 HTTP/1.0\r\n\r\n")
        .unwrap();
    let mut status = String::new();
    BufReader::new(unauthenticated)
        .read_line(&mut status)
        .unwrap();
    assert!(status.starts_with("HTTP/1.0 401"), "{status}");

    let openapi = directory.0.join("openapi.yaml");
    let fragment = directory.0.join("template.yaml");
    let contract = fs::read_to_string(&openapi)
        .unwrap()
        .replace("#/components/schemas/Template", "template.yaml");
    fs::write(&openapi, &contract).unwrap();
    fs::write(&fragment, "type: object\n").unwrap();

    let output = stderr(&cli(&["run", experiment], PASSWORD), 0);
    assert!(output.contains("Findings: 0"), "{output}");
    let run = read_record(&path_from_output(&output, "Run record: "));
    let cases = run["cases"].as_array().unwrap();
    assert!(!cases.is_empty());
    assert_eq!(run["status"], "passed");
    assert_eq!(
        run["counters"]["requests_sent"].as_u64(),
        Some((cases.len() as u64 + 1) * 4)
    );
    let statuses: Vec<_> = run["baseline_case"]["step_results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| step["status"].as_u64().unwrap())
        .collect();
    assert_eq!(statuses, [200, 200, 201, 200]);

    let output = stderr(&cli(&["run", experiment], "invalid-password"), 2);
    assert!(output.contains("SetupCheckFailed"), "{output}");
    assert!(output.contains("login-succeeds"), "{output}");
    assert!(output.contains("Enumeration cases: 0"), "{output}");
    drop(normal);

    let output = stderr(&cli(&["run", experiment], PASSWORD), 2);
    assert!(output.contains("transport_error"), "{output}");
    assert!(output.contains("Enumeration cases: 0"), "{output}");

    assert!(output.contains("Findings: 0"), "{output}");
    let incomplete = read_record(&path_from_output(&output, "Run record: "));
    assert_eq!(incomplete["counters"]["findings_count"], 0);
    assert_eq!(incomplete["status"], "incomplete");
    assert_eq!(
        incomplete["baseline_case"]["step_results"][0]["oracle_verdicts"][1]["verdict"],
        "inconclusive"
    );

    let source = fs::read_to_string(&experiment_file).unwrap();
    fs::write(
        &experiment_file,
        source.replace("          setup: true\n", ""),
    )
    .unwrap();
    let output = stderr(&cli(&["run", experiment], PASSWORD), 2);
    assert!(output.contains("BaselineIncomplete"), "{output}");
    assert!(output.contains("Findings: 0"), "{output}");
    fs::write(&experiment_file, &source).unwrap();

    let (regression, _) = Fixture::start(port, "title_regression");
    let output = stderr(&cli(&["run", experiment], PASSWORD), 1);
    for detail in [
        "template-title-preserved",
        "expected \"Invoice Template\"",
        "observed \"Invoice Template (draft)\"",
        "Findings: 1",
        "replay:",
    ] {
        assert!(output.contains(detail), "{output}");
    }
    let regression_run = path_from_output(&output, "Run record: ");
    let saved = path_from_output(&output, "case record: ");
    let saved_case = read_record(&saved);
    let steps = saved_case["step_results"].as_array().unwrap();
    assert!(steps.iter().all(|step| step["body_sha256"].is_null()
        && step["extracted_outputs"].as_object().unwrap().is_empty()));
    let copied = directory.0.join("copied-case.json");
    fs::copy(saved, &copied).unwrap();
    let replay = ["replay", experiment, "--case", copied.to_str().unwrap()];
    let output = stderr(&cli(&replay, PASSWORD), 1);
    assert!(output.contains("template-title-preserved"), "{output}");
    let replayed_id = output
        .lines()
        .find_map(|line| line.strip_prefix("Replayed case: "))
        .unwrap();
    assert_ne!(saved_case["case_id"].as_str().unwrap(), replayed_id);
    drop(regression);

    let (_normal, _) = Fixture::start(port, "corrected");
    stderr(&cli(&replay, PASSWORD), 0);

    let run = read_record(&regression_run);
    assert_eq!(run["findings"][0]["oracle_id"], "template-title-preserved");
    assert_eq!(run["counters"]["findings_count"], 1);
    assert_eq!(run["status"], "failed");
    assert_eq!(saved_case["artifact_version"], 2);
    assert!(saved_case["regression_plan"]["openapi_fingerprints"]["template.yaml"].is_string());

    fs::write(&fragment, "type: object\ndescription: compatible change\n").unwrap();
    let output = stderr(&cli(&replay, PASSWORD), 0);
    assert!(
        output.contains("changed OpenAPI contract: template.yaml"),
        "{output}"
    );

    drop(_normal);
    fs::write(
        &openapi,
        contract.replace("Accept-Language", "X-Other-Language"),
    )
    .unwrap();
    let output = stderr(&cli(&replay, PASSWORD), 2);
    assert!(output.contains("saved selector is absent"), "{output}");
    assert!(!output.contains("transport_error"), "{output}");

    fs::write(&openapi, &contract).unwrap();
    let (_normal, _) = Fixture::start(port, "corrected");
    let mut legacy = saved_case;
    legacy["artifact_version"] = 1.into();
    let plan = legacy["regression_plan"].as_object_mut().unwrap();
    plan.remove("openapi_fingerprints");
    let variant = plan["variant"].as_object_mut().unwrap();
    variant.remove("variant_id");
    variant.insert("choice".into(), 1.into());
    fs::write(&copied, serde_json::to_vec(&legacy).unwrap()).unwrap();
    let output = stderr(&cli(&replay, PASSWORD), 0);
    assert!(
        output.contains("Original OpenAPI contract is unverified"),
        "{output}"
    );

    legacy.as_object_mut().unwrap().remove("regression_plan");
    fs::write(&copied, serde_json::to_vec(&legacy).unwrap()).unwrap();
    let output = stderr(&cli(&replay, PASSWORD), 2);
    assert!(output.contains("no replayable regression plan"), "{output}");
}

#[test]
fn cli_checks_configuration_help_and_artifact_errors() {
    let directory = TestDirectory::new();
    let experiment_file = directory.experiment(8080);
    let experiment = experiment_file.to_str().unwrap();

    let help = cli(&["--help"], PASSWORD);
    assert_eq!(help.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&help.stdout).contains("random cases with strategy: fuzz"));
    assert!(
        stderr(&cli(&["run", experiment, "--corpus", "."], PASSWORD), 2)
            .contains("--corpus is unavailable")
    );
    // `strategy: fuzz` needs a positive duration and no target block.
    let source = fs::read_to_string(&experiment_file).unwrap();
    let fuzz = |options: &str| {
        fs::write(
            &experiment_file,
            source.replace("    strategy: enumerate\n", &format!("    strategy: fuzz\n{options}")),
        )
        .unwrap();
        cli(&["validate", experiment], PASSWORD)
    };
    stderr(&fuzz("    duration_s: 10\n    seed: 3\n"), 0);
    assert!(stderr(&fuzz(""), 2).contains("requires 'duration_s'"));
    assert!(stderr(&fuzz("    duration_s: 0\n"), 2).contains("duration_s must be at least 1"));
    assert!(
        stderr(&fuzz("    duration_s: 5\n    max_execs: 0\n"), 2)
            .contains("max_execs must be at least 1")
    );
    fs::write(&experiment_file, &source).unwrap();

    let source = fs::read_to_string(&experiment_file).unwrap();
    let coverage = source.replace(
        "  adapters:",
        "  target:\n    command: [python3]\n    ready: {}\n    coverage:\n      transport: mmap_file\n  adapters:",
    );
    fs::write(&experiment_file, coverage).unwrap();
    stderr(&cli(&["validate", experiment], PASSWORD), 0);
    let output = stderr(&cli(&["run", experiment], PASSWORD), 2);
    assert!(
        output.contains("target.coverage is unavailable"),
        "{output}"
    );
    assert!(!directory.0.join(".nadir").exists());
    fs::write(&experiment_file, &source).unwrap();

    // A file blocking the artifact directory also works when tests run as root.
    fs::write(directory.0.join(".nadir"), "").unwrap();
    assert!(stderr(&cli(&["run", experiment], PASSWORD), 2).contains(".nadir/runs"));

    fs::write(
        &experiment_file,
        source.replace("mode: edge_cases", "mode: schema_valid"),
    )
    .unwrap();
    let output = stderr(&cli(&["validate", experiment], PASSWORD), 0);
    assert!(
        output.contains("schema_valid is an alias for edge_cases"),
        "{output}"
    );
    fs::write(
        &experiment_file,
        source.replace("      bind:\n        path.id: tpl-123\n", ""),
    )
    .unwrap();
    let output = stderr(&cli(&["validate", experiment], PASSWORD), 2);
    assert!(output.contains("get_template"), "{output}");
    assert!(output.contains("required path parameter 'id'"), "{output}");
}

#[test]
fn library_prepares_runtime_references_before_sending_http() {
    use nadir::config::load_resolved;
    use nadir::execution::{PrepareError, WorkflowRunner};

    let directory = TestDirectory::new();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let experiment_file = directory.0.join("experiment.yaml");
    let contract = r#"
openapi: 3.1.0
info: {title: Runtime preparation fixture, version: '1'}
paths:
  /reports/{port}?view=summary:
    post:
      operationId: writeReport
      parameters:
        - {name: port, in: path, required: true, schema: {type: string}}
        - {name: X-Report-Type, in: header, schema: {type: string}}
      requestBody:
        content:
          application/json:
            schema: {type: object, properties: {title: {type: string}}}
            example: {title: Summary}
      responses:
        '200': {description: OK}
"#;
    fs::write(directory.0.join("openapi.yaml"), contract).unwrap();
    let experiment = format!(
        r#"
version: 1
experiment:
  name: runtime-preparation
  max_cases: 1
  adapters:
    http:
      base_url: http://127.0.0.1:{port}/api/
      openapi: openapi.yaml
      timeout_ms: 2000
  steps:
    - id: report
      adapter: http
      operation_id: writeReport
      bind:
        path.port: "${{target.port}}"
        header.X-Report-Type: summary
"#
    );
    let missing_env = format!("NADIR_MISSING_TEST_{}", uuid::Uuid::new_v4().simple());
    fs::write(
        &experiment_file,
        experiment.replace(
            "header.X-Report-Type: summary",
            &format!(r#"header.X-Report-Type: "${{env.{missing_env}}}""#),
        ),
    )
    .unwrap();

    let configuration = load_resolved(&experiment_file).unwrap();
    assert!(matches!(
        WorkflowRunner::new(configuration.experiment, configuration.contracts),
        Err(PrepareError::Environment { step_id, name }) if step_id == "report" && name == missing_env
    ));
    listener.set_nonblocking(true).unwrap();
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );

    fs::write(&experiment_file, experiment).unwrap();
    let configuration = load_resolved(&experiment_file).unwrap();
    let mut runner =
        WorkflowRunner::new(configuration.experiment, configuration.contracts).unwrap();
    let server = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut client = loop {
            match listener.accept() {
                Ok((client, _)) => break client,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(5))
                }
                Err(error) => panic!("did not accept local request: {error}"),
            }
        };
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let mut reader = BufReader::new(&mut client);
        let mut request_line = String::new();
        reader.read_line(&mut request_line).unwrap();
        let mut headers = std::collections::HashMap::new();
        loop {
            let mut line = String::new();
            assert_ne!(reader.read_line(&mut line).unwrap(), 0);
            if line == "\r\n" {
                break;
            }
            let (key, value) = line.split_once(':').unwrap();
            headers.insert(key.to_ascii_lowercase(), value.trim().to_owned());
        }
        let mut body = vec![0; headers["content-length"].parse().unwrap()];
        reader.read_exact(&mut body).unwrap();
        client
            .write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\n{}")
            .unwrap();
        (request_line, headers, body)
    });

    let case = runner.execute_baseline().unwrap();
    let (request_line, headers, body) = server.join().unwrap();
    assert_eq!(
        request_line,
        format!("POST /reports/{port}?view=summary HTTP/1.1\r\n")
    );
    assert_eq!(headers["x-report-type"], "summary");
    assert_eq!(headers["content-type"], "application/json");
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap(),
        serde_json::json!({"title":"Summary"})
    );
    assert_eq!(case.requests_sent, 1);
    assert_eq!(
        case.step_results[0].observation.as_ref().unwrap().status,
        200
    );
}

fn auth_command(args: &[&str], admin_password: &str, user_password: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_nadir"))
        .args(args)
        .env("NADIR_ADMIN_PASSWORD", admin_password)
        .env("NADIR_USER_PASSWORD", user_password)
        .output()
        .unwrap()
}

fn auth_run(experiment: &str, admin_password: &str, user_password: &str) -> Output {
    auth_command(&["run", experiment], admin_password, user_password)
}

#[test]
fn cli_auth_matrix_checks_role_based_access() {
    let directory = TestDirectory::new();
    let admin_password = format!("admin-{}", uuid::Uuid::new_v4().simple());
    let user_password = format!("user-{}", uuid::Uuid::new_v4().simple());

    // 1. Correct fixture: admin creates, user reads, user cannot create
    let (auth_fixture, port) =
        Fixture::auth_start(0, "auth_matrix", &admin_password, &user_password);
    let experiment = directory.auth_experiment(port);

    let output = stderr(&auth_run(&experiment, &admin_password, &user_password), 0);
    assert!(output.contains("Findings: 0"), "{output}");
    let run = read_record(&path_from_output(&output, "Run record: "));
    assert_eq!(run["status"], "passed");
    let statuses: Vec<_> = run["baseline_case"]["step_results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| step["status"].as_u64().unwrap())
        .collect();
    assert_eq!(statuses, [200, 200, 201, 200, 403, 404]);
    drop(auth_fixture);

    // 2. Policy discrepancy: user can also create in auth_matrix_open mode
    let experiment = directory.auth_experiment(port);
    let (open_fixture, _) =
        Fixture::auth_start(port, "auth_matrix_open", &admin_password, &user_password);
    let output = stderr(&auth_run(&experiment, &admin_password, &user_password), 1);
    assert!(output.contains("user-create-rejected"), "{output}");
    // Both findings are in the baseline case, so its record is listed once.
    assert_eq!(output.matches("case record: ").count(), 1, "{output}");
    assert!(!output.contains("Findings: 0"), "{output}");
    assert!(!output.contains(&admin_password), "{output}");
    assert!(!output.contains(&user_password), "{output}");
    let run = read_record(&path_from_output(&output, "Run record: "));
    assert_eq!(run["status"], "failed");
    let finding_ids: Vec<&str> = run["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["oracle_id"].as_str().unwrap())
        .collect();
    assert!(
        finding_ids.contains(&"user-create-rejected"),
        "findings: {finding_ids:?}"
    );
    assert!(
        finding_ids.contains(&"no-record-from-rejected-create"),
        "findings: {finding_ids:?}"
    );
    let saved = path_from_output(&output, "case record: ");
    let saved_case = read_record(&saved);
    assert_eq!(saved_case["artifact_version"], 2);
    // Verify tokens and credentials are redacted from JSON
    let raw = fs::read_to_string(&saved).unwrap();
    assert!(!raw.contains(&admin_password));
    assert!(!raw.contains(&user_password));
    assert!(!raw.contains("tok-"));
    let output = stderr(
        &auth_command(
            &["replay", &experiment, "--case", saved.to_str().unwrap()],
            &admin_password,
            &user_password,
        ),
        1,
    );
    assert!(output.contains("user-create-rejected"), "{output}");
    assert!(
        output.contains("no-record-from-rejected-create"),
        "{output}"
    );
    assert!(!output.contains(&admin_password));
    assert!(!output.contains(&user_password));
    drop(open_fixture);

    // 3. The shipped example starts its own fixture through `target`.
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/auth-matrix/experiment.yaml");
    let managed = directory.0.join("managed-auth-experiment.yaml");
    let managed_port = free_port();
    fs::write(
        &managed,
        fs::read_to_string(&example)
            .unwrap()
            .replace("127.0.0.1:8091", &format!("127.0.0.1:{managed_port}"))
            .replace(
                "../../tests/fixtures/python_api/server.py",
                &Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/python_api/server.py")
                    .display()
                    .to_string(),
            )
            .replace("../create-record/openapi.yaml", "openapi.yaml"),
    )
    .unwrap();
    let output = stderr(
        &auth_run(managed.to_str().unwrap(), &admin_password, &user_password),
        0,
    );
    assert!(output.contains("Target ready: pid "), "{output}");
    assert!(output.contains("Findings: 0"), "{output}");
    assert!(TcpStream::connect(("127.0.0.1", managed_port)).is_err());

    // 4. Setup failure: wrong admin password → exit 2
    let experiment = directory.auth_experiment(port);
    let (_auth_fixture, _) =
        Fixture::auth_start(port, "auth_matrix", &admin_password, &user_password);
    let output = stderr(&auth_run(&experiment, "wrong-password", &user_password), 2);
    assert!(output.contains("SetupCheckFailed"), "{output}");
    assert!(output.contains("admin-login-succeeds"), "{output}");
    let run = read_record(&path_from_output(&output, "Run record: "));
    let steps = run["baseline_case"]["step_results"].as_array().unwrap();
    assert_eq!(steps[0]["status"], 401);
    assert!(steps
        .iter()
        .filter(|step| {
            matches!(
                step["step_id"].as_str(),
                Some("admin_creates_record" | "admin_verifies_no_record")
            )
        })
        .all(|step| {
            step["status"].is_null() && step["outcome"].as_str().unwrap().starts_with("skipped")
        }));
}

#[test]
fn cli_transport_diagnostics_show_reason_and_phase() {
    let directory = TestDirectory::new();

    // 1. Timeout: slow mode with very short client timeout
    let (slow, port) = Fixture::start(0, "slow");
    let source = without_target(CREATE_RECORD)
        .replace("timeout_ms: 2000", "timeout_ms: 10")
        .replace("max_cases: 12", "max_cases: 1");
    let experiment_file = directory.0.join("timeout-experiment.yaml");
    fs::write(
        &experiment_file,
        source.replace("127.0.0.1:8080", &format!("127.0.0.1:{port}")),
    )
    .unwrap();
    fs::write(
        directory.0.join("openapi.yaml"),
        include_str!("../examples/create-record/openapi.yaml"),
    )
    .unwrap();

    let output = stderr(
        &Command::new(env!("CARGO_BIN_EXE_nadir"))
            .args(["run", experiment_file.to_str().unwrap()])
            .env("NADIR_FIXTURE_PASSWORD", PASSWORD)
            .output()
            .unwrap(),
        2,
    );
    assert!(output.contains("transport_error"), "{output}");
    drop(slow);

    // 2. Normal response retains oracle results
    let (normal, port) = Fixture::start(port, "corrected");
    let experiment_file = directory.experiment(port);
    let output = stderr(
        &Command::new(env!("CARGO_BIN_EXE_nadir"))
            .args(["run", experiment_file.to_str().unwrap()])
            .env("NADIR_FIXTURE_PASSWORD", PASSWORD)
            .output()
            .unwrap(),
        0,
    );
    assert!(output.contains("Findings: 0"), "{output}");
    let run = read_record(&path_from_output(&output, "Run record: "));
    assert_eq!(run["status"], "passed");

    // Verify transport_reason is absent on normal runs
    let baseline = run["baseline_case"]["step_results"].as_array().unwrap();
    for step in baseline {
        assert!(step.get("transport_reason").is_none_or(|v| v.is_null()));
    }
    drop(normal);
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Write the create-record experiment with a `target` block. Paths in
/// `command` are relative to the experiment file.
fn target_experiment(directory: &TestDirectory, port: u16, command: &str, ready: &str) -> PathBuf {
    let source = without_target(CREATE_RECORD)
        .replace("127.0.0.1:8080", &format!("127.0.0.1:{port}"))
        .replace(
            "  adapters:",
            &format!("  target:\n    command: {command}\n    ready: {ready}\n\n  adapters:"),
        );
    let destination = directory.0.join("experiment.yaml");
    fs::write(&destination, source).unwrap();
    fs::write(
        directory.0.join("openapi.yaml"),
        include_str!("../examples/create-record/openapi.yaml"),
    )
    .unwrap();
    destination
}

/// Run the CLI from `/` so relative target paths must resolve against the experiment.
fn cli_elsewhere(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_nadir"))
        .args(args)
        .current_dir("/")
        .env("NADIR_FIXTURE_PASSWORD", PASSWORD)
        .output()
        .unwrap()
}

fn fixture_command(mode: &str) -> String {
    format!(r#"["python3", "-I", "server.py", "--port", "${{target.port}}", "--mode", "{mode}"]"#)
}

#[test]
fn cli_starts_watches_and_stops_the_target_process() {
    let directory = TestDirectory::new();
    fs::write(
        directory.0.join("server.py"),
        include_str!("fixtures/python_api/server.py"),
    )
    .unwrap();
    let port = free_port();
    let ready = "{ path: /health }";

    // 1. A healthy target is started, used, and stopped.
    let experiment_file = target_experiment(&directory, port, &fixture_command("corrected"), ready);
    let experiment = experiment_file.to_str().unwrap();
    let output = stderr(&cli_elsewhere(&["run", experiment]), 0);
    assert!(output.contains("Target ready: pid "), "{output}");
    assert!(output.contains("Findings: 0"), "{output}");
    assert!(
        TcpStream::connect(("127.0.0.1", port)).is_err(),
        "target still listening after the run"
    );

    // 2. A target that crashes on a variant is a process-exit finding,
    //    and replay restarts it and reproduces the crash.
    target_experiment(&directory, port, &fixture_command("exit_on_reserved"), ready);
    let output = stderr(&cli_elsewhere(&["run", experiment]), 1);
    assert!(output.contains("Stop reason: ProcessExit"), "{output}");
    assert!(
        output.contains("target process exited unexpectedly (exit code 1)"),
        "{output}"
    );
    assert!(output.contains("exiting on reserved title"), "{output}");
    assert!(
        output.contains("step post_record: target process exited (exit code 1)"),
        "{output}"
    );
    let run = read_record(&path_from_output(&output, "Run record: "));
    let crash = run["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|finding| finding["kind"] == "process_exit")
        .unwrap_or_else(|| panic!("no process_exit finding in {run}"));
    assert_eq!(crash["exit_code"], 1);
    assert_eq!(crash["step_id"], "post_record");
    let crash_case = output
        .lines()
        .skip_while(|line| !line.starts_with("step post_record: target process exited"))
        .find_map(|line| line.strip_prefix("case record: "))
        .unwrap();
    let output = stderr(&cli_elsewhere(&["replay", experiment, "--case", crash_case]), 1);
    assert!(output.contains("target process exited unexpectedly"), "{output}");
    assert!(TcpStream::connect(("127.0.0.1", port)).is_err());

    // 3. Nadir refuses to start a target when something already owns the port.
    target_experiment(&directory, port, &fixture_command("corrected"), ready);
    let (existing, _) = Fixture::start(port, "corrected");
    let output = stderr(&cli_elsewhere(&["run", experiment]), 2);
    assert!(output.contains("is already accepting connections"), "{output}");
    drop(existing);

    // 4. A target that exits during startup shows its stderr, with secrets redacted,
    //    and no run directory is created.
    fs::remove_dir_all(directory.0.join(".nadir")).unwrap();
    fs::write(
        directory.0.join("crash.py"),
        "import os, sys\n\
         print('boom-marker', file=sys.stderr)\n\
         print('password=' + os.environ['NADIR_FIXTURE_PASSWORD'], file=sys.stderr)\n\
         sys.exit(3)\n",
    )
    .unwrap();
    target_experiment(&directory, port, r#"["python3", "-I", "crash.py"]"#, ready);
    let output = stderr(&cli_elsewhere(&["run", experiment]), 2);
    assert!(output.contains("target exited during readiness check"), "{output}");
    assert!(output.contains("boom-marker"), "{output}");
    assert!(output.contains("[redacted]"), "{output}");
    assert!(!directory.0.join(".nadir").exists());

    // 5. A target that never becomes ready is stopped after ready.timeout_ms.
    target_experiment(
        &directory,
        port,
        r#"["python3", "-c", "import time; time.sleep(30)"]"#,
        "{ path: /health, timeout_ms: 300 }",
    );
    let started = std::time::Instant::now();
    let output = stderr(&cli_elsewhere(&["run", experiment]), 2);
    assert!(output.contains("target not ready after 300ms"), "{output}");
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
}

#[test]
fn cli_reports_server_errors_as_findings() {
    let directory = TestDirectory::new();
    let (fixture, port) = Fixture::start(0, "error_on_reserved");
    let experiment_file = directory.experiment(port);
    let output = stderr(&cli(&["run", experiment_file.to_str().unwrap()], PASSWORD), 1);
    assert!(
        output.contains("step post_record, rule openapi: fail; expected declared status (201, 400, 403, 422), no 5xx, valid JSON, observed HTTP 500"),
        "{output}"
    );
    let run = read_record(&path_from_output(&output, "Run record: "));
    let finding = &run["findings"].as_array().unwrap()[0];
    assert_eq!(finding["violation_category"], "ServerError");
    assert_eq!(finding["step_id"], "post_record");

    // The saved case reproduces the 500 on replay.
    let saved = path_from_output(&output, "case record: ");
    let output = stderr(
        &cli(
            &["replay", experiment_file.to_str().unwrap(), "--case", saved.to_str().unwrap()],
            PASSWORD,
        ),
        1,
    );
    assert!(output.contains("observed HTTP 500"), "{output}");
    drop(fixture);
}

#[test]
fn cli_replays_unconstrained_cases_and_checks_records_before_starting_the_target() {
    let directory = TestDirectory::new();
    let port = free_port();
    let source = include_str!("../examples/fuzz-probe/experiment.yaml")
        .replace("127.0.0.1:8081", &format!("127.0.0.1:{port}"))
        .replace(
            "../../tests/fixtures/python_api/server.py",
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/python_api/server.py")
                .display()
                .to_string(),
        )
        .replace("../create-record/openapi.yaml", "openapi.yaml")
        .replace("duration_s: 5", "duration_s: 1");
    let experiment_file = directory.0.join("probe.yaml");
    fs::write(&experiment_file, source).unwrap();
    fs::write(
        directory.0.join("openapi.yaml"),
        include_str!("../examples/create-record/openapi.yaml"),
    )
    .unwrap();
    let experiment = experiment_file.to_str().unwrap();

    let output = stderr(&cli(&["run", experiment], PASSWORD), 0);
    assert!(output.contains("Enumeration cases: 19"), "{output}");
    let run_file = path_from_output(&output, "Run record: ");
    let case_file = run_file.parent().unwrap().join("cases/0004.json");
    let case = read_record(&case_file);
    assert_eq!(
        case["regression_plan"]["variant"]["variant_id"]["kind"],
        "huge_uppercase"
    );
    let output = stderr(
        &cli(&["replay", experiment, "--case", case_file.to_str().unwrap()], PASSWORD),
        0,
    );
    assert!(output.contains("Replayed case: "), "{output}");

    // A record that cannot be replayed fails before any target is started.
    let mut unplayable = case.clone();
    unplayable["regression_plan"] = Value::Null;
    let unplayable_file = directory.0.join("unplayable.json");
    fs::write(&unplayable_file, unplayable.to_string()).unwrap();
    let output = stderr(
        &cli(&["replay", experiment, "--case", unplayable_file.to_str().unwrap()], PASSWORD),
        2,
    );
    assert!(output.contains("no replayable regression plan"), "{output}");
    assert!(!output.contains("Target ready"), "{output}");
}

#[test]
fn cli_schema_mode_reports_accepted_invalid_values() {
    let directory = TestDirectory::new();
    let port = free_port();
    let source = include_str!("../examples/fuzz-probe/experiment.yaml")
        .replace("127.0.0.1:8081", &format!("127.0.0.1:{port}"))
        .replace(
            "../../tests/fixtures/python_api/server.py",
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/python_api/server.py")
                .display()
                .to_string(),
        )
        .replace("../create-record/openapi.yaml", "openapi.yaml")
        .replace("${env.NADIR_FIXTURE_MODE:-corrected}", "lenient")
        .replace("duration_s: 5", "duration_s: 1");
    let experiment_file = directory.0.join("probe.yaml");
    fs::write(&experiment_file, source).unwrap();
    fs::write(
        directory.0.join("openapi.yaml"),
        include_str!("../examples/create-record/openapi.yaml"),
    )
    .unwrap();
    let experiment = experiment_file.to_str().unwrap();

    let output = stderr(&cli(&["run", experiment], PASSWORD), 1);
    assert!(
        output.contains(
            "step probe_schema, rule schema:rejects-invalid: fail; expected rejection (status >= 400), observed 200"
        ),
        "{output}"
    );
    let also = output
        .lines()
        .find_map(|line| line.strip_prefix("  same rule also failed in: "))
        .unwrap_or_else(|| panic!("no grouped cases in {output}"));
    for kind in ["null_value", "wrong_type_array", "over_max_length"] {
        assert!(also.contains(kind), "{also}");
    }
    // A value at the limit and custom values carry no rejection rule.
    assert!(!also.contains("at_max_length") && !also.contains("custom"), "{also}");

    let run = read_record(&path_from_output(&output, "Run record: "));
    let finding = &run["findings"].as_array().unwrap()[0];
    assert_eq!(finding["oracle_id"], "schema:rejects-invalid");
    assert_eq!(finding["occurrence_count"], 7);

    // The first accepted case (the missing field) replays with the same result.
    let saved = path_from_output(&output, "case record: ");
    assert_eq!(
        read_record(&saved)["regression_plan"]["variant"]["variant_id"]["kind"],
        "missing_field"
    );
    let output = stderr(&cli(&["replay", experiment, "--case", saved.to_str().unwrap()], PASSWORD), 1);
    assert!(output.contains("schema:rejects-invalid: fail"), "{output}");
}

#[test]
fn cli_fuzz_runs_random_cases_and_replays_them_from_seed_and_iteration() {
    let directory = TestDirectory::new();
    let (fixture, port) = Fixture::start(0, "error_on_reserved");
    let experiment_file = directory.experiment(port);
    let source = fs::read_to_string(&experiment_file).unwrap().replace(
        "    strategy: enumerate\n",
        "    strategy: fuzz\n    duration_s: 30\n    max_execs: 300\n    seed: 7\n",
    );
    fs::write(&experiment_file, &source).unwrap();
    let experiment = experiment_file.to_str().unwrap();

    let output = stderr(&cli(&["run", experiment], PASSWORD), 1);
    assert!(output.contains("Enumeration cases: 7"), "{output}");
    assert!(output.contains("Random cases: 300 (kept "), "{output}");
    assert!(output.contains("seed 7)"), "{output}");
    assert!(output.contains("Stop reason: MaxExecsReached"), "{output}");
    let run = read_record(&path_from_output(&output, "Run record: "));
    assert_eq!(run["fuzz"]["seed"], 7);
    assert_eq!(run["fuzz"]["executed"], 300);
    let kept = run["fuzz"]["kept"].as_u64().unwrap();
    assert!(kept >= 1, "the random phase should hit the 500 at least once: {run}");
    assert_eq!(run["cases"].as_array().unwrap().len() as u64, 7 + kept);

    // Random cases are saved by seed and iteration, and replay regenerates them.
    let random_case = run["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["regression_plan"]["fuzz"].is_object())
        .unwrap();
    assert_eq!(random_case["regression_plan"]["fuzz"]["seed"], 7);
    let case_file = path_from_output(&output, "Run record: ")
        .parent()
        .unwrap()
        .join(format!("cases/{:04}.json", random_case["case_number"].as_u64().unwrap()));
    let output = stderr(
        &cli(&["replay", experiment, "--case", case_file.to_str().unwrap()], PASSWORD),
        1,
    );
    assert!(output.contains("observed HTTP 500"), "{output}");

    // The same seed produces the same campaign.
    let again = stderr(&cli(&["run", experiment], PASSWORD), 1);
    let again = read_record(&path_from_output(&again, "Run record: "));
    assert_eq!(again["fuzz"]["kept"], run["fuzz"]["kept"]);
    let iterations = |run: &Value| -> Vec<Value> {
        run["cases"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|case| case["regression_plan"]["fuzz"].get("iteration").cloned())
            .collect()
    };
    assert_eq!(iterations(&again), iterations(&run));
    drop(fixture);
}
