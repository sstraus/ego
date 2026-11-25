use super::*;

const HELD_HEAD: &str = "MODEL-CONTROL-HELD";
const PARENT_TAIL: &str = "MODEL-CONTROL-PARENT-FINISHED";
const NEXT_REPLY: &str = "MODEL-CONTROL-NEXT-GENERATION";
const RECEIPT: &str = "Model: ollama/terra (this session only)";
const TERRA: &str = r#"
[model."ollama/terra".capabilities]
schema_version = 6
max_context_tokens = 8192
max_output_tokens = 2048
supports_parallel_tool_calls = true
reasoning_carryover = "unsupported"
reasoning_efforts = "any"
default_reasoning_effort = "off"
cancel = "cooperative"
cache = "none"
transport = "sse"
"#;

/// Unwind guard: opens the provider gate and the fixture-owned hook release
/// file, so a failed assertion ends the held hook and generation at once
/// instead of after the hook's 60 s timeout. Declare it after the PTY tuple so
/// it drops first, while the stub, the home directory and the child still live.
/// A failed release is printed, never swallowed (a drop must not panic).
struct ReleaseOnDrop {
    gate: Arc<ReleaseGate>,
    hook_release: std::path::PathBuf,
}
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.gate.release();
        if let Err(error) = std::fs::write(&self.hook_release, b"") {
            eprintln!(
                "fixture cleanup could not release the held hook at {}: {error}",
                self.hook_release.display()
            );
        }
    }
}

impl Fixture {
    fn release_on_drop(&self) -> ReleaseOnDrop {
        ReleaseOnDrop {
            gate: Arc::clone(&self.gate),
            hook_release: self.release.clone(),
        }
    }
}

/// Every durable selection event as `(model, thinking)`, strictly decoded.
fn selections(journal: &std::path::Path) -> Vec<(String, String)> {
    std::fs::read_to_string(journal)
        .expect("read session journal")
        .lines()
        .filter_map(|line| {
            let record: serde_json::Value =
                serde_json::from_str(line).expect("journal record is JSON");
            let event = record["payload"].get("event")?.clone();
            let event: ego_core::event::domain::DomainEvent =
                serde_json::from_value(event).expect("journal event decodes");
            let ego_core::event::domain::EventKind::SessionSelectionsSelected { selections } =
                event.kind
            else {
                return None;
            };
            let value = serde_json::to_value(selections).expect("selections encode");
            let field = |name: &str| {
                value[name]
                    .as_str()
                    .unwrap_or_else(|| panic!("selection {name} is a string: {value}"))
                    .to_owned()
            };
            Some((field("model"), field("thinking")))
        })
        .collect()
}

struct Fixture {
    stub: Stub,
    ego: Ego,
    gate: Arc<ReleaseGate>,
    started: std::path::PathBuf,
    release: std::path::PathBuf,
    journal: std::path::PathBuf,
    config: std::path::PathBuf,
}

/// A held first generation, a PreModelSwitch hook that is held by a file or
/// answers with `verdict`, and a second ordinary generation.
fn fixture(verdict: Option<&str>) -> Fixture {
    let gate = ReleaseGate::new();
    let Reply::Paced { head, .. } = paced_line(&format!("{HELD_HEAD}\n"), Duration::ZERO) else {
        unreachable!()
    };
    let stub = Stub::start(Plan {
        installed: vec!["qwen3".to_owned(), "terra".to_owned()],
        completions: vec![
            Reply::GatedPaced {
                head,
                tail: text(PARENT_TAIL),
                gate: Arc::clone(&gate),
            },
            Reply::Sse(text(NEXT_REPLY)),
        ],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--thinking",
        "low",
        "--sandbox",
        "off",
        "--trust-workspace",
        "--allow",
        "exec(bash *)",
        "--session-id",
        support::FIXTURE_SESSION_ID,
    ]);
    let started = ego.home_path().join("pre-switch-started");
    let release = ego.home_path().join("pre-switch-release");
    let command = match verdict {
        None => format!(
            "cat >/dev/null; : > '{}'; while [ ! -e '{}' ]; do sleep 0.05; done",
            started.display(),
            release.display()
        ),
        Some(verdict) => format!(
            "cat >/dev/null; : > '{}'; printf '%s' '{verdict}'",
            started.display()
        ),
    };
    ego.home_file("models.toml", TERRA);
    ego.home_file(
        "config.toml",
        &format!(
            "[[hooks]]\nevent = \"PreModelSwitch\"\n[hooks.handler]\ntype = \"command\"\nshell = \"bash\"\ncommand = {command:?}\ntimeout = 60\n"
        ),
    );
    let journal = ego
        .home_path()
        .join("sessions")
        .join(support::FIXTURE_SESSION_ID)
        .join("events.jsonl");
    let config = ego.home_path().join("config.toml");
    Fixture {
        stub,
        ego,
        gate,
        started,
        release,
        journal,
        config,
    }
}

fn until_file(reader: &mpsc::Receiver<Vec<u8>>, output: &mut Vec<u8>, path: &std::path::Path) {
    let deadline = Instant::now() + PROGRESS_DEADLINE;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        if let Ok(bytes) = reader.recv_timeout(Duration::from_millis(20)) {
            output.extend(bytes);
        }
    }
}

fn until_generation(
    reader: &mpsc::Receiver<Vec<u8>>,
    output: &mut Vec<u8>,
    stub: &Stub,
    count: usize,
) {
    let deadline = Instant::now() + PROGRESS_DEADLINE;
    while stub.generations().len() < count {
        assert!(
            Instant::now() < deadline,
            "generation {count} never started"
        );
        if let Ok(bytes) = reader.recv_timeout(Duration::from_millis(20)) {
            output.extend(bytes);
        }
    }
}

fn model_and_effort(body: &serde_json::Value) -> (String, String) {
    let effort = ["reasoning_effort", "think", "reasoning"]
        .iter()
        .find_map(|key| body.get(*key).map(|value| value.to_string()))
        .unwrap_or_default();
    (body["model"].to_string(), effort)
}

/// Choose terra with the first listed effort for this session only, through the
/// real picker opened while the turn is busy.
fn pick_terra(
    input: &mut dyn std::io::Write,
    reader: &mpsc::Receiver<Vec<u8>>,
    output: &mut Vec<u8>,
) {
    let mut send = |bytes: &[u8]| {
        input.write_all(bytes).unwrap();
        input.flush().unwrap();
    };
    let start = output.len();
    send(b"/model\r");
    until_normalized(reader, output, start, "Esc cancels");
    send(b"terra");
    until_normalized(reader, output, start, "filter: terra");
    send(b"\r");
    until_normalized(reader, output, start, "Select reasoning for ollama/terra");
    send(b"1");
    send(b"s");
}

// Catches: `/model` typed during a held reply is queued to turn end, blocks the
// captured parent while its PreModelSwitch hook waits, publishes or saves the
// selection before the hook returns, alters the captured request, or fails to
// feed the chosen model and effort to the next actual generation.
#[test]
fn native_model_switch_waits_on_held_hook_while_captured_parent_completes() {
    let f = fixture(None);
    let config_before = std::fs::read(&f.config).expect("read config");
    let (session, mut input, reader) = PtySession::start(&f.ego, 40, 120, Some((40, 120)));
    let _release = f.release_on_drop();
    let mut output = Vec::new();
    until_prompt_from(&reader, &mut output, 0);
    let baseline = selections(&f.journal);
    input
        .write_all(b"hold the reply while switching\r")
        .unwrap();
    input.flush().unwrap();
    until(&reader, &mut output, HELD_HEAD.as_bytes());
    until_generation(&reader, &mut output, &f.stub, 1);
    let captured = f.stub.generations()[0].clone();

    pick_terra(&mut input, &reader, &mut output);
    until_file(&reader, &mut output, &f.started);
    // The hook is held. The captured parent must still progress to completion.
    let start = output.len();
    f.gate.release();
    until_normalized(&reader, &mut output, start, PARENT_TAIL);
    assert!(
        !normalized(&output[start..]).contains(RECEIPT),
        "the selection was reported before the held hook returned"
    );
    assert_eq!(
        selections(&f.journal),
        baseline,
        "no durable selection before the hook returns"
    );
    assert_eq!(std::fs::read(&f.config).unwrap(), config_before);
    assert_eq!(f.stub.generations().len(), 1, "no extra generation");

    let start = output.len();
    std::fs::write(&f.release, b"").expect("release hook");
    until_normalized(&reader, &mut output, start, RECEIPT);
    until_prompt_from(&reader, &mut output, start);
    let committed = selections(&f.journal);
    assert_eq!(committed.len(), baseline.len() + 1, "{committed:?}");
    assert_eq!(
        committed.last().map(|(model, _)| model.as_str()),
        Some("ollama/terra")
    );
    assert_eq!(
        committed.last().map(|(_, thinking)| thinking.as_str()),
        Some("off")
    );
    assert_eq!(
        std::fs::read(&f.config).unwrap(),
        config_before,
        "session-only"
    );

    let start = output.len();
    input.write_all(b"use the new selection\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, NEXT_REPLY);
    let generations = f.stub.generations();
    assert_eq!(generations.len(), 2);
    assert_eq!(generations[0], captured, "captured request is unchanged");
    let (old_model, old_effort) = model_and_effort(&generations[0]);
    let (new_model, new_effort) = model_and_effort(&generations[1]);
    assert!(
        new_model.contains("terra") && old_model.contains("qwen3"),
        "{old_model} {new_model}"
    );
    assert_ne!(
        old_effort, new_effort,
        "next generation carries the new effort"
    );
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
}

// Catches: interrupting the captured turn cancels an already accepted model
// switch (the turn token replacing the run-level owner token) or leaves its
// settlement unobserved; the commit must follow the hook, not the turn.
#[test]
fn turn_interrupt_does_not_cancel_an_accepted_model_switch() {
    let f = fixture(None);
    let (session, mut input, reader) = PtySession::start(&f.ego, 40, 120, Some((40, 120)));
    let _release = f.release_on_drop();
    let mut output = Vec::new();
    until_prompt_from(&reader, &mut output, 0);
    let baseline = selections(&f.journal);
    input
        .write_all(b"hold the reply while switching\r")
        .unwrap();
    input.flush().unwrap();
    until(&reader, &mut output, HELD_HEAD.as_bytes());
    until_generation(&reader, &mut output, &f.stub, 1);
    input.write_all(b"/model terra\r").unwrap();
    input.flush().unwrap();
    until_file(&reader, &mut output, &f.started);

    let start = output.len();
    input.write_all(b"\x03").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "cancelling active work");
    assert_eq!(
        selections(&f.journal),
        baseline,
        "held hook, nothing published"
    );

    let start = output.len();
    std::fs::write(&f.release, b"").expect("release hook");
    until_normalized(&reader, &mut output, start, RECEIPT);
    let committed = selections(&f.journal);
    assert_eq!(committed.len(), baseline.len() + 1, "{committed:?}");
    assert_eq!(
        committed.last().map(|(model, _)| model.as_str()),
        Some("ollama/terra")
    );
    f.gate.release();
    input.write_all(b"\x15/quit\r").unwrap();
    input.flush().unwrap();
    // The session exit carries its last turn, which Ctrl+C interrupted: 130
    // (CLI.md exit table; the same status `busy_interrupt_raw` asserts).
    let status = session.quit(input);
    assert_eq!(status.exit_code(), 130, "REPL exit: {status:?}");
}

// Catches: a hook veto during a busy switch still publishes the selection, or
// the next generation uses the refused model.
#[test]
fn blocked_busy_model_switch_publishes_nothing_and_next_generation_keeps_the_model() {
    let f = fixture(Some(
        r#"{"version":1,"decision":"block","feedback":"model switch vetoed"}"#,
    ));
    let (session, mut input, reader) = PtySession::start(&f.ego, 40, 120, Some((40, 120)));
    let _release = f.release_on_drop();
    let mut output = Vec::new();
    until_prompt_from(&reader, &mut output, 0);
    let baseline = selections(&f.journal);
    input
        .write_all(b"hold the reply while switching\r")
        .unwrap();
    input.flush().unwrap();
    until(&reader, &mut output, HELD_HEAD.as_bytes());
    until_generation(&reader, &mut output, &f.stub, 1);
    let start = output.len();
    input.write_all(b"/model terra\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "model switch failed");
    assert!(!normalized(&output[start..]).contains(RECEIPT));
    assert_eq!(selections(&f.journal), baseline, "a veto publishes nothing");

    let start = output.len();
    f.gate.release();
    until_normalized(&reader, &mut output, start, PARENT_TAIL);
    until_prompt_from(&reader, &mut output, start);
    let start = output.len();
    input.write_all(b"still the first model\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, NEXT_REPLY);
    let generations = f.stub.generations();
    assert_eq!(generations.len(), 2);
    assert_eq!(
        model_and_effort(&generations[0]),
        model_and_effort(&generations[1])
    );
    assert_eq!(selections(&f.journal), baseline);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
}
