mod support;

use portable_pty::{NativePtySystem, PtySize, PtySystem};
use std::{
    io::{self, Read, Write},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use support::{Ego, Plan, Reply, Stub, paced_tool_call, text};

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

fn find(output: &[u8], needle: &[u8]) -> usize {
    output
        .windows(needle.len())
        .position(|window| window == needle)
        .unwrap_or_else(|| panic!("missing PTY marker {:?}", String::from_utf8_lossy(needle)))
}

#[test]
fn live_surface_preserves_draft_through_busy_permission_and_resize() {
    let draft = "fn draft() {\n    println!(\"draft e\\u{301} 👩‍💻\");\n}\n";
    let write_arguments = serde_json::json!({
        "path": "notes.md",
        "contents": "permission",
    })
    .to_string();
    let stub = Stub::start(Plan {
        completions: vec![
            paced_tool_call(
                "write-1",
                "write",
                &write_arguments,
                Duration::from_millis(20),
            ),
            Reply::Sse(text("WRITE-COMPLETE")),
            Reply::Sse(text("DRAFT-RECEIVED")),
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
    input.write_all(b"\x1b[200~").expect("paste prefix");
    input.write_all(draft.as_bytes()).expect("paste draft");
    input.write_all(b"\x1b[201~").expect("paste suffix");
    input.flush().expect("paste flush");
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
    until(&reader, &mut output, b"WRITE-COMPLETE");

    let write_complete = find(&output, b"WRITE-COMPLETE") + b"WRITE-COMPLETE".len();
    until_normalized(&reader, &mut output, write_complete, draft);
    input.write_all(b"\r").expect("idle submit");
    input.flush().expect("idle submit flush");
    until(&reader, &mut output, b"DRAFT-RECEIVED");
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
    drain(&reader, &mut output);

    assert!(!output.windows(8).any(|window| window == b"\x1b[?1049h"));
    assert!(!output.windows(8).any(|window| window == b"\x1b[?1000h"));
    assert!(!output.windows(8).any(|window| window == b"\x1b[?1006h"));
    assert!(output.windows(8).any(|window| window == b"\x1b[?2004h"));
    assert!(output.windows(8).any(|window| window == b"\x1b[?2004l"));

    let streamed = find(&output, b"STREAMED-BEFORE-TOOL");
    let permission = find(&output, b"allow/deny/amend");
    let write_complete = find(&output, b"WRITE-COMPLETE");
    let draft_received = find(&output, b"DRAFT-RECEIVED");
    assert!(streamed < permission);
    assert!(permission < write_complete);
    assert!(write_complete < draft_received);
    assert!(normalized(&output[write_complete..]).contains(draft));

    let generations = stub.generations();
    assert_eq!(generations.len(), 3);
    assert_eq!(
        generations
            .iter()
            .map(|generation| count_exact_strings(generation, draft))
            .sum::<usize>(),
        1
    );
}
