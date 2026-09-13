mod support;

use ego_cli::repl::actions::ActionRegistry;
use portable_pty::{NativePtySystem, PtySize, PtySystem};
use std::{
    io::{self, Read, Write},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use support::{Ego, Plan, Reply, Stub, gated_paced_tool_call_line, paced, text, tool_call};

// portable-pty transports bytes but does not emulate a terminal. Keep the
// reported cursor inside both 80x24 and 12x8; painter geometry is proven by
// the vendor unit test, while this test needs only a live DSR response.
const DSR_QUERY: &[u8] = b"\x1b[6n";
const DSR_RESPONSE: &[u8] = b"\x1b[4;1R";

/// How long every wait below gives the child to make progress.
///
/// One name for what was ten copies of the same eight seconds. Each use is a
/// liveness bailout and none is a pass condition: a wait that expires panics,
/// and the one deadline that does not — the post-EOF exit wait — kills the
/// child and then asserts on a status a killed child cannot have. So the value
/// only decides how long a genuine hang takes to become legible, and raising
/// it cannot turn a failing run green.
///
/// The gap it must clear is the child's, measured while the whole workspace's
/// test binaries run at once rather than on an idle machine. Eight seconds was
/// the idle figure: `live_surface_esc_expedites_sampling_once_and_reports_interrupt_state`
/// passes alone and failed under `cargo test --workspace` having seen only the
/// two idle prompt renders, on a startup gap rather than on anything it
/// asserts.
const PROGRESS_DEADLINE: Duration = Duration::from_secs(60);

enum WriteRequest {
    Bytes(Vec<u8>),
    Flush,
}

struct PtyWriter {
    sender: mpsc::SyncSender<WriteRequest>,
}

impl Write for PtyWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.sender
            .send(WriteRequest::Bytes(bytes.to_vec()))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "PTY writer stopped"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.sender
            .send(WriteRequest::Flush)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "PTY writer stopped"))
    }
}

fn writer_owner(
    mut writer: Box<dyn Write + Send>,
) -> (
    PtyWriter,
    mpsc::SyncSender<WriteRequest>,
    thread::JoinHandle<()>,
) {
    let (sender, requests) = mpsc::sync_channel(32);
    let responder = sender.clone();
    let join = thread::spawn(move || {
        for request in requests {
            let result = match request {
                WriteRequest::Bytes(bytes) => writer.write_all(&bytes),
                WriteRequest::Flush => writer.flush(),
            };
            if result.is_err() {
                break;
            }
        }
    });
    (PtyWriter { sender }, responder, join)
}

fn stream(
    reader: Box<dyn Read + Send>,
    dsr_responder: mpsc::SyncSender<WriteRequest>,
) -> (mpsc::Receiver<Vec<u8>>, thread::JoinHandle<()>) {
    let (sender, receiver) = mpsc::channel();
    let join = thread::spawn(move || {
        let mut reader = reader;
        let mut chunk = [0; 1024];
        let mut pending = Vec::new();
        loop {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(length) => {
                    pending.extend_from_slice(&chunk[..length]);
                    while let Some(index) = pending
                        .windows(DSR_QUERY.len())
                        .position(|window| window == DSR_QUERY)
                    {
                        pending.drain(..index + DSR_QUERY.len());
                        if dsr_responder
                            .send(WriteRequest::Bytes(DSR_RESPONSE.to_vec()))
                            .is_err()
                            || dsr_responder.send(WriteRequest::Flush).is_err()
                        {
                            return;
                        }
                    }
                    if pending.len() > DSR_QUERY.len() - 1 {
                        let keep = DSR_QUERY.len() - 1;
                        pending.drain(..pending.len() - keep);
                    }
                    if sender.send(chunk[..length].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    (receiver, join)
}

fn until(receiver: &mpsc::Receiver<Vec<u8>>, output: &mut Vec<u8>, needle: &[u8]) {
    until_with_context(receiver, output, needle, String::new);
}

fn until_from(
    receiver: &mpsc::Receiver<Vec<u8>>,
    output: &mut Vec<u8>,
    start: usize,
    needle: &[u8],
) {
    while !output[start..]
        .windows(needle.len())
        .any(|window| window == needle)
    {
        let chunk = receiver
            .recv_timeout(PROGRESS_DEADLINE)
            .unwrap_or_else(|error| {
                drain(receiver, output);
                panic!(
                    "raw PTY wait failed for needle {:?}: {error}; output: {:?}",
                    String::from_utf8_lossy(needle),
                    String::from_utf8_lossy(&output[start..]),
                )
            });
        output.extend(chunk);
    }
}

fn until_with_context(
    receiver: &mpsc::Receiver<Vec<u8>>,
    output: &mut Vec<u8>,
    needle: &[u8],
    context: impl Fn() -> String,
) {
    while !output.windows(needle.len()).any(|window| window == needle) {
        let chunk = match receiver.recv_timeout(PROGRESS_DEADLINE) {
            Ok(chunk) => chunk,
            Err(error) => {
                drain(receiver, output);
                panic!(
                    "semantic PTY wait failed for needle {:?}: {error}; context: {}; raw output: {:?}; normalized output: {:?}",
                    String::from_utf8_lossy(needle),
                    context(),
                    String::from_utf8_lossy(output),
                    normalized(output),
                );
            }
        };
        output.extend(chunk);
    }
}

fn strip_csi(bytes: &[u8]) -> Vec<u8> {
    let mut plain = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if index + 1 < bytes.len()
            && bytes[index] == b'\x1b'
            && matches!(bytes[index + 1], b'7' | b'8')
        {
            index += 2;
            continue;
        }
        if index + 1 < bytes.len() && bytes[index] == b'\x1b' && bytes[index + 1] == b'[' {
            index += 2;
            while index < bytes.len() {
                let byte = bytes[index];
                index += 1;
                if (0x40..=0x7e).contains(&byte) {
                    break;
                }
            }
            continue;
        }
        plain.push(bytes[index]);
        index += 1;
    }
    plain
}

fn normalized(bytes: &[u8]) -> String {
    let plain = strip_csi(bytes);
    let text = String::from_utf8_lossy(&plain).replace("\r\n", "\n");
    text.split('\n')
        .map(|line| {
            line.strip_prefix("> ")
                .or_else(|| line.strip_prefix("… "))
                .unwrap_or(line)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn until_normalized(
    receiver: &mpsc::Receiver<Vec<u8>>,
    output: &mut Vec<u8>,
    start: usize,
    needle: &str,
) {
    while !normalized(&output[start..]).contains(needle) {
        let chunk = match receiver.recv_timeout(PROGRESS_DEADLINE) {
            Ok(chunk) => chunk,
            Err(error) => {
                drain(receiver, output);
                panic!(
                    "normalized PTY wait failed for needle {:?}: {error}; raw output: {:?}; normalized output: {:?}",
                    needle,
                    String::from_utf8_lossy(&output[start..]),
                    normalized(&output[start..]),
                );
            }
        };
        output.extend(chunk);
    }
}

fn until_normalized_or_generation(
    receiver: &mpsc::Receiver<Vec<u8>>,
    output: &mut Vec<u8>,
    start: usize,
    needle: &str,
    stub: &Stub,
) {
    while !normalized(&output[start..]).contains(needle) {
        let generations = stub.generations();
        if !generations.is_empty() {
            drain(receiver, output);
            panic!(
                "PTY input fell through to provider while waiting for {needle:?}; generations: {generations:?}; output: {:?}",
                normalized(&output[start..]),
            );
        }
        let chunk = receiver
            .recv_timeout(PROGRESS_DEADLINE)
            .unwrap_or_else(|error| {
                drain(receiver, output);
                panic!(
                    "PTY wait failed for {needle:?}: {error}; generations: {:?}; output: {:?}",
                    stub.generations(),
                    normalized(&output[start..]),
                )
            });
        output.extend(chunk);
    }
}

fn until_normalized_without_new_generation(
    receiver: &mpsc::Receiver<Vec<u8>>,
    output: &mut Vec<u8>,
    start: usize,
    needle: &str,
    stub: &Stub,
    generation_count: usize,
) {
    while !normalized(&output[start..]).contains(needle) {
        let generations = stub.generations();
        if generations.len() != generation_count {
            drain(receiver, output);
            panic!(
                "PTY input changed provider generation count while waiting for {needle:?}; expected {generation_count}, generations: {generations:?}; output: {:?}",
                normalized(&output[start..]),
            );
        }
        let chunk = receiver
            .recv_timeout(PROGRESS_DEADLINE)
            .unwrap_or_else(|error| {
                drain(receiver, output);
                panic!(
                    "PTY wait failed for {needle:?}: {error}; generations: {:?}; output: {:?}",
                    stub.generations(),
                    normalized(&output[start..]),
                )
            });
        output.extend(chunk);
    }
}

fn until_post_trust_prompt(receiver: &mpsc::Receiver<Vec<u8>>, output: &mut Vec<u8>, start: usize) {
    loop {
        let suffix = &output[start..];
        let Some(query) = suffix
            .windows(DSR_QUERY.len())
            .position(|window| window == DSR_QUERY)
        else {
            let chunk = receiver
                .recv_timeout(PROGRESS_DEADLINE)
                .expect("post-trust DSR query did not arrive");
            output.extend(chunk);
            continue;
        };
        let prompt_start = query + DSR_QUERY.len();
        if suffix[prompt_start..]
            .windows(2)
            .any(|window| window == b"> ")
        {
            return;
        }
        let chunk = receiver
            .recv_timeout(PROGRESS_DEADLINE)
            .expect("idle prompt did not follow the post-trust DSR query");
        output.extend(chunk);
    }
}

fn submit_local_action(
    receiver: &mpsc::Receiver<Vec<u8>>,
    output: &mut Vec<u8>,
    input: &mut impl Write,
    stub: &Stub,
    command: &str,
    expected: Option<&str>,
) {
    let start = output.len();
    let prompt_count = output.windows(2).filter(|window| *window == b"> ").count();
    input
        .write_all(format!("/{command}\r").as_bytes())
        .unwrap_or_else(|error| panic!("write /{command}: {error}"));
    input
        .flush()
        .unwrap_or_else(|error| panic!("flush /{command}: {error}"));
    if let Some(expected) = expected {
        until_normalized_without_new_generation(receiver, output, start, expected, stub, 0);
    }
    until_prompt_after(receiver, output, prompt_count);
    drain_until_quiet(receiver, output);
    assert!(
        stub.generations().is_empty(),
        "/{command} leaked into a provider generation"
    );
}

fn until_prompt_after(receiver: &mpsc::Receiver<Vec<u8>>, output: &mut Vec<u8>, previous: usize) {
    while output.windows(2).filter(|window| *window == b"> ").count() <= previous {
        let chunk = receiver
            .recv_timeout(PROGRESS_DEADLINE)
            .unwrap_or_else(|error| {
                drain(receiver, output);
                panic!(
                    "prompt count did not advance beyond {previous}: {error}; output: {:?}",
                    normalized(output),
                )
            });
        output.extend(chunk);
    }
}

fn drain(receiver: &mpsc::Receiver<Vec<u8>>, output: &mut Vec<u8>) {
    while let Ok(chunk) = receiver.try_recv() {
        output.extend(chunk);
    }
}

/// A raw PTY read may have already received a complete redraw while its reader
/// thread still holds the chunk. A key assertion begins only after that queue
/// is quiet, so an old marker cannot satisfy a later-key expectation.
fn drain_until_quiet(receiver: &mpsc::Receiver<Vec<u8>>, output: &mut Vec<u8>) {
    let deadline = Instant::now() + PROGRESS_DEADLINE;
    while let Ok(chunk) = receiver.recv_timeout(Duration::from_millis(100)) {
        output.extend(chunk);
        assert!(Instant::now() < deadline, "PTY redraw never became quiet");
    }
}

fn count_exact_strings(value: &serde_json::Value, target: &str) -> usize {
    match value {
        serde_json::Value::String(value) => usize::from(value == target),
        serde_json::Value::Array(values) => values
            .iter()
            .map(|value| count_exact_strings(value, target))
            .sum(),
        serde_json::Value::Object(values) => values
            .values()
            .map(|value| count_exact_strings(value, target))
            .sum(),
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => 0,
    }
}

fn count_notes_write_results(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::String(value) => {
            usize::from(value.starts_with("wrote 10 bytes to ") && value.ends_with("/notes.md"))
        }
        serde_json::Value::Array(values) => values.iter().map(count_notes_write_results).sum(),
        serde_json::Value::Object(values) => values.values().map(count_notes_write_results).sum(),
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => 0,
    }
}

fn line_framed(reply: Reply, content: &str) -> Reply {
    let Reply::Paced {
        mut head,
        tail,
        pause,
    } = reply
    else {
        return reply;
    };
    let encoded = serde_json::to_string(content).expect("the marker is serialisable");
    let framed = serde_json::to_string(&format!("{content}\n"))
        .expect("the line-framed marker is serialisable");
    head = head.replace(
        &format!("\"content\":{encoded}"),
        &format!("\"content\":{framed}"),
    );
    Reply::Paced { head, tail, pause }
}

fn paced_line(content: &str, pause: Duration) -> Reply {
    line_framed(paced(content, pause), content)
}

fn find(output: &[u8], needle: &[u8]) -> usize {
    output
        .windows(needle.len())
        .position(|window| window == needle)
        .unwrap_or_else(|| panic!("missing PTY marker {:?}", String::from_utf8_lossy(needle)))
}

#[test]
fn live_surface_preserves_draft_through_busy_permission_and_resize() {
    let older_draft = "fn older_draft() {\n    println!(\"older e\\u{301} 👩‍💻\");\n}\n";
    let draft = "fn newest_draft() {\n    println!(\"newest e\\u{301} 👩‍💻\");\n}\n";
    let write_arguments = serde_json::json!({
        "path": "notes.md",
        "contents": "permission",
    })
    .to_string();
    let (paced_tool_call, provider_gate) =
        gated_paced_tool_call_line("write-1", "write", &write_arguments);
    let stub = Stub::start(Plan {
        completions: vec![
            paced_tool_call,
            Reply::Sse(text("NEWEST-DRAFT-RECEIVED")),
            Reply::Sse(text("OLDER-DRAFT-RECEIVED")),
        ],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--mode",
        "default",
        "--sandbox",
        "off",
    ]);
    // These cases all begin by answering the trust question, so the workspace
    // has to hold something worth asking about. An empty checkout is asked
    // nothing at all.
    ego.file("AGENTS.md", "# live surface fixture\n");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let mut child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
    );
    let mut input = input;
    let mut output = Vec::new();

    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust answer flush");
    let idle_start = output.len();
    until_from(&reader, &mut output, idle_start, b"> ");

    input.write_all(b"\x1b[200~").expect("paste prefix");
    input
        .write_all(older_draft.as_bytes())
        .expect("paste older draft");
    input.write_all(b"\x1b[201~").expect("paste suffix");
    input.write_all(b"\x13").expect("stash older draft");
    input.flush().expect("stash older draft flush");
    let older_stash_start = output.len();
    until_from(&reader, &mut output, older_stash_start, b"> ");
    input.write_all(b"\x1b[200~").expect("paste prefix");
    input
        .write_all(draft.as_bytes())
        .expect("paste newest draft");
    input.write_all(b"\x1b[201~").expect("paste suffix");
    input.write_all(b"\x13").expect("stash newest draft");
    input.flush().expect("stash newest draft flush");
    let newest_stash_start = output.len();
    until_from(&reader, &mut output, newest_stash_start, b"> ");
    input.write_all(b"initial\r").expect("initial prompt");
    input.flush().expect("initial prompt flush");
    until(&reader, &mut output, b"STREAMED-BEFORE-TOOL");
    input
        .write_all(b"\x13")
        .expect("restore newest draft from LIFO stash during Busy");
    let newest_restore_start = output.len();
    input.flush().expect("restore newest draft flush");
    until_from(&reader, &mut output, newest_restore_start, b"newest_draft");
    input.write_all(b"\r").expect("busy steer submit");
    input.flush().expect("busy steer flush");
    until_normalized(
        &reader,
        &mut output,
        0,
        "Messages to be submitted after next tool call (press esc to interrupt and send immediately)",
    );
    provider_gate.release();
    until(&reader, &mut output, b"STREAMED-BEFORE-TOOL");
    until_with_context(&reader, &mut output, b"[y] allow [n] deny [e] edit", || {
        let requests = stub.requests();
        let paths: Vec<_> = requests
            .iter()
            .map(|request| request.path.clone())
            .collect();
        format!("stub request paths: {paths:?}")
    });
    let resize_start = output.len();
    pair.master
        .resize(PtySize {
            rows: 8,
            cols: 12,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("resize");
    until_from(
        &reader,
        &mut output,
        resize_start,
        b"[y] allow [n] deny [e] edit",
    );
    input.write_all(b"y").expect("immediate permission answer");
    input.flush().expect("permission answer flush");
    until(&reader, &mut output, b"NEWEST-DRAFT-RECEIVED");
    input
        .write_all(b"\x13")
        .expect("restore older draft from LIFO stash");
    let older_restore_start = output.len();
    input.flush().expect("restore older draft flush");
    until_from(&reader, &mut output, older_restore_start, b"older_draft");
    input.write_all(b"\r").expect("submit restored older draft");
    input.flush().expect("older draft submit flush");
    until(&reader, &mut output, b"OLDER-DRAFT-RECEIVED");
    let unsent_draft_start = output.len();
    input.write_all(b"unsent draft").expect("type unsent draft");
    input.flush().expect("unsent draft flush");
    until_from(&reader, &mut output, unsent_draft_start, b"unsent draft");
    let unsent_stash_start = output.len();
    input.write_all(b"\x13").expect("stash unsent draft");
    input.flush().expect("stash unsent draft flush");
    until_post_trust_prompt(&reader, &mut output, unsent_stash_start);
    input.write_all(b"\x04").expect("EOF");
    input.flush().expect("EOF flush");
    until_normalized(&reader, &mut output, 0, "1 unsent draft");

    drop(input);
    drop(pair);
    let mut killer = child.clone_killer();
    let (status_sender, status_receiver) = mpsc::channel();
    let waiter = thread::spawn(move || {
        let status = child.wait();
        status_sender.send(status).expect("status receiver dropped");
    });
    let deadline = Instant::now() + PROGRESS_DEADLINE;
    let status = loop {
        drain(&reader, &mut output);
        match status_receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(Ok(status)) => break status,
            Ok(Err(error)) => panic!("wait failed: {error}"),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("child waiter disconnected")
            }
            Err(mpsc::RecvTimeoutError::Timeout) if Instant::now() >= deadline => {
                drain(&reader, &mut output);
                killer.kill().expect("kill timed out child");
                match status_receiver
                    .recv_timeout(Duration::from_secs(2))
                    .expect("killed child did not exit")
                {
                    Ok(status) => break status,
                    Err(error) => panic!("wait after kill failed: {error}"),
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    };
    assert!(status.success(), "interactive process failed: {status:?}");
    waiter.join().expect("waiter thread panicked");
    reader_join.join().expect("reader thread panicked");
    writer_join.join().expect("writer thread panicked");
    drain(&reader, &mut output);

    assert!(!output.windows(8).any(|window| window == b"\x1b[?1049h"));
    assert!(!output.windows(8).any(|window| window == b"\x1b[?1000h"));
    assert!(!output.windows(8).any(|window| window == b"\x1b[?1006h"));
    assert!(output.windows(8).any(|window| window == b"\x1b[?2004h"));
    assert!(output.windows(8).any(|window| window == b"\x1b[?2004l"));

    let rendered = normalized(&output);
    for fact in [
        "Call: \"write\"",
        "Reason:",
        "Operations:",
        "Risk: Bounded",
        "Parameters:",
        "Remember would add:",
        "widening for operation 0",
    ] {
        assert!(
            rendered.contains(fact),
            "permission presentation omitted {fact:?}: {rendered}"
        );
    }

    let streamed = find(&output, b"STREAMED-BEFORE-TOOL");
    let permission = find(&output, b"[y] allow [n] deny [e] edit");
    let draft_received = find(&output, b"NEWEST-DRAFT-RECEIVED");
    let older_draft_received = find(&output, b"OLDER-DRAFT-RECEIVED");
    assert!(streamed < permission);
    assert!(permission < draft_received);
    assert!(draft_received < older_draft_received);

    let generations = stub.generations();
    assert_eq!(generations.len(), 3);
    let write_result_counts: Vec<_> = generations.iter().map(count_notes_write_results).collect();
    assert_eq!(
        write_result_counts,
        vec![0, 1, 1],
        "the tool result must reach each subsequent model request exactly once"
    );
    let draft_counts: Vec<_> = generations
        .iter()
        .map(|generation| count_exact_strings(generation, draft))
        .collect();
    assert_eq!(draft_counts, vec![0, 1, 1]);
    let older_draft_counts: Vec<_> = generations
        .iter()
        .map(|generation| count_exact_strings(generation, older_draft))
        .collect();
    assert_eq!(older_draft_counts, vec![0, 0, 1]);
    assert_eq!(
        generations
            .iter()
            .map(|generation| count_exact_strings(generation, "unsent draft"))
            .sum::<usize>(),
        0,
        "an unsent draft must never enter model history"
    );
    assert!(
        normalized(&output)
            .to_ascii_lowercase()
            .contains("1 unsent draft"),
        "exit must report the exact unsent-draft count rather than silently discarding it"
    );
}

#[cfg(unix)]
#[test]
fn interactive_exec_resolves_real_home_local_bin_from_effective_path() {
    use std::os::unix::fs::PermissionsExt as _;

    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call(
                "local-path-exec",
                "exec",
                r#"{"argv":["wiz-run","stories-cli.js","list"],"cwd":null,"timeout_ms":1000}"#,
            )),
            Reply::Sse(text("LOCAL-PATH-EXEC-DONE")),
        ],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--mode",
        "yolo",
        "--sandbox",
        "off",
    ]);
    let executable = ego.home_file(
        ".local/bin/wiz-run",
        "#!/bin/sh\nprintf '%s\\n' WIZ-RUN-FROM-LOCAL-PATH\n",
    );
    let mut permissions = std::fs::metadata(&executable)
        .expect("local executable metadata")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&executable, permissions).expect("make local fixture executable");
    let path = format!("{}:/usr/bin:/bin", executable.parent().unwrap().display());

    let finished = ego
        .env("PATH", &path)
        .run_pty("run the local story command");

    assert_eq!(finished.code, Some(0), "{}", finished.stdout);
    assert!(finished.stdout.contains("LOCAL-PATH-EXEC-DONE"));
    assert!(
        !finished.stdout.contains("[y] allow"),
        "yolo must not stop for a second confirmation: {}",
        finished.stdout
    );
    let generations = stub.generations();
    assert_eq!(generations.len(), 2);
    assert!(
        generations[1]
            .to_string()
            .contains("WIZ-RUN-FROM-LOCAL-PATH"),
        "the executable found on effective PATH did not run: {}",
        generations[1]
    );
}

#[test]
fn live_surface_esc_expedites_sampling_once_and_reports_interrupt_state() {
    let queued = "steer while sampling";
    let extension = "tail extension while expedited";
    let stub = Stub::start(Plan {
        completions: vec![
            paced_line("STREAMED-BEFORE-ESC", Duration::from_millis(500)),
            paced_line("EXPEDITED-RECEIVED", Duration::from_millis(500)),
            Reply::Sse(text("EXTENSION-RECEIVED")),
        ],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--sandbox",
        "off",
    ]);
    // These cases all begin by answering the trust question, so the workspace
    // has to hold something worth asking about. An empty checkout is asked
    // nothing at all.
    ego.file("AGENTS.md", "# live surface fixture\n");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let mut child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
    );
    let mut input = input;
    let mut output = Vec::new();

    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust answer flush");
    input.write_all(b"initial\r").expect("initial prompt");
    input.flush().expect("initial prompt flush");
    until(&reader, &mut output, b"STREAMED-BEFORE-ESC");

    input.write_all(queued.as_bytes()).expect("queue steer");
    input.write_all(b"\r").expect("queue steer submit");
    input.flush().expect("queue steer flush");
    until_normalized(
        &reader,
        &mut output,
        0,
        "Messages to be submitted after next tool call (press esc to interrupt and send immediately)",
    );

    let esc_start = output.len();
    input.write_all(b"\x1b\x1b").expect("repeated Esc");
    input.flush().expect("Esc flush");
    until_normalized(&reader, &mut output, esc_start, "interrupt");
    until(&reader, &mut output, b"EXPEDITED-RECEIVED");
    input
        .write_all(extension.as_bytes())
        .expect("extend expedited tail");
    input
        .write_all(b"\r")
        .expect("extend expedited tail submit");
    input.flush().expect("extend expedited tail flush");
    until_normalized(
        &reader,
        &mut output,
        esc_start,
        "Messages to be submitted after next tool call (press esc to interrupt and send immediately)",
    );
    let second_esc_start = output.len();
    input
        .write_all(b"\x1b\x1b")
        .expect("repeated Esc extension");
    input.flush().expect("Esc extension flush");
    until_normalized(&reader, &mut output, second_esc_start, "interrupt");
    until(&reader, &mut output, b"EXTENSION-RECEIVED");
    input.write_all(b"\x04").expect("EOF");
    input.flush().expect("EOF flush");

    drop(input);
    drop(pair);
    let mut killer = child.clone_killer();
    let (status_sender, status_receiver) = mpsc::channel();
    let waiter = thread::spawn(move || {
        let status = child.wait();
        status_sender.send(status).expect("status receiver dropped");
    });
    let deadline = Instant::now() + PROGRESS_DEADLINE;
    let status = loop {
        drain(&reader, &mut output);
        match status_receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(Ok(status)) => break status,
            Ok(Err(error)) => panic!("wait failed: {error}"),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("child waiter disconnected")
            }
            Err(mpsc::RecvTimeoutError::Timeout) if Instant::now() >= deadline => {
                drain(&reader, &mut output);
                killer.kill().expect("kill timed out child");
                match status_receiver
                    .recv_timeout(Duration::from_secs(2))
                    .expect("killed child did not exit")
                {
                    Ok(status) => break status,
                    Err(error) => panic!("wait after kill failed: {error}"),
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    };
    assert!(status.success(), "interactive process failed: {status:?}");
    waiter.join().expect("waiter thread panicked");
    reader_join.join().expect("reader thread panicked");
    writer_join.join().expect("writer thread panicked");

    let generations = stub.generations();
    assert_eq!(
        generations.len(),
        3,
        "repeated Esc must not duplicate delivery"
    );
    assert_eq!(
        generations
            .iter()
            .map(|generation| count_exact_strings(generation, queued))
            .collect::<Vec<_>>(),
        vec![0, 1, 1],
        "the expedited tail is delivered once and then retained in history"
    );
    assert_eq!(
        generations
            .iter()
            .map(|generation| count_exact_strings(generation, extension))
            .collect::<Vec<_>>(),
        vec![0, 0, 1],
        "a tail extension is delivered once after the first marker"
    );
    assert!(!output.windows(8).any(|window| window == b"\x1b[?1049h"));
    assert!(!output.windows(8).any(|window| window == b"\x1b[?1000h"));
    assert!(!output.windows(8).any(|window| window == b"\x1b[?1006h"));
}

#[test]
fn live_surface_meta_p_and_model_slash_share_one_modal_and_paste_stays_prompt() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("PASTED-SLASH-PROMPT"))],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--sandbox",
        "off",
    ]);
    // These cases all begin by answering the trust question, so the workspace
    // has to hold something worth asking about. An empty checkout is asked
    // nothing at all.
    ego.file("AGENTS.md", "# live surface fixture\n");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let mut child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
    );
    let mut input = input;
    let mut output = Vec::new();

    until(&reader, &mut output, b"Trust project files");
    drain(&reader, &mut output);
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust answer flush");
    let startup_start = output.len();
    until_post_trust_prompt(&reader, &mut output, startup_start);
    assert!(
        !String::from_utf8_lossy(&output[startup_start..])
            .contains("the local action handler is not installed"),
        "activation must complete before the first live prompt"
    );

    let slash_start = output.len();
    // An exact registered action submits with the same single Enter as a
    // normal prompt, even while its completion menu is active.
    input.write_all(b"/").expect("slash prefix");
    input.flush().expect("slash prefix flush");
    until_normalized_or_generation(&reader, &mut output, slash_start, "model", &stub);
    let suffix_start = output.len();
    input.write_all(b"model").expect("model slash suffix");
    input.flush().expect("model slash suffix flush");
    until_normalized(&reader, &mut output, suffix_start, "/model");
    input.write_all(b"\r").expect("model slash submit");
    input.flush().expect("model slash flush");
    until_normalized_or_generation(&reader, &mut output, slash_start, "Select model", &stub);
    drain_until_quiet(&reader, &mut output);
    let slash_modal = normalized(&output[slash_start..]);
    assert!(slash_modal.contains("Select model"));
    let down_start = output.len();
    input.write_all(b"\x1b[B").expect("model picker down");
    input.flush().expect("model picker down flush");
    until_from(
        &reader,
        &mut output,
        down_start,
        b"> 2. anthropic/claude-fable-5-1",
    );
    drain_until_quiet(&reader, &mut output);
    let up_start = output.len();
    input.write_all(b"\x1b[A").expect("model picker up");
    input.flush().expect("model picker up flush");
    until_from(&reader, &mut output, up_start, b"> 1. ollama/qwen3");
    drain_until_quiet(&reader, &mut output);
    let numeric_start = output.len();
    input.write_all(b"2").expect("model picker number");
    input.flush().expect("model picker number flush");
    until_from(
        &reader,
        &mut output,
        numeric_start,
        b"> 2. anthropic/claude-fable-5-1",
    );
    drain_until_quiet(&reader, &mut output);
    let restore_start = output.len();
    input.write_all(b"\x1b[A").expect("model picker restore");
    input.flush().expect("model picker restore flush");
    until_from(&reader, &mut output, restore_start, b"> 1. ollama/qwen3");
    drain_until_quiet(&reader, &mut output);
    let enter_start = output.len();
    input.write_all(b"\r").expect("model picker enter");
    input.flush().expect("model picker enter flush");
    until_normalized(
        &reader,
        &mut output,
        enter_start,
        "Select reasoning for ollama/qwen3",
    );
    drain_until_quiet(&reader, &mut output);
    // Discovery has no reasoning default: select Off before committing.
    let reasoning_start = output.len();
    input
        .write_all(b"1")
        .expect("model picker reasoning number");
    input.flush().expect("model picker reasoning number flush");
    until_from(&reader, &mut output, reasoning_start, b"> 1. Off");
    drain_until_quiet(&reader, &mut output);
    let slash_accept_start = output.len();
    input
        .write_all(b"\r")
        .expect("model picker reasoning enter");
    input.flush().expect("model picker reasoning enter flush");
    until_from(&reader, &mut output, slash_accept_start, b"> ");

    let meta_start = output.len();
    // The PTY uses the parser-stable CSI-u wire spelling for Meta-P. It
    // yields the same crossterm KeyEvent as a terminal's Option-P/Alt-P,
    // without the ESC-prefix timing ambiguity of a raw `ESC p` pair.
    input.write_all(b"\x1b[112;33u").expect("Meta-P");
    input.flush().expect("Meta-P flush");
    until_normalized(&reader, &mut output, meta_start, "Select model");
    drain_until_quiet(&reader, &mut output);
    let meta_modal = normalized(&output[meta_start..]);
    // Compare the complete latest menu, not the number of painter redraws.
    assert_eq!(
        meta_modal.rsplit_once("Select model").unwrap().1,
        slash_modal.rsplit_once("Select model").unwrap().1,
        "Meta-P and /model must present the same complete picker"
    );
    let meta_close_start = output.len();
    input.write_all(b"\x1b").expect("close Meta-P modal");
    input.flush().expect("close Meta-P modal flush");
    until_from(&reader, &mut output, meta_close_start, b"> ");

    let menu_start = output.len();
    input.write_all(b"/").expect("bare slash");
    input.flush().expect("bare slash flush");
    until_normalized(&reader, &mut output, menu_start, "help");
    let menu = normalized(&output[menu_start..]);
    assert!(
        menu.contains("help"),
        "bare typed slash opens the action menu"
    );
    let menu_close_start = output.len();
    input.write_all(b"\x1b").expect("close slash menu");
    input.flush().expect("close slash menu flush");
    until_from(&reader, &mut output, menu_close_start, b"> ");
    let prompt_count = output.windows(2).filter(|window| *window == b"> ").count();
    input.write_all(b"\x03").expect("clear slash prefix");
    input.flush().expect("clear slash prefix flush");
    until_prompt_after(&reader, &mut output, prompt_count);

    let partial_start = output.len();
    input.write_all(b"/hel\r").expect("partial help command");
    input.flush().expect("partial help flush");
    drain_until_quiet(&reader, &mut output);
    assert!(stub.generations().is_empty());
    assert!(
        !normalized(&output[partial_start..]).contains("Exit the interactive session"),
        "a partial slash must complete without executing the action"
    );
    input
        .write_all(b"\r")
        .expect("submit completed help command");
    input.flush().expect("completed help flush");
    until_normalized_without_new_generation(
        &reader,
        &mut output,
        partial_start,
        "Exit the interactive session",
        &stub,
        0,
    );
    until_post_trust_prompt(&reader, &mut output, partial_start);

    let pasted_start = output.len();
    input
        .write_all(b"\x1b[200~/model\x1b[201~\r")
        .expect("pasted slash");
    input.flush().expect("pasted slash flush");
    until_normalized(&reader, &mut output, pasted_start, "PASTED-SLASH-PROMPT");
    drain_until_quiet(&reader, &mut output);
    assert!(
        output[pasted_start..]
            .windows(2)
            .any(|window| window == b"> "),
        "the composer must remain available after the pasted prompt"
    );
    assert!(
        !normalized(&output[pasted_start..]).contains("Select model"),
        "pasted slash text stays a prompt instead of opening the picker"
    );
    assert_eq!(stub.generations().len(), 1);
    assert!(stub.generations()[0].to_string().contains("/model"));

    let draft_start = output.len();
    input
        .write_all(b"draft restored after picker")
        .expect("type modal draft");
    input.flush().expect("type modal draft flush");
    until_from(
        &reader,
        &mut output,
        draft_start,
        b"draft restored after picker",
    );
    let draft_modal_start = output.len();
    input.write_all(b"\x1b[112;33u").expect("Meta-P with draft");
    input.flush().expect("Meta-P with draft flush");
    until_normalized(&reader, &mut output, draft_modal_start, "Select model");
    let draft_close_start = output.len();
    input.write_all(b"\x1b").expect("close draft modal");
    input.flush().expect("close draft modal flush");
    until_from(&reader, &mut output, draft_close_start, b"> ");
    assert!(
        normalized(&output[draft_close_start..]).contains("draft restored after picker"),
        "the modal restores the exact composer text"
    );

    let cleanup_start = output.len();
    input.write_all(b"\x03").expect("clear restored draft");
    input.flush().expect("clear restored draft flush");
    until_from(&reader, &mut output, cleanup_start, b"> ");
    input.write_all(b"\x04").expect("EOF");
    input.flush().expect("EOF flush");
    drop(input);
    drop(pair);
    let mut killer = child.clone_killer();
    let (status_sender, status_receiver) = mpsc::channel();
    let waiter = thread::spawn(move || {
        let status = child.wait();
        status_sender.send(status).expect("status receiver dropped");
    });
    let deadline = Instant::now() + PROGRESS_DEADLINE;
    let status = loop {
        drain(&reader, &mut output);
        match status_receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(Ok(status)) => break status,
            Ok(Err(error)) => panic!("wait failed: {error}"),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("child waiter disconnected")
            }
            Err(mpsc::RecvTimeoutError::Timeout) if Instant::now() >= deadline => {
                drain(&reader, &mut output);
                killer.kill().expect("kill timed out child");
                match status_receiver
                    .recv_timeout(Duration::from_secs(2))
                    .expect("killed child did not exit")
                {
                    Ok(status) => break status,
                    Err(error) => panic!("wait after kill failed: {error}"),
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    };
    assert!(status.success(), "interactive process failed: {status:?}");
    waiter.join().expect("waiter thread panicked");
    reader_join.join().expect("reader thread panicked");
    writer_join.join().expect("writer thread panicked");
}

#[test]
fn argv_seed_and_two_typed_follow_ups_are_distinct_provider_turns() {
    let seed = "argv seed";
    let first = "typed follow-up one";
    let second = "typed follow-up two";
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(text("ARGV-SEED-FIRST\nARGV-SEED-SECOND")),
            Reply::Sse(text("FOLLOW-UP-ONE-REPLY")),
            Reply::Sse(text("FOLLOW-UP-TWO-REPLY")),
        ],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--sandbox",
        "off",
        seed,
    ]);
    ego.file("AGENTS.md", "# live surface fixture\n");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let mut child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
    );
    let mut input = input;
    let mut output = Vec::new();

    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust answer flush");
    let post_trust_start = output.len();
    // An argv seed can complete before Reedline finishes its cursor-position
    // handshake. Sending the first typed line merely because provider output
    // arrived races that handshake and the terminal consumes the input as its
    // cursor response. The first post-trust DSR followed by a prompt is the
    // observable boundary at which typed input is safe.
    until_post_trust_prompt(&reader, &mut output, post_trust_start);
    until_normalized(&reader, &mut output, 0, "ARGV-SEED-SECOND");
    drain_until_quiet(&reader, &mut output);
    let visible =
        String::from_utf8_lossy(&strip_csi(&output[post_trust_start..])).replace("\r\n", "\n");
    let first_at = visible.find("ARGV-SEED-FIRST").expect("first streamed row");
    let second_at = visible
        .find("ARGV-SEED-SECOND")
        .expect("final streamed row");
    assert!(
        first_at < second_at,
        "streamed rows changed order: {visible:?}"
    );
    assert!(
        !visible[first_at..second_at].contains("\n> "),
        "the idle composer appeared before the final streamed row: {visible:?}"
    );

    let help_start = output.len();
    input.write_all(b"/help\r").expect("local help command");
    input.flush().expect("local help flush");
    until_normalized_without_new_generation(
        &reader,
        &mut output,
        help_start,
        "/quit  Exit the interactive session",
        &stub,
        1,
    );
    until_post_trust_prompt(&reader, &mut output, help_start);

    let refusal_start = output.len();
    input
        .write_all(b"/mcp add\r")
        .expect("unsupported MCP command");
    input.flush().expect("unsupported MCP command flush");
    until_normalized_without_new_generation(
        &reader,
        &mut output,
        refusal_start,
        "ego mcp add NAME",
        &stub,
        1,
    );
    until_post_trust_prompt(&reader, &mut output, refusal_start);

    let first_follow_up_start = output.len();
    input.write_all(first.as_bytes()).expect("first follow-up");
    input.write_all(b"\r").expect("first follow-up submit");
    input.flush().expect("first follow-up flush");
    until_normalized(&reader, &mut output, refusal_start, "FOLLOW-UP-ONE-REPLY");
    until_post_trust_prompt(&reader, &mut output, first_follow_up_start);

    input
        .write_all(second.as_bytes())
        .expect("second follow-up");
    input.write_all(b"\r").expect("second follow-up submit");
    input.flush().expect("second follow-up flush");
    until_normalized(&reader, &mut output, refusal_start, "FOLLOW-UP-TWO-REPLY");

    input.write_all(b"\x04").expect("EOF");
    input.flush().expect("EOF flush");
    drop(input);
    drop(pair);
    let mut killer = child.clone_killer();
    let (status_sender, status_receiver) = mpsc::channel();
    let waiter = thread::spawn(move || {
        let status = child.wait();
        status_sender.send(status).expect("status receiver dropped");
    });
    let deadline = Instant::now() + PROGRESS_DEADLINE;
    let status = loop {
        drain(&reader, &mut output);
        match status_receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(Ok(status)) => break status,
            Ok(Err(error)) => panic!("wait failed: {error}"),
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!("child waiter disconnected"),
            Err(mpsc::RecvTimeoutError::Timeout) if Instant::now() >= deadline => {
                drain(&reader, &mut output);
                killer.kill().expect("kill timed out child");
                match status_receiver
                    .recv_timeout(Duration::from_secs(2))
                    .expect("killed child did not exit")
                {
                    Ok(status) => break status,
                    Err(error) => panic!("wait after kill failed: {error}"),
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    };
    assert!(status.success(), "interactive process failed: {status:?}");
    waiter.join().expect("waiter thread panicked");
    reader_join.join().expect("reader thread panicked");
    writer_join.join().expect("writer thread panicked");

    let generations = stub.generations();
    assert_eq!(generations.len(), 3);
    assert_eq!(
        generations
            .iter()
            .map(|generation| count_exact_strings(generation, seed))
            .collect::<Vec<_>>(),
        vec![1, 1, 1],
        "the argv seed is retained once in context and never replayed"
    );
    assert_eq!(
        generations
            .iter()
            .map(|generation| count_exact_strings(generation, first))
            .collect::<Vec<_>>(),
        vec![0, 1, 1],
        "the first typed follow-up is delivered once and retained"
    );
    assert_eq!(
        generations
            .iter()
            .map(|generation| count_exact_strings(generation, second))
            .collect::<Vec<_>>(),
        vec![0, 0, 1],
        "the second typed follow-up is delivered once"
    );
}

#[test]
fn busy_ctrl_c_cancels_work_and_idle_double_ctrl_c_exits() {
    let stub = Stub::start(Plan {
        completions: vec![
            paced_line("ACTIVE-WORK", Duration::from_millis(500)),
            Reply::Sse(text("AFTER-CANCEL")),
        ],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--sandbox",
        "off",
        "cancel this turn",
    ]);
    ego.file("AGENTS.md", "# live surface fixture\n");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let mut child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
    );
    let mut input = input;
    let mut output = Vec::new();

    until(&reader, &mut output, b"Trust project files");
    let prompt_count = output.windows(2).filter(|window| *window == b"> ").count();
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust answer flush");
    until_prompt_after(&reader, &mut output, prompt_count);
    until_normalized(&reader, &mut output, 0, "ACTIVE-WORK");
    let cancel_start = output.len();
    let prompt_count = output.windows(2).filter(|window| *window == b"> ").count();
    input.write_all(b"\x03").expect("busy Ctrl-C");
    input.flush().expect("busy Ctrl-C flush");
    until_normalized(&reader, &mut output, cancel_start, "cancelling active work");
    until_prompt_after(&reader, &mut output, prompt_count);

    input
        .write_all(b"after cancel\r")
        .expect("post-cancel prompt");
    input.flush().expect("post-cancel prompt flush");
    until_normalized(&reader, &mut output, cancel_start, "AFTER-CANCEL");
    drain_until_quiet(&reader, &mut output);
    input.write_all(b"\x03").expect("first idle Ctrl-C");
    input.flush().expect("first idle Ctrl-C flush");
    thread::sleep(Duration::from_millis(50));
    input.write_all(b"\x03").expect("second idle Ctrl-C");
    input.flush().expect("second idle Ctrl-C flush");

    drop(input);
    drop(pair);
    let mut killer = child.clone_killer();
    let (status_sender, status_receiver) = mpsc::channel();
    let waiter = thread::spawn(move || {
        let status = child.wait();
        status_sender.send(status).expect("status receiver dropped");
    });
    let status = match status_receiver.recv_timeout(PROGRESS_DEADLINE) {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => panic!("wait failed: {error}"),
        Err(error) => {
            killer.kill().expect("kill timed out child");
            panic!("idle double Ctrl-C did not exit: {error}")
        }
    };
    assert!(status.success(), "interactive process failed: {status:?}");
    waiter.join().expect("waiter thread panicked");
    reader_join.join().expect("reader thread panicked");
    writer_join.join().expect("writer thread panicked");

    let generations = stub.generations();
    assert_eq!(generations.len(), 2);
    assert_eq!(
        generations
            .iter()
            .map(|generation| count_exact_strings(generation, "after cancel"))
            .collect::<Vec<_>>(),
        vec![0, 1],
    );
}

fn bounded_exit(mut child: Box<dyn portable_pty::Child + Send + Sync>) -> portable_pty::ExitStatus {
    let mut killer = child.clone_killer();
    let (sender, receiver) = mpsc::channel();
    let waiter = thread::spawn(move || {
        let _ = sender.send(child.wait());
    });
    let status = match receiver.recv_timeout(PROGRESS_DEADLINE) {
        Ok(status) => status.expect("child wait"),
        Err(error) => {
            killer.kill().expect("kill stalled child");
            panic!("child did not exit within deadline: {error}");
        }
    };
    waiter.join().expect("child waiter panicked");
    status
}

#[test]
fn startup_picker_escape_exits_without_saving_a_model() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub)
        .conversing()
        .env("ANTHROPIC_API_KEY", "fixture-anthropic-key");
    ego.file("AGENTS.md", "# live surface fixture\n");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 40,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (mut input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
    );
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust flush");
    until_normalized(&reader, &mut output, 0, "Select model");
    drain_until_quiet(&reader, &mut output);
    input.write_all(b"\x1b").expect("startup Escape");
    input.flush().expect("startup Escape flush");
    let status = bounded_exit(child);
    assert_eq!(status.exit_code(), 130, "Escape must cancel startup");
    drop(input);
    reader_join.join().expect("reader thread panicked");
    writer_join.join().expect("writer thread panicked");
    assert!(stub.generations().is_empty());
    assert!(!ego.home_path().join("config.toml").exists());
}

#[test]
fn startup_picker_routes_raw_keys_and_persists_the_explicit_choice() {
    let stub = Stub::start(Plan::default());
    let home = tempfile::tempdir().expect("home");
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(
        workspace.path().join("AGENTS.md"),
        "# live surface fixture\n",
    )
    .expect("workspace fixture");
    let first = Ego::at(home.path(), workspace.path(), Some(&stub))
        .conversing()
        .env("OPENAI_API_KEY", "fixture-openai-key")
        .env("ANTHROPIC_API_KEY", "fixture-anthropic-key");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair
        .slave
        .spawn_command(first.pty_command())
        .expect("spawn");
    let (input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
    );
    let mut input = input;
    let mut output = Vec::new();

    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust answer flush");
    let picker_start = output.len();
    until_normalized(&reader, &mut output, picker_start, "Select model");
    drain_until_quiet(&reader, &mut output);

    let down_start = output.len();
    input.write_all(b"\x1b[B").expect("startup picker down");
    input.flush().expect("startup picker down flush");
    until_from(
        &reader,
        &mut output,
        down_start,
        b"> 2. anthropic/claude-haiku-4-5",
    );
    drain_until_quiet(&reader, &mut output);

    let up_start = output.len();
    input.write_all(b"\x1b[A").expect("startup picker up");
    input.flush().expect("startup picker up flush");
    until_from(
        &reader,
        &mut output,
        up_start,
        b"> 1. anthropic/claude-fable-5-1",
    );
    drain_until_quiet(&reader, &mut output);

    let number_start = output.len();
    input.write_all(b"1").expect("startup picker number one");
    input.flush().expect("startup picker number one flush");
    until_from(
        &reader,
        &mut output,
        number_start,
        b"> 1. anthropic/claude-fable-5-1",
    );
    drain_until_quiet(&reader, &mut output);
    let chosen_start = output.len();
    input.write_all(b"2").expect("startup picker number two");
    input.flush().expect("startup picker number two flush");
    until_from(
        &reader,
        &mut output,
        chosen_start,
        b"> 2. anthropic/claude-haiku-4-5",
    );
    drain_until_quiet(&reader, &mut output);

    input.write_all(b"\r").expect("startup model accept");
    input.flush().expect("startup model accept flush");
    until_normalized(&reader, &mut output, chosen_start, "Select reasoning for");
    drain_until_quiet(&reader, &mut output);
    let reasoning_start = output.len();
    input.write_all(b"1").expect("startup reasoning number");
    input.flush().expect("startup reasoning number flush");
    until_from(&reader, &mut output, reasoning_start, b"> 1.");
    drain_until_quiet(&reader, &mut output);
    let first_prompt = output.windows(2).filter(|window| *window == b"> ").count();
    input.write_all(b"\r").expect("startup reasoning accept");
    input.flush().expect("startup reasoning accept flush");
    until_prompt_after(&reader, &mut output, first_prompt);
    drain_until_quiet(&reader, &mut output);

    let config = std::fs::read_to_string(home.path().join("config.toml"))
        .expect("startup selection persisted atomically");
    assert!(config.contains("model = \"anthropic/claude-haiku-4-5\""));
    assert!(config.contains("thinking = \"off\""));

    input.write_all(b"\x04").expect("first EOF");
    input.flush().expect("first EOF flush");
    let status = bounded_exit(child);
    assert!(
        status.success(),
        "first interactive process failed: {status:?}"
    );
    drop(input);
    reader_join.join().expect("first reader thread panicked");
    writer_join.join().expect("first writer thread panicked");

    let second = Ego::at(home.path(), workspace.path(), Some(&stub))
        .conversing()
        .env("OPENAI_API_KEY", "fixture-openai-key")
        .env("ANTHROPIC_API_KEY", "fixture-anthropic-key");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("second pty");
    let child = pair
        .slave
        .spawn_command(second.pty_command())
        .expect("second spawn");
    let (input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("second writer"));
    let (reader, reader_join) = stream(
        pair.master.try_clone_reader().expect("second reader"),
        dsr_responder,
    );
    let mut input = input;
    let mut output = Vec::new();

    let restart_start = 0;
    until_post_trust_prompt(&reader, &mut output, restart_start);
    assert!(
        !normalized(&output[restart_start..]).contains("Select model"),
        "persisted selection must bypass the startup picker: {}",
        normalized(&output[restart_start..]),
    );

    input.write_all(b"\x04").expect("second EOF");
    input.flush().expect("second EOF flush");
    let status = bounded_exit(child);
    assert!(
        status.success(),
        "second interactive process failed: {status:?}"
    );
    drop(input);
    reader_join.join().expect("second reader thread panicked");
    writer_join.join().expect("second writer thread panicked");
}

#[test]
fn every_registered_slash_action_is_local_and_has_intentional_terminal_behavior() {
    const COMMANDS: [&str; 24] = [
        "help",
        "model",
        "thinking",
        "mode",
        "plan",
        "yolo",
        "permissions",
        "mcp",
        "sessions",
        "fork",
        "compact",
        "clear",
        "usage",
        "statusline",
        "export",
        "doctor",
        "quit",
        "new",
        "name",
        "goal",
        "tools",
        "skills",
        "hooks",
        "version",
    ];
    assert_eq!(ActionRegistry::final_surface().names(), COMMANDS);

    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--sandbox",
        "off",
    ]);
    ego.file("AGENTS.md", "# registered action PTY fixture\n");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 40,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (mut input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
    );
    let mut output = Vec::new();

    until(&reader, &mut output, b"Trust project files");
    let prompt_count = output.windows(2).filter(|window| *window == b"> ").count();
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust answer flush");
    until_prompt_after(&reader, &mut output, prompt_count);

    submit_local_action(
        &reader,
        &mut output,
        &mut input,
        &stub,
        "help",
        Some("εgo commands"),
    );

    for (command, prompt) in [
        ("model", "Select model"),
        ("thinking", "Select reasoning for"),
        ("mode", "Choose execution mode"),
        ("permissions", "Choose permission operation"),
        ("statusline", "Choose status-line setup"),
    ] {
        let start = output.len();
        input
            .write_all(format!("/{command}\r").as_bytes())
            .unwrap_or_else(|error| panic!("write /{command}: {error}"));
        input.flush().expect("modal action flush");
        until_normalized_without_new_generation(&reader, &mut output, start, prompt, &stub, 0);
        let prompt_count = output.windows(2).filter(|window| *window == b"> ").count();
        input.write_all(b"\x1b").expect("cancel modal action");
        input.flush().expect("cancel modal action flush");
        until_prompt_after(&reader, &mut output, prompt_count);
        drain_until_quiet(&reader, &mut output);
    }

    submit_local_action(&reader, &mut output, &mut input, &stub, "plan", None);
    submit_local_action(&reader, &mut output, &mut input, &stub, "yolo", None);

    for (command, expected) in [
        ("mcp", Some("mcp:")),
        ("sessions", Some("UPDATED")),
        ("fork", None),
        ("compact", None),
        ("clear", None),
        ("usage", Some("usage as_of=")),
        ("export", None),
        ("doctor", Some("acp.extension:hold")),
        ("new", None),
        ("name", None),
        ("goal", None),
        ("tools", Some("tools:")),
        ("skills", None),
        ("hooks", None),
        ("version", Some("ego 0.1.0")),
    ] {
        submit_local_action(&reader, &mut output, &mut input, &stub, command, expected);
    }

    let quit_start = output.len();
    input.write_all(b"/quit\r").expect("quit command");
    input.flush().expect("quit command flush");
    let status = bounded_exit(child);
    assert!(status.success(), "interactive process failed: {status:?}");
    drain(&reader, &mut output);
    assert!(
        normalized(&output[quit_start..]).contains("unsent drafts"),
        "/quit did not render its explicit exit accounting"
    );
    assert!(stub.generations().is_empty());
    drop(input);
    drop(pair);
    reader_join.join().expect("reader thread panicked");
    writer_join.join().expect("writer thread panicked");
}

#[test]
fn help_and_doctor_remain_local_after_80_60_and_40_column_resizes() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--sandbox",
        "off",
    ]);
    ego.file("AGENTS.md", "# live surface fixture\n");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 40,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let mut child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
    );
    let mut input = input;
    let mut output = Vec::new();

    until(&reader, &mut output, b"Trust project files");
    let prompt_count = output.windows(2).filter(|window| *window == b"> ").count();
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust answer flush");
    until_prompt_after(&reader, &mut output, prompt_count);

    for width in [80, 60, 40] {
        let prompt_count = output.windows(2).filter(|window| *window == b"> ").count();
        pair.master
            .resize(PtySize {
                rows: 40,
                cols: width,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("resize");
        until_prompt_after(&reader, &mut output, prompt_count);

        let help_start = output.len();
        let prompt_count = output.windows(2).filter(|window| *window == b"> ").count();
        input.write_all(b"/help\r").expect("help command");
        input.flush().expect("help command flush");
        until_normalized_without_new_generation(
            &reader,
            &mut output,
            help_start,
            "Surface",
            &stub,
            0,
        );
        until_prompt_after(&reader, &mut output, prompt_count);

        let doctor_start = output.len();
        let prompt_count = output.windows(2).filter(|window| *window == b"> ").count();
        input.write_all(b"/doctor\r").expect("doctor command");
        input.flush().expect("doctor command flush");
        until_normalized_without_new_generation(
            &reader,
            &mut output,
            doctor_start,
            "acp.extension:hold",
            &stub,
            0,
        );
        until_prompt_after(&reader, &mut output, prompt_count);
    }

    input.write_all(b"\x04").expect("EOF");
    input.flush().expect("EOF flush");
    drop(input);
    drop(pair);
    let mut killer = child.clone_killer();
    let (status_sender, status_receiver) = mpsc::channel();
    let waiter = thread::spawn(move || {
        let status = child.wait();
        status_sender.send(status).expect("status receiver dropped");
    });
    let status = match status_receiver.recv_timeout(PROGRESS_DEADLINE) {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => panic!("wait failed: {error}"),
        Err(error) => {
            killer.kill().expect("kill timed out child");
            panic!("interactive process did not exit: {error}")
        }
    };
    assert!(status.success(), "interactive process failed: {status:?}");
    waiter.join().expect("waiter thread panicked");
    reader_join.join().expect("reader thread panicked");
    writer_join.join().expect("writer thread panicked");
    assert!(stub.generations().is_empty());
}

#[test]
fn live_surface_native_questions_cover_single_multiple_and_free_text() {
    let single = serde_json::json!({
        "prompt": "Single question",
        "mode": "single",
        "options": [{"id": "yes", "label": "Yes"}, {"id": "no", "label": "No"}],
        "free_text": false,
    })
    .to_string();
    let multiple = serde_json::json!({
        "prompt": "Multiple question",
        "mode": "multiple",
        "options": [{"id": "red", "label": "Red"}, {"id": "blue", "label": "Blue"}],
        "free_text": false,
    })
    .to_string();
    let free_text = serde_json::json!({
        "prompt": "Free question",
        "mode": "free_text",
        "options": [],
        "free_text": true,
    })
    .to_string();
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call("question-single", "ask_question", &single)),
            Reply::Sse(tool_call("question-multiple", "ask_question", &multiple)),
            Reply::Sse(tool_call("question-free", "ask_question", &free_text)),
            Reply::Sse(text("QUESTIONS-DONE")),
        ],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--sandbox",
        "off",
    ]);
    // These cases all begin by answering the trust question, so the workspace
    // has to hold something worth asking about. An empty checkout is asked
    // nothing at all.
    ego.file("AGENTS.md", "# live surface fixture\n");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let mut child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
    );
    let mut input = input;
    let mut output = Vec::new();

    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust answer flush");
    let idle_start = output.len();
    until_from(&reader, &mut output, idle_start, b"> ");

    input
        .write_all(b"ask questions\r")
        .expect("question prompt");
    input.flush().expect("question prompt flush");
    until_normalized(&reader, &mut output, idle_start, "Single question");
    let single_choice_start = output.len();
    until_from(&reader, &mut output, single_choice_start, b"> 1. Yes");
    input.write_all(b"2").expect("single selection");
    input.flush().expect("single selection flush");
    until_from(&reader, &mut output, single_choice_start, b"> 2. No");
    let single_submit_start = output.len();
    input.write_all(b"\r").expect("single answer");
    input.flush().expect("single answer flush");
    until_from(
        &reader,
        &mut output,
        single_submit_start,
        b"Multiple question",
    );
    let multiple_first_start = output.len();
    until_from(&reader, &mut output, multiple_first_start, b"> [ ] 1. Red");
    input.write_all(b"1").expect("multiple first selection");
    input.flush().expect("multiple first selection flush");
    until_from(&reader, &mut output, multiple_first_start, b"> [x] 1. Red");
    let multiple_second_start = output.len();
    input.write_all(b"2").expect("multiple second selection");
    input.flush().expect("multiple second selection flush");
    until_from(
        &reader,
        &mut output,
        multiple_second_start,
        b"> [x] 2. Blue",
    );
    let multiple_submit_start = output.len();
    input.write_all(b"\r").expect("multiple answer");
    input.flush().expect("multiple answer flush");
    until_from(
        &reader,
        &mut output,
        multiple_submit_start,
        b"Free question ",
    );
    input.write_all(b"free answer\r").expect("free-text answer");
    input.flush().expect("free-text answer flush");
    until_normalized(&reader, &mut output, idle_start, "QUESTIONS-DONE");

    let generations = stub.generations();
    assert_eq!(
        generations.len(),
        4,
        "each answer resumes the same live turn"
    );
    assert!(
        generations
            .iter()
            .any(|generation| generation.to_string().contains("free answer")),
        "the free-text answer reaches the model using the canonical answer value"
    );

    input.write_all(b"\x04").expect("EOF");
    input.flush().expect("EOF flush");
    drop(input);
    drop(pair);
    let mut killer = child.clone_killer();
    let (status_sender, status_receiver) = mpsc::channel();
    let waiter = thread::spawn(move || {
        let status = child.wait();
        status_sender.send(status).expect("status receiver dropped");
    });
    let deadline = Instant::now() + PROGRESS_DEADLINE;
    let status = loop {
        drain(&reader, &mut output);
        match status_receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(Ok(status)) => break status,
            Ok(Err(error)) => panic!("wait failed: {error}"),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("child waiter disconnected")
            }
            Err(mpsc::RecvTimeoutError::Timeout) if Instant::now() >= deadline => {
                drain(&reader, &mut output);
                killer.kill().expect("kill timed out child");
                match status_receiver
                    .recv_timeout(Duration::from_secs(2))
                    .expect("killed child did not exit")
                {
                    Ok(status) => break status,
                    Err(error) => panic!("wait after kill failed: {error}"),
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    };
    assert!(status.success(), "interactive process failed: {status:?}");
    waiter.join().expect("waiter thread panicked");
    reader_join.join().expect("reader thread panicked");
    writer_join.join().expect("writer thread panicked");
}
