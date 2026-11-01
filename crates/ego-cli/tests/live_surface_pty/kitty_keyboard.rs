use super::*;

// Catches: Ctrl+Enter parses in tests but cannot be produced by a compliant host
// because the REPL never queries or enables keyboard disambiguation.
#[test]
fn kitty_keyboard_negotiation_preserves_busy_steer_and_balances_modes() {
    let steer = "steer now with ctrl enter";
    let (initial, initial_gate) = gated_paced_line("STREAMED-BEFORE-STEER");
    let stub = Stub::start(Plan {
        completions: vec![initial, Reply::Sse(text("STEER-RECEIVED"))],
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
    ego.file("AGENTS.md", "# live surface fixture\n");
    let (session, mut input, reader) = start(&ego);
    let mut output = Vec::new();

    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").expect("trust answer");
    input.write_all(b"initial\r").expect("initial prompt");
    input.flush().expect("initial prompt flush");
    until(&reader, &mut output, b"STREAMED-BEFORE-STEER");

    let steer_start = output.len();
    input.write_all(steer.as_bytes()).expect("steer text");
    input.write_all(b"\x1b[13;5u").expect("Ctrl+Enter");
    input.flush().expect("Ctrl+Enter flush");
    until_normalized(&reader, &mut output, steer_start, "interrupt pending");
    initial_gate.release();
    until(&reader, &mut output, b"STEER-RECEIVED");
    until_prompt_from(&reader, &mut output, steer_start);
    input.write_all(b"/quit\r").expect("quit command");
    input.flush().expect("quit command flush");
    let status = session.quit(input);
    drain(&reader, &mut output);
    assert_balanced(&output);
    assert!(status.success(), "interactive process failed: {status:?}");

    let generations = stub.generations();
    assert_eq!(
        generations.len(),
        2,
        "one interrupted turn, one steered turn"
    );
    assert_eq!(
        generations
            .iter()
            .map(|generation| count_exact_strings(generation, steer))
            .collect::<Vec<_>>(),
        vec![0, 1],
        "the steer reaches the provider once, in the next request"
    );
}

// This terminal peer implements the Kitty support query and stack. It observes
// real REPL output; it does not enable the application's mode on its behalf.
struct Terminal {
    reader: Box<dyn Read + Send>,
    responder: mpsc::SyncSender<WriteRequest>,
    pending: Vec<u8>,
    flags: u8,
    stack: Vec<u8>,
    supported: bool,
    disabled_reply: Vec<u8>,
    enabled_reply: Vec<u8>,
    attributes_reply: Vec<u8>,
    replies_attributes: bool,
}

impl Read for Terminal {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.reader.read(buffer)?;
        for &byte in &buffer[..count] {
            if byte == 0x1b {
                self.pending.clear();
                self.pending.push(byte);
            } else if !self.pending.is_empty() {
                self.pending.push(byte);
                if self.pending.len() > 2 && (b'@'..=b'~').contains(&byte) {
                    let response = match self.pending.as_slice() {
                        b"\x1b[?u" if self.supported => Some(if self.flags == 0 {
                            self.disabled_reply.clone()
                        } else {
                            self.enabled_reply.clone()
                        }),
                        b"\x1b[c" if self.replies_attributes => Some(self.attributes_reply.clone()),
                        b"\x1b[>1u" => {
                            self.stack.push(self.flags);
                            self.flags = 1;
                            None
                        }
                        b"\x1b[<1u" => {
                            self.flags = self.stack.pop().unwrap_or(0);
                            None
                        }
                        _ => None,
                    };
                    if let Some(response) = response {
                        self.responder.send(WriteRequest::Bytes(response)).ok();
                        self.responder.send(WriteRequest::Flush).ok();
                    }
                    self.pending.clear();
                }
            }
        }
        Ok(count)
    }
}

pub(super) fn terminal(
    reader: Box<dyn Read + Send>,
    responder: mpsc::SyncSender<WriteRequest>,
) -> Box<dyn Read + Send> {
    terminal_with_support(reader, responder, true)
}

fn terminal_with_support(
    reader: Box<dyn Read + Send>,
    responder: mpsc::SyncSender<WriteRequest>,
    supported: bool,
) -> Box<dyn Read + Send> {
    terminal_with_replies(reader, responder, supported, true)
}

fn terminal_with_replies(
    reader: Box<dyn Read + Send>,
    responder: mpsc::SyncSender<WriteRequest>,
    supported: bool,
    replies_attributes: bool,
) -> Box<dyn Read + Send> {
    let recorded: serde_json::Value =
        serde_json::from_str(include_str!("../fixtures/kitty_keyboard_tuic.json")).unwrap();
    let reply = |key: &str| recorded[key].as_str().unwrap().as_bytes().to_vec();
    Box::new(Terminal {
        reader,
        responder,
        pending: Vec::new(),
        flags: 0,
        stack: Vec::new(),
        supported,
        disabled_reply: reply("disabled_flags"),
        enabled_reply: reply("enabled_flags"),
        attributes_reply: reply("device_attributes"),
        replies_attributes,
    })
}

fn start(ego: &Ego) -> (PtySession, PtyWriter, mpsc::Receiver<Vec<u8>>) {
    start_command(ego.pty_command(), true)
}

fn start_command(
    command: portable_pty::CommandBuilder,
    supported: bool,
) -> (PtySession, PtyWriter, mpsc::Receiver<Vec<u8>>) {
    start_command_with_replies(command, supported, true)
}

fn start_command_with_replies(
    command: portable_pty::CommandBuilder,
    supported: bool,
    replies_attributes: bool,
) -> (PtySession, PtyWriter, mpsc::Receiver<Vec<u8>>) {
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(command).expect("spawn");
    let (input, responder, writer_join) = writer_owner(pair.master.take_writer().expect("writer"));
    let reader = terminal_with_replies(
        pair.master.try_clone_reader().expect("reader"),
        responder.clone(),
        supported,
        replies_attributes,
    );
    let (reader, reader_join) = stream(reader, responder);
    (
        PtySession {
            child,
            pair,
            reader_join,
            writer_join,
        },
        input,
        reader,
    )
}

fn assert_balanced(output: &[u8]) {
    assert!(
        output.windows(4).any(|bytes| bytes == b"\x1b[?u"),
        "REPL did not query Kitty support"
    );
    let pushes = output
        .windows(5)
        .filter(|bytes| *bytes == b"\x1b[>1u")
        .count();
    let pops = output
        .windows(5)
        .filter(|bytes| *bytes == b"\x1b[<1u")
        .count();
    assert!(pushes > 0, "REPL did not enable keyboard disambiguation");
    assert_eq!(pushes, pops, "keyboard stack leaked on exit");
}

// Catches: a job stops with its Kitty mode still pushed, or a modal consumes
// Ctrl+Z as its answer instead of resuming the same unanswered prompt.
#[cfg(unix)]
#[test]
fn kitty_keyboard_suspend_balances_modes_for_composer_busy_and_modal() {
    for phase in ["idle", "busy", "modal"] {
        let (reply, gate) = gated_paced_line("KITTY-760-RUNNING");
        let stub = Stub::start(Plan {
            completions: vec![reply],
            ..Plan::default()
        });
        let ego = job_ego(&stub, "default");
        let mut job = Job::start_with_keyboard(&ego, 24, 80, true);
        if phase == "busy" {
            let mark = job.output.len();
            job.send(b"hold turn\r");
            // Token traffic proves the gated response reached the busy turn.
            // Its text can remain buffered until the response completes; waiting
            // for that text before releasing its tail creates a circular wait.
            until_normalized(&job.reader, &mut job.output, mark, "↓");
            assert_eq!(stub.generations().len(), 1, "busy turn reached provider");
        }
        let expected = if phase == "modal" {
            job.send(b"/mode\r");
            "Enter answers; Esc cancels."
        } else {
            job.send(b"KITTY-760-DRAFT");
            "KITTY-760-DRAFT"
        };
        until_normalized(&job.reader, &mut job.output, 0, expected);
        job.suspend();
        let stopped_cooked = job.cooked();
        let stopped = job.output.clone();
        let resume = job.output.len();
        job.resume();
        until_normalized(&job.reader, &mut job.output, resume, expected);
        until_from(&job.reader, &mut job.output, resume, b"\x1b[>1u");
        let resumed_raw = !job.cooked();
        if phase == "modal" {
            let mark = job.output.len();
            job.send(b"\x1b");
            until_prompt_from(&job.reader, &mut job.output, mark);
        }
        if phase == "busy" {
            job.send(b"\x15");
            gate.release();
            until_mcp_composer_is_idle(&job.reader, &mut job.output);
        }
        job.quit(phase == "idle");
        assert!(stopped_cooked, "{phase}: shell received a raw tty");
        assert!(resumed_raw, "{phase}: resume left tty cooked");
        assert_balanced(&stopped);
    }
}

// Catches: an unsupported terminal gets a mode push, or plain CR no longer
// submits after the support query reports only primary device attributes.
#[cfg(unix)]
#[test]
fn kitty_keyboard_unsupported_terminal_keeps_plain_enter_fallback() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("KITTY-760-FALLBACK"))],
        ..Plan::default()
    });
    let ego = job_ego(&stub, "default");
    let (session, mut input, reader) = start_command(ego.pty_command(), false);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\rfallback prompt\r").unwrap();
    input.flush().unwrap();
    until(&reader, &mut output, b"KITTY-760-FALLBACK");
    until_mcp_composer_is_idle(&reader, &mut output);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    drain(&reader, &mut output);
    assert!(output.windows(4).any(|bytes| bytes == b"\x1b[?u"));
    assert!(!output.windows(5).any(|bytes| bytes == b"\x1b[>1u"));
    assert!(!output.windows(5).any(|bytes| bytes == b"\x1b[<1u"));
    assert_eq!(stub.generations().len(), 1);
}

// A separate test process lets the real Reedline guard unwind with a live tty.
// It is not an ego production hook and cannot affect an installed binary.
#[test]
#[ignore = "PTY child of kitty_keyboard_unwind_pops_active_mode"]
fn kitty_keyboard_unwind_child() {
    struct PanicMode;
    impl reedline::EditMode for PanicMode {
        fn parse_event(&mut self, _: reedline::ReedlineRawEvent) -> reedline::ReedlineEvent {
            panic!("intentional input-boundary unwind")
        }
        fn edit_mode(&self) -> reedline::PromptEditMode {
            reedline::PromptEditMode::Emacs
        }
    }
    std::panic::set_hook(Box::new(|_| {}));
    let result = std::panic::catch_unwind(|| {
        let mut editor = reedline::Reedline::create()
            .with_edit_mode(Box::new(PanicMode))
            .use_kitty_keyboard_enhancement(true);
        editor.read_line(&reedline::DefaultPrompt::default()).ok();
    });
    assert!(result.is_err());
    println!("KITTY-760-UNWOUND");
}

// Catches: the active mode survives unwinding because cleanup only runs on the
// successful read_line return path, not when the editor itself is dropped.
#[test]
fn kitty_keyboard_unwind_pops_active_mode() {
    let mut command = portable_pty::CommandBuilder::new(std::env::current_exe().unwrap());
    command.args([
        "--exact",
        "kitty_keyboard::kitty_keyboard_unwind_child",
        "--ignored",
        "--nocapture",
    ]);
    let (session, mut input, reader) = start_command(command, true);
    let mut output = Vec::new();
    until(&reader, &mut output, b"\x1b[>1u");
    input.write_all(b"x").unwrap();
    input.flush().unwrap();
    until(&reader, &mut output, b"KITTY-760-UNWOUND");
    assert!(session.quit(input).success());
    drain(&reader, &mut output);
    assert_balanced(&output);
}

// Catches: startup refusal leaves enhancement pushed after dropping the seat,
// before the main composer has ever accepted a prompt.
#[test]
fn kitty_keyboard_startup_refusal_leaves_no_mode_pushed() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/not-installed",
        "--trust-workspace",
        "--sandbox",
        "off",
    ]);
    let (session, input, reader) = start(&ego);
    let status = session.quit(input);
    let mut output = Vec::new();
    drain(&reader, &mut output);
    assert!(!status.success(), "unknown model must refuse startup");
    assert!(
        output.windows(4).any(|bytes| bytes == b"\x1b[?u"),
        "fixture did not reach terminal seat creation"
    );
    assert_eq!(
        output
            .windows(5)
            .filter(|bytes| *bytes == b"\x1b[>1u")
            .count(),
        output
            .windows(5)
            .filter(|bytes| *bytes == b"\x1b[<1u")
            .count(),
        "startup refusal leaked a keyboard stack entry"
    );
    assert!(stub.generations().is_empty());
}

// Catches: the query timeout delays a silent terminal's composer past one second.
#[test]
fn kitty_keyboard_silent_terminal_reaches_composer_within_one_second() {
    bounded_keyboard_query(false);
}

// Catches: a flags reply enters an unbounded read for missing device attributes.
#[test]
fn kitty_keyboard_flags_only_reply_does_not_hang_startup() {
    bounded_keyboard_query(true);
}

fn bounded_keyboard_query(supported: bool) {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("KITTY-ANSWER"))],
        ..Plan::default()
    });
    let ego = job_ego(&stub, "default").args(["--trust-workspace"]);
    let started = Instant::now();
    let (mut session, mut input, reader) =
        start_command_with_replies(ego.pty_command(), supported, false);
    let mut output = Vec::new();
    let deadline = started + Duration::from_secs(1);
    while rendered_prompt_count(&output) == 0 {
        match reader.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(bytes) => output.extend(bytes),
            Err(_) => break,
        }
    }
    let ready = rendered_prompt_count(&output) > 0;
    if ready {
        input.write_all(b"keyboard fallback prompt\r").unwrap();
        input.flush().unwrap();
        until(&reader, &mut output, b"KITTY-ANSWER");
        until_mcp_composer_is_idle(&reader, &mut output);
        input.write_all(b"/quit\r").unwrap();
        input.flush().unwrap();
    } else {
        session.child.kill().expect("kill stuck query child");
    }
    let status = session.quit(input);
    drain(&reader, &mut output);
    assert!(
        ready,
        "keyboard query blocked composer for one second: {:?}",
        String::from_utf8_lossy(&output)
    );
    assert!(status.success(), "REPL exit: {status:?}");
    assert!(output.windows(4).any(|bytes| bytes == b"\x1b[?u"));
    if supported {
        assert_balanced(&output);
    } else {
        assert!(!output.windows(5).any(|bytes| bytes == b"\x1b[>1u"));
    }
    assert_eq!(stub.generations().len(), 1);
}
