use super::*;
use support::ReleaseGate;

// Catches: one missing cursor report kills the editor while its admitted wake
// keeps running; the next question then exits with a headless resume hint.
#[test]
fn lost_cursor_reply_after_wake_does_not_kill_the_question_owner() {
    let question = serde_json::json!({
        "prompt": "Answer after the cursor timeout", "mode": "free_text",
        "options": [], "free_text": true,
    });
    let gate = ReleaseGate::new();
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::GatedPaced {
                head: String::new(),
                tail: tool_call(
                    "lost-cursor-question",
                    "ask_question",
                    &question.to_string(),
                ),
                gate: Arc::clone(&gate),
            },
            Reply::Sse(text("CURSOR-ANSWERED")),
        ],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "off",
        "--yolo",
        "--trust-workspace",
    ]);
    ego.file("AGENTS.md", "# Lost cursor reply fixture\n");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let child = pair.slave.spawn_command(ego.pty_command()).unwrap();
    struct Cleanup(
        Option<Box<dyn portable_pty::ChildKiller + Send + Sync>>,
        Arc<ReleaseGate>,
    );
    impl Drop for Cleanup {
        fn drop(&mut self) {
            self.1.release();
            if let Some(killer) = &mut self.0 {
                let _ = killer.kill();
            }
        }
    }
    let mut cleanup = Cleanup(Some(child.clone_killer()), Arc::clone(&gate));
    let (mut input, responder, writer_join) = writer_owner(pair.master.take_writer().unwrap());
    let terminal = MissingCursorReply {
        reader: pair.master.try_clone_reader().unwrap(),
        seen: Vec::new(),
        gate: Some(gate),
        cursor_query: None,
    };
    let (reader, reader_join) = stream_with_screen(Box::new(terminal), responder, Some((40, 120)));
    let mut session = PtySession {
        child,
        pair,
        reader_join,
        writer_join,
    };
    let mut output = Vec::new();
    until_prompt_from(&reader, &mut output, 0);
    input
        .write_all(b"BG DONE exit=0 log=/tmp/811.log cmd=true\r")
        .unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, 0, "Answer after the cursor timeout\n");
    assert!(
        session.child.try_wait().unwrap().is_none(),
        "the question must stay interactive"
    );
    let question_end = output.len();
    input.write_all(b"continue\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, 0, "CURSOR-ANSWERED");
    until_prompt_from(&reader, &mut output, 0);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    // The process was reaped normally; the guard is only for failing runs.
    cleanup.0 = None;
    if let Some(directory) = std::env::var_os("EGO_WAKE_QUESTION_CAPTURE_DIR") {
        let directory = std::path::PathBuf::from(directory);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("question.ansi"), &output[..question_end]).unwrap();
        std::fs::write(directory.join("answered.ansi"), &output).unwrap();
        std::fs::write(
            directory.join("geometry.json"),
            b"{\"rows\":40,\"cols\":120}",
        )
        .unwrap();
    }
}

struct MissingCursorReply {
    reader: Box<dyn Read + Send>,
    seen: Vec<u8>,
    gate: Option<Arc<ReleaseGate>>,
    cursor_query: Option<std::sync::mpsc::SyncSender<()>>,
}

impl Read for MissingCursorReply {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let length = self.reader.read(buffer)?;
        for byte in &mut buffer[..length] {
            self.seen.push(*byte);
            if self.seen.ends_with(b"background task done")
                && let Some(gate) = self.gate.take()
            {
                let (query, received) = std::sync::mpsc::sync_channel(0);
                self.cursor_query = Some(query);
                thread::spawn(move || {
                    // A retained cursor anchor needs no new query. Admit the
                    // question even when that optional request never arrives.
                    if received.recv_timeout(Duration::from_secs(3)).is_ok() {
                        thread::sleep(Duration::from_millis(2300));
                    }
                    drop(received);
                    gate.release();
                });
            }
            if self.seen.ends_with(b"\x1b[6n")
                && let Some(query) = self.cursor_query.take()
                && query.send(()).is_ok()
            {
                // Hide this request from the DSR responder. The child really
                // waits for its OS query timeout before the provider proceeds.
                *byte = b'c';
            }
        }
        Ok(length)
    }
}

// Catches: a background completion admits work but loses the terminal owner,
// so a subsequent question exits with the headless resume contract.
#[test]
fn background_wake_question_keeps_the_terminal_alive() {
    for (width, busy) in [(60, false), (120, false), (120, true)] {
        let (first, gate) = gated_paced_line("WAKE-RUNNING");
        let question = serde_json::json!({
            "prompt": "Answer the wake question",
            "mode": "free_text",
            "options": [],
            "free_text": true,
        });
        let mut completions = vec![
            Reply::Sse(tool_call(
                "wake-question",
                "ask_question",
                &question.to_string(),
            )),
            Reply::Sse(text("WAKE-ANSWERED")),
            Reply::Sse(text("WAKE-NEXT-PROMPT")),
        ];
        if busy {
            completions.insert(0, first);
        }
        let stub = Stub::start(Plan {
            completions,
            ..Plan::default()
        });
        let ego = Ego::against(&stub).conversing().args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--yolo",
            "--trust-workspace",
        ]);
        ego.file("AGENTS.md", "# Wake question fixture\n");
        let (mut session, mut input, reader) =
            PtySession::start(&ego, 40, width, Some((40, width)));
        let mut output = Vec::new();
        until_prompt_from(&reader, &mut output, 0);
        if busy {
            input.write_all(b"start gated turn\r").unwrap();
            input.flush().unwrap();
            until(&reader, &mut output, b"WAKE-RUNNING");
        }
        let wake = "BG DONE exit=100 log=/home/user/work/.tmp/wake-question-red.log cmd=env JOB_SESSION=8f982c66-afd5-4841-aef2-1bf914339810 env FORCE_REMOTE=1 /home/user/work/projects/build-tools/remote-build/run-test…";
        input.write_all(b"\x15").unwrap();
        input.write_all(wake.as_bytes()).unwrap();
        input.write_all(b"\r").unwrap();
        input.flush().unwrap();
        if busy {
            drain_until_quiet(&reader, &mut output);
            gate.release();
        }
        until_normalized(&reader, &mut output, 0, "Answer the wake question\n");
        assert!(
            session.child.try_wait().unwrap().is_none(),
            "question must wait for input"
        );
        input.write_all(b"continue\r").unwrap();
        input.flush().unwrap();
        until_normalized(&reader, &mut output, 0, "WAKE-ANSWERED");
        until_prompt_from(&reader, &mut output, 0);
        let next = output.len();
        input.write_all(b"next prompt\r").unwrap();
        input.flush().unwrap();
        until_normalized(&reader, &mut output, next, "WAKE-NEXT-PROMPT");
        until_prompt_from(&reader, &mut output, next);
        input.write_all(b"/quit\r").unwrap();
        input.flush().unwrap();
        assert!(session.quit(input).success());
        let generations = stub.generations();
        assert_eq!(
            count_exact_strings(&generations[usize::from(busy)]["messages"], wake),
            1
        );
        let answer = generations[usize::from(busy) + 1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "tool" && message["tool_call_id"] == "wake-question")
            .expect("question answer reaches the provider");
        assert!(answer.to_string().contains("continue"));
    }
}
