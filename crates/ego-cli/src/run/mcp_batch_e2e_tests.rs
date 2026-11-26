//! End-to-end measurement of `mcp_batch` through the real headless driver
//! (`run_with_registry`), a loopback provider stub and a real stdio MCP server
//! subprocess. A test-only switch in the headless driver controls exposure for
//! the comparison. No hosted provider or external credentials are used.
use super::*;
use serde_json::{Value, json};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use super::test_support as support;
use support::{Plan, Reply, Stub, paced, paced_tool_call};

const CHILD: &str = "EGO_MCP_BATCH_E2E_ARM";
/// One injected latency, applied to every provider generation and every lookup.
const LATENCY: Duration = Duration::from_millis(300);
const KEYS: [&str; 3] = ["alpha", "bravo", "charlie"];
const ANSWER: &str = "ANSWER-alpha-bravo-charlie";
const TEST: &str = concat!(
    module_path!(),
    "::serial_and_batched_lookups_match_with_two_provider_requests_instead_of_four"
);

#[path = "mcp_batch_e2e_tests/restart.rs"]
mod restart;
#[path = "mcp_batch_e2e_tests/worker.rs"]
mod worker;

const SERVER: &str = r#"
import json
import os
import sys
import time

log_path, latency, scenario = sys.argv[1], float(sys.argv[2]), sys.argv[3]
for line in sys.stdin:
    request = json.loads(line)
    if "id" not in request:
        continue
    method = request["method"]
    if method == "server/discover":
        result = {
            "resultType": "complete",
            "supportedVersions": ["2026-07-28"],
            "capabilities": {"tools": {"listChanged": False}},
            "ttlMs": 0,
            "cacheScope": "private",
        }
    elif method == "tools/list":
        result = {
            "resultType": "complete",
            "tools": [
                {
                    "name": name,
                    "description": "Look up a value by key",
                    "inputSchema": {
                        "type": "object",
                        "properties": {"key": {"type": "string"}},
                        "required": ["key"],
                        "additionalProperties": False,
                    },
                }
                for name in ("lookup", "lookup_alt")
            ],
            "nextCursor": None,
            "ttlMs": 0,
            "cacheScope": "private",
        }
    elif method == "tools/call":
        params = request["params"]
        key = params["arguments"]["key"]
        resumed = params.get("requestState") == "opaque-state"
        assert params["arguments"] == {"key": key}, "protocol fields leaked into tool arguments"
        if resumed:
            assert params["inputResponses"] == {"answer": {"action": "accept", "content": {"choice": "yes"}}}
        start = time.time()
        time.sleep(latency)
        with open(log_path, "a") as log:
            log.write(json.dumps({"key": key, "start": start, "end": time.time(),
                                  "pid": os.getpid(), "resumed": resumed}) + "\n")
        if scenario == "restart" and key == "bravo" and not resumed:
            result = {
                "resultType": "input_required",
                "inputRequests": {"answer": {"method": "elicitation/create", "params": {
                    "mode": "form", "message": "Confirm lookup",
                    "requestedSchema": {"type": "object", "properties": {"choice": {"type": "string"}},
                                        "required": ["choice"], "additionalProperties": False}
                }}},
                "requestState": "opaque-state",
            }
        else:
            result = {
                "resultType": "complete",
                "content": [{"type": "text", "text": "value-of-" + key}],
                "isError": False,
            }
    else:
        raise AssertionError(method)
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
"#;

fn replies(batch: bool) -> Vec<Reply> {
    let call = |id: &str, name: &str, args: &str| paced_tool_call(id, name, args, LATENCY);
    if batch {
        let calls: Vec<Value> = KEYS
            .iter()
            .map(|key| json!({"tool": "lookup", "arguments": {"key": key}}))
            .collect();
        vec![
            call("b1", "mcp_batch", &json!({ "calls": calls }).to_string()),
            paced(ANSWER, LATENCY),
        ]
    } else {
        let mut all: Vec<Reply> = KEYS
            .iter()
            .enumerate()
            .map(|(index, key)| {
                call(
                    &format!("c{index}"),
                    "lookup",
                    &json!({"key": key}).to_string(),
                )
            })
            .collect();
        all.push(paced(ANSWER, LATENCY));
        all
    }
}

#[derive(Debug)]
struct Measured {
    generations: usize,
    request_bytes: usize,
    dispatch_keys: Vec<String>,
    dispatch_span_ms: u128,
    tool_texts: Vec<String>,
    batch_offered: bool,
    driver_elapsed_ms: u128,
}

/// Run one arm in a child process of this test binary and measure it.
fn run_arm(arm: &str) -> Measured {
    let stub = Stub::start(Plan {
        completions: replies(arm == "batch"),
        ..Plan::default()
    });
    let fixture = StdioFixture::new("measure", LATENCY);
    fixture.child(&stub, TEST, arm);
    let workspace = &fixture.workspace;
    let log = workspace.path().join("dispatch.log");
    let requests: Vec<_> = stub
        .requests()
        .into_iter()
        .filter(|request| request.path.contains("chat/completions"))
        .collect();
    let first: Value = serde_json::from_str(&requests[0].body).unwrap();
    let batch_offered = first["tools"].as_array().is_some_and(|tools| {
        tools
            .iter()
            .any(|tool| tool["function"]["name"] == "mcp_batch")
    });
    let last: Value = serde_json::from_str(&requests.last().unwrap().body).unwrap();
    let tool_texts = last["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "tool")
        .map(|message| message["content"].as_str().unwrap_or_default().to_owned())
        .collect();
    let entries = read_dispatches(&log);
    let first_start = entries
        .iter()
        .map(|entry| entry["start"].as_f64().unwrap())
        .fold(f64::MAX, f64::min);
    let last_end = entries
        .iter()
        .map(|entry| entry["end"].as_f64().unwrap())
        .fold(0.0, f64::max);
    Measured {
        generations: requests.len(),
        request_bytes: requests.iter().map(|request| request.body.len()).sum(),
        dispatch_keys: entries
            .iter()
            .map(|entry| entry["key"].as_str().unwrap().to_owned())
            .collect(),
        dispatch_span_ms: ((last_end - first_start) * 1000.0) as u128,
        tool_texts,
        batch_offered,
        driver_elapsed_ms: std::fs::read_to_string(workspace.path().join("elapsed_ms"))
            .unwrap()
            .trim()
            .parse()
            .unwrap(),
    }
}

struct StdioFixture {
    home: tempfile::TempDir,
    workspace: tempfile::TempDir,
}

impl StdioFixture {
    fn new(scenario: &str, latency: Duration) -> Self {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let server = workspace.path().join("lookup-server.py");
        std::fs::write(&server, SERVER).unwrap();
        let log = workspace.path().join("dispatch.log");
        std::fs::write(&log, "").unwrap();
        let python = if cfg!(windows) {
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                .map(|directory| directory.join("python.exe"))
                .find(|candidate| candidate.is_file())
                .map_or_else(
                    || "python.exe".to_owned(),
                    |path| path.display().to_string(),
                )
        } else {
            "/usr/bin/python3".to_owned()
        };
        // A Windows interpreter cannot start without the system root.
        let inherit = if cfg!(windows) {
            "inherit_env=['SystemRoot','WINDIR']\n"
        } else {
            ""
        };
        std::fs::write(
        home.path().join("config.toml"),
        format!(
            "sandbox = 'off'\n[mcp.servers.fixture]\ntransport='stdio'\ncommand={python:?}\n{inherit}args=[{:?}, {:?}, {:?}, {scenario:?}]\n",
            server.display().to_string(),
            log.display().to_string(),
            latency.as_secs_f64().to_string(),
        ),
    )
    .unwrap();
        Self { home, workspace }
    }

    fn child(&self, stub: &Stub, test: &str, arm: &str) {
        let output = self.run_child(stub, test, arm);
        assert!(
            output.status.success(),
            "{arm}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    }

    fn run_child(&self, stub: &Stub, test: &str, arm: &str) -> std::process::Output {
        let Self { home, workspace } = self;
        std::process::Command::new(std::env::current_exe().unwrap())
            .env_clear()
            .envs(support::coverage_environment())
            .envs(support::windows_system_environment())
            .env("PATH", support::fixture_path())
            .env("RUST_MIN_STACK", "16777216")
            .env("EGO_LOG", "warn")
            .env("TMPDIR", std::env::temp_dir())
            .env("TEMP", std::env::temp_dir())
            .env("TMP", std::env::temp_dir())
            .env("USERPROFILE", home.path())
            .env("HOME", home.path())
            .env("EGO_HOME", home.path())
            .env("EGO_CREDENTIAL_BACKEND", "file")
            .env("OLLAMA_HOST", &stub.address)
            .env(CHILD, arm)
            .current_dir(workspace.path())
            .args([
                "--exact",
                test.strip_prefix(concat!(env!("CARGO_CRATE_NAME"), "::"))
                    .expect("qualified test name"),
                "--nocapture",
            ])
            .output()
            .unwrap()
    }
}

fn read_dispatches(log: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(log)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn record(measured: &Measured) -> Value {
    json!({
        "provider_requests": measured.generations,
        "provider_request_bytes": measured.request_bytes,
        "mcp_dispatch_order": measured.dispatch_keys,
        "mcp_dispatch_span_ms": measured.dispatch_span_ms,
        "tool_result_messages": measured.tool_texts.len(),
        "batch_tool_offered": measured.batch_offered,
        "driver_elapsed_ms": measured.driver_elapsed_ms,
        "latency_ms_per_provider_generation_and_per_lookup": LATENCY.as_millis(),
    })
}

// Catches: the batch changing which lookups run, their order, or what the
// model reads back; or failing to save provider generations (4 serial vs 2).
#[test]
fn serial_and_batched_lookups_match_with_two_provider_requests_instead_of_four() {
    if std::env::var_os(CHILD).is_none() {
        let serial = run_arm("serial");
        let batched = run_arm("batch");
        if let Some(out) = std::env::var_os("EGO_MCP_BATCH_REPORT") {
            let report = json!({ "serial": record(&serial), "batch": record(&batched) });
            std::fs::write(out, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
        }
        assert!(
            !serial.batch_offered,
            "the baseline offered mcp_batch: {serial:?}"
        );
        assert!(
            batched.batch_offered,
            "the batch arm did not offer mcp_batch: {batched:?}"
        );
        for run in [&serial, &batched] {
            assert_eq!(run.dispatch_keys, KEYS, "{run:?}");
        }
        assert_eq!(serial.generations, 4, "{serial:?}");
        assert_eq!(batched.generations, 2, "{batched:?}");
        assert_eq!(serial.tool_texts.len(), 3, "{serial:?}");
        assert_eq!(batched.tool_texts.len(), 1, "{batched:?}");
        let serial_all = serial.tool_texts.join("\n");
        for text in [&serial_all, &batched.tool_texts[0]] {
            let found: Vec<usize> = KEYS
                .iter()
                .map(|key| {
                    text.find(&format!("value-of-{key}"))
                        .unwrap_or_else(|| panic!("value-of-{key} missing in {text}"))
                })
                .collect();
            assert!(found.windows(2).all(|pair| pair[0] < pair[1]), "{text}");
        }
        return;
    }
    let enable_batch = match std::env::var(CHILD).as_deref() {
        Ok("serial") => false,
        Ok("batch") => true,
        other => panic!("unknown measurement arm: {other:?}"),
    };
    super::headless_driver::TEST_MCP_BATCH.store(enable_batch, Ordering::SeqCst);
    runtime().block_on(async {
        let crate::cli::Invocation::Interactive(args) = crate::cli::parse(
            [
                "ego",
                "--model",
                "ollama/qwen3",
                "--thinking",
                "off",
                "--sandbox",
                "off",
                "--trust-workspace",
                "--mode",
                "yolo",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        )
        .unwrap() else {
            panic!("interactive arguments")
        };
        let mut args = *args;
        args.prompt = Some("Look up alpha, bravo and charlie, then answer.".to_owned());
        let started = Instant::now();
        let ended = run_with_registry(
            args,
            |_| &NullSink,
            CancelToken::new(),
            Arc::new(EvidenceAdmissionService::new()),
            None,
        )
        .await;
        std::fs::write("elapsed_ms", started.elapsed().as_millis().to_string()).unwrap();
        assert_eq!(ended.termination.exit_code(), 0, "{:?}", ended.termination);
    });
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn fixture_args() -> Vec<String> {
    [
        "ego",
        "--model",
        "ollama/qwen3",
        "--thinking",
        "off",
        "--sandbox",
        "off",
        "--trust-workspace",
        "--mode",
        "yolo",
    ]
    .map(str::to_owned)
    .to_vec()
}
