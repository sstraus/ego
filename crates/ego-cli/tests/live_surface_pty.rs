mod support;

use portable_pty::{NativePtySystem, PtySize, PtySystem};
use std::{
    io::{self, Read, Write},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use support::{Ego, Plan, Reply, Stub, gated_paced_tool_call_line, paced, text};

// portable-pty transports bytes but does not emulate a terminal. Keep the
// reported cursor inside both 80x24 and 12x8; painter geometry is proven by
// the vendor unit test, while this test needs only a live DSR response.
const DSR_QUERY: &[u8] = b"\x1b[6n";
const DSR_RESPONSE: &[u8] = b"\x1b[4;1R";

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
            .recv_timeout(Duration::from_secs(8))
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
        let chunk = match receiver.recv_timeout(Duration::from_secs(8)) {
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
        let chunk = match receiver.recv_timeout(Duration::from_secs(8)) {
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

fn drain(receiver: &mpsc::Receiver<Vec<u8>>, output: &mut Vec<u8>) {
    while let Ok(chunk) = receiver.try_recv() {
        output.extend(chunk);
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
        "--ephemeral",
        "--sandbox",
        "off",
    ]);
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
    pair.master
        .resize(PtySize {
            rows: 8,
            cols: 12,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("resize");

    until(&reader, &mut output, b"STREAMED-BEFORE-TOOL");
    until_with_context(&reader, &mut output, b"allow/deny/amend", || {
        let requests = stub.requests();
        let paths: Vec<_> = requests
            .iter()
            .map(|request| request.path.clone())
            .collect();
        format!("stub request paths: {paths:?}")
    });
    input.write_all(b"allow\r").expect("permission answer");
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
    until_from(&reader, &mut output, unsent_stash_start, b"> ");
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
    let deadline = Instant::now() + Duration::from_secs(8);
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

    let streamed = find(&output, b"STREAMED-BEFORE-TOOL");
    let permission = find(&output, b"allow/deny/amend");
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
        "--ephemeral",
        "--sandbox",
        "off",
    ]);
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
    let deadline = Instant::now() + Duration::from_secs(8);
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
