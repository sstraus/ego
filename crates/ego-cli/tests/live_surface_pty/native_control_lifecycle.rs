use super::*;
use ego_core::event::domain::{DomainEvent, EventKind};

struct ReleaseOnDrop(Vec<Arc<ReleaseGate>>);
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        for gate in &self.0 {
            gate.release();
        }
    }
}

fn held_reply(head: &str, final_text: &str) -> (Reply, Arc<ReleaseGate>) {
    let (Reply::GatedPaced { head, gate, .. }, owner) = gated_paced_line(head) else {
        unreachable!()
    };
    (
        Reply::GatedPaced {
            head,
            tail: text(final_text),
            gate,
        },
        owner,
    )
}

fn journal_events(home: &Path, session: &str) -> Vec<DomainEvent> {
    let path = home.join("sessions").join(session).join("events.jsonl");
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        // A concurrent append can leave the last line incomplete temporarily.
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|record| record["payload"].get("event").cloned())
        .map(|event| serde_json::from_value(event).unwrap())
        .collect()
}

fn wait_observation(
    reader: &std::sync::mpsc::Receiver<Vec<u8>>,
    output: &mut Vec<u8>,
    description: &str,
    mut ready: impl FnMut() -> bool,
) {
    let deadline = Instant::now() + PROGRESS_DEADLINE;
    while !ready() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "{description}: {}",
            normalized(output)
        );
        match reader.recv_timeout(remaining.min(Duration::from_millis(50))) {
            Ok(bytes) => output.extend(bytes),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(error) => panic!("{description}: {error}"),
        }
    }
}

// Catches: busy native cancellation only acknowledges a request, cancels the
// parent/sibling, or cannot settle a real child until the parent is released.
#[test]
fn selected_real_model_child_settles_while_parent_and_sibling_remain_held() {
    let (parent_reply, parent_gate) = held_reply("PARENT-1169-HELD", "PARENT-1169-DONE");
    let (selected_reply, selected_gate) = held_reply("SELECTED-1169-HELD", "SELECTED-1169-DONE");
    let (sibling_reply, sibling_gate) = held_reply("SIBLING-1169-HELD", "SIBLING-1169-DONE");
    let launch = |id: &str, name: &str, prompt: &str| {
        Reply::Sse(tool_call(
            id,
            "task",
            &serde_json::json!({
                "name": name, "prompt": prompt, "background": true
            })
            .to_string(),
        ))
    };
    let stub = Stub::start_concurrent(Plan {
        completions: vec![
            launch("selected-initial", "Selected lifecycle", "SELECTED-1169"),
            launch("sibling-initial", "Sibling lifecycle", "SIBLING-1169"),
            parent_reply,
            Reply::Sse(text("FOLLOWUP-1169-DONE")),
        ],
        prompt_completions: vec![
            ("SELECTED-1169".into(), vec![selected_reply]),
            ("SIBLING-1169".into(), vec![sibling_reply]),
        ],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "off",
        "--mode",
        "yolo",
        "--trust-workspace",
        "--session-id",
        support::FIXTURE_SESSION_ID,
    ]);
    let (session, mut input, reader) = PtySession::start(&ego, 40, 120, Some((40, 120)));
    let _release = ReleaseOnDrop(vec![
        parent_gate.clone(),
        selected_gate.clone(),
        sibling_gate.clone(),
    ]);
    let mut output = Vec::new();
    until_prompt_from(&reader, &mut output, 0);
    input
        .write_all(b"launch both lifecycle children\r")
        .unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, 0, "PARENT-1169-HELD");
    wait_observation(&reader, &mut output, "both real child requests", || {
        let requests = stub.generations();
        ["SELECTED-1169", "SIBLING-1169"].iter().all(|marker| {
            requests.iter().any(|r| {
                // Prompt routing is exact first-user text, not a parent tool argument.
                r["messages"].as_array().is_some_and(|messages| {
                    messages
                        .iter()
                        .any(|m| m["role"] == "user" && m["content"] == *marker)
                })
            })
        })
    });
    let spawned = journal_events(ego.home_path(), support::FIXTURE_SESSION_ID)
        .into_iter()
        .filter_map(|event| match event.kind {
            EventKind::SubagentSpawned { invocation, child } => Some((invocation, child)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(spawned.len(), 2, "exactly two admitted real children");
    let (selected, selected_run) = spawned[0];
    let (_, sibling_run) = spawned[1];
    let mark = output.len();
    input
        .write_all(format!("/tasks cancel {selected}\r").as_bytes())
        .unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, mark, "cancellation requested");
    let child_events =
        |run: ego_core::run::RunId| journal_events(ego.home_path(), &run.session_id().to_string());
    wait_observation(
        &reader,
        &mut output,
        "selected child durable cancellation",
        || {
            child_events(selected_run).iter().any(|event| {
                matches!(
                    event.kind,
                    EventKind::TurnCompleted {
                        exit_reason: ego_core::event::domain::ExitReason::Cancelled(_),
                        ..
                    }
                )
            })
        },
    );
    assert!(
        !child_events(sibling_run)
            .iter()
            .any(|event| matches!(event.kind, EventKind::TurnCompleted { .. })),
        "sibling settled while held"
    );
    assert!(
        !journal_events(ego.home_path(), support::FIXTURE_SESSION_ID)
            .iter()
            .any(|event| matches!(event.kind, EventKind::TurnCompleted { .. })),
        "parent settled while held"
    );
    sibling_gate.release();
    parent_gate.release();
    selected_gate.release();
    until_normalized(&reader, &mut output, mark, "PARENT-1169-DONE");
    until_mcp_composer_is_idle(&reader, &mut output);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    let selected_events = child_events(selected_run);
    assert_eq!(
        selected_events
            .iter()
            .filter(|event| matches!(event.kind, EventKind::TurnCompleted { .. }))
            .count(),
        1
    );
    assert!(!selected_events.iter().any(|event| matches!(&event.kind, EventKind::AssistantMessage { text } if text.contains("SELECTED-1169-DONE"))));
}

/// One real unauthenticated HTTP probe whose response the test owns. Shutdown
/// releases both the accepted socket and a listener not yet reached on failure.
struct HeldProbe {
    address: std::net::SocketAddr,
    arrived: std::sync::mpsc::Receiver<String>,
    release: std::sync::mpsc::Sender<()>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl HeldProbe {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (arrive, arrived) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            // The production probe first opens/closes a TCP preflight socket,
            // then opens the actual HTTP exchange. Only GET reaches the gate.
            for _ in 0..3 {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") && request.len() < 8192 {
                    if socket.read(&mut byte).unwrap_or(0) == 0 {
                        break;
                    }
                    request.push(byte[0]);
                }
                if request.is_empty() {
                    if released.try_recv().is_ok() {
                        return;
                    }
                    continue;
                }
                let _ = arrive.send(String::from_utf8(request).unwrap());
                let _ = released.recv_timeout(Duration::from_secs(10));
                let _ = socket.write_all(
                    b"HTTP/1.1 418 Fixture\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                return;
            }
        });
        Self {
            address,
            arrived,
            release,
            worker: Some(worker),
        }
    }
    fn release(&self) {
        let _ = self.release.send(());
    }
}
impl Drop for HeldProbe {
    fn drop(&mut self) {
        self.release();
        let _ = std::net::TcpStream::connect(self.address);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

// Catches: joining a blocking doctor probe starves the separately ready parent.
#[test]
fn parent_finishes_while_real_doctor_probe_remains_held() {
    doctor_completion_order(true);
}

// Catches: doctor completion cancels, restarts, or waits for the held parent.
#[test]
fn real_doctor_probe_finishes_while_parent_remains_held() {
    doctor_completion_order(false);
}

fn doctor_completion_order(parent_first: bool) {
    let probe = HeldProbe::start();
    let (reply, gate) = held_reply("DOCTOR-PARENT-1169-HELD", "DOCTOR-PARENT-1169-DONE");
    let stub = Stub::start(Plan {
        completions: vec![reply],
        ..Plan::default()
    });
    let ego = Ego::against(&stub)
        .conversing()
        .env("ANTHROPIC_API_KEY", "local-fixture-not-a-credential")
        .env("ANTHROPIC_BASE_URL", &format!("http://{}", probe.address))
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--trust-workspace",
            "--session-id",
            support::FIXTURE_SESSION_ID,
        ]);
    ego.home_file("config.toml", "doctor_network = true\n");
    let (session, mut input, reader) = PtySession::start(&ego, 40, 120, Some((40, 120)));
    let _release = ReleaseOnDrop(vec![gate.clone()]);
    let mut output = Vec::new();
    until_prompt_from(&reader, &mut output, 0);
    input.write_all(b"hold doctor parent\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, 0, "DOCTOR-PARENT-1169-HELD");
    let mark = output.len();
    input.write_all(b"/doctor details\r").unwrap();
    input.flush().unwrap();
    let request = probe
        .arrived
        .recv_timeout(Duration::from_secs(3))
        .expect("real doctor HTTP request");
    assert!(request.starts_with("GET "), "{request}");
    assert!(
        !request.to_ascii_lowercase().contains("authorization:"),
        "probe sent a credential"
    );
    if parent_first {
        gate.release();
        until_normalized(&reader, &mut output, mark, "DOCTOR-PARENT-1169-DONE");
        wait_observation(
            &reader,
            &mut output,
            "parent durable completion before probe release",
            || {
                journal_events(ego.home_path(), support::FIXTURE_SESSION_ID)
                    .iter()
                    .any(|event| matches!(event.kind, EventKind::TurnCompleted { .. }))
            },
        );
        assert!(!normalized(&output[mark..]).contains("HTTP 418"));
        probe.release();
    } else {
        probe.release();
        until_normalized(&reader, &mut output, mark, "HTTP 418");
        assert!(
            !journal_events(ego.home_path(), support::FIXTURE_SESSION_ID)
                .iter()
                .any(|event| matches!(event.kind, EventKind::TurnCompleted { .. })),
            "doctor completed the held parent"
        );
        gate.release();
    }
    until_normalized(&reader, &mut output, mark, "HTTP 418");
    until_normalized(&reader, &mut output, mark, "DOCTOR-PARENT-1169-DONE");
    until_mcp_composer_is_idle(&reader, &mut output);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert_eq!(
        normalized(&output[mark..]).matches("HTTP 418").count(),
        1,
        "duplicate doctor receipt"
    );
    assert_eq!(stub.generations().len(), 1, "doctor restarted the parent");
}
