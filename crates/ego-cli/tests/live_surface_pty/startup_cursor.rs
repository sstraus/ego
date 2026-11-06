use super::*;

// Catches: the first missing cursor report silently exits before a composer
// appears, instead of recovering when the terminal answers the next query.
#[test]
fn missing_first_cursor_reply_recovers_an_interactive_startup() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "off",
        "--trust-workspace",
    ]);
    ego.file("AGENTS.md", "# Startup cursor fixture\n");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let child = pair.slave.spawn_command(ego.pty_command()).unwrap();
    struct Cleanup(Option<Box<dyn portable_pty::ChildKiller + Send + Sync>>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            if let Some(killer) = &mut self.0 {
                let _ = killer.kill();
            }
        }
    }
    let mut cleanup = Cleanup(Some(child.clone_killer()));
    let (mut input, responder, writer_join) = writer_owner(pair.master.take_writer().unwrap());
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let terminal = MissingFirstCursorQuery {
        reader: pair.master.try_clone_reader().unwrap(),
        seen: Vec::new(),
        dropped: Arc::clone(&dropped),
    };
    let (reader, reader_join) = stream_with_screen(Box::new(terminal), responder, Some((24, 120)));
    let mut session = PtySession {
        child,
        pair,
        reader_join,
        writer_join,
    };
    let mut output = Vec::new();
    until_prompt_from(&reader, &mut output, 0);
    assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
    assert!(session.child.try_wait().unwrap().is_none());
    let start = output.len();
    input.write_all(b"/help\r").unwrap();
    input.flush().unwrap();
    until_normalized(
        &reader,
        &mut output,
        start,
        "Unknown slash input is refused locally",
    );
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    // The process has already exited and been reaped.
    cleanup.0 = None;
    assert!(stub.generations().is_empty());
}

struct MissingFirstCursorQuery {
    reader: Box<dyn Read + Send>,
    seen: Vec<u8>,
    dropped: Arc<std::sync::atomic::AtomicBool>,
}

// Catches: a startup CPR arriving after fallback becomes draft text or moves
// the recovered composer instead of being ignored as an obsolete report.
#[test]
fn late_startup_cursor_reply_keeps_the_composer_empty_and_usable() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "off",
        "--trust-workspace",
    ]);
    ego.file("AGENTS.md", "# Late startup cursor fixture\n");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let child = pair.slave.spawn_command(ego.pty_command()).unwrap();
    struct Cleanup(Option<Box<dyn portable_pty::ChildKiller + Send + Sync>>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            if let Some(killer) = &mut self.0 {
                let _ = killer.kill();
            }
        }
    }
    let mut cleanup = Cleanup(Some(child.clone_killer()));
    let (delivered, delivery) = mpsc::channel();
    let writer = LateStartupCursorReply {
        writer: pair.master.take_writer().unwrap(),
        delivered: Some(delivered),
    };
    let (mut input, responder, writer_join) = writer_owner(Box::new(writer));
    let (reader, reader_join) = stream_with_screen(
        pair.master.try_clone_reader().unwrap(),
        responder,
        Some((24, 120)),
    );
    let session = PtySession {
        child,
        pair,
        reader_join,
        writer_join,
    };
    let mut output = Vec::new();
    until_prompt_from(&reader, &mut output, 0);
    assert!(
        matches!(delivery.try_recv(), Err(mpsc::TryRecvError::Empty)),
        "fallback must render before the delayed CPR arrives"
    );
    drain_until_quiet(&reader, &mut output);
    let mut before_reply = vt100::Parser::new(24, 120, 100);
    before_reply.process(&output);
    let before_reply = before_reply.screen().contents();
    let reply = delivery.recv_timeout(PROGRESS_DEADLINE).unwrap();
    let late = output.len();
    drain_until_quiet(&reader, &mut output);
    let mut terminal = vt100::Parser::new(24, 120, 100);
    terminal.process(&output);
    assert_eq!(
        terminal.screen().contents(),
        before_reply,
        "a late CPR must not change the recovered screen"
    );
    assert!(
        !output[late..]
            .windows(reply.len())
            .any(|bytes| bytes == reply),
        "the terminal reply must not be echoed"
    );
    assert!(
        !output[late..]
            .windows(DSR_QUERY.len())
            .any(|bytes| bytes == DSR_QUERY),
        "a late report must not trigger another discovery"
    );
    let help = output.len();
    input.write_all(b"/help\r").unwrap();
    input.flush().unwrap();
    until_normalized(
        &reader,
        &mut output,
        help,
        "Unknown slash input is refused locally",
    );
    until_prompt_from(&reader, &mut output, help);
    let draft = "LATE-UNSENT-DRAFT";
    input.write_all(draft.as_bytes()).unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, help, draft);
    drain_until_quiet(&reader, &mut output);
    let mut terminal = vt100::Parser::new(24, 120, 100);
    terminal.process(&output);
    assert!(terminal.screen().contents().contains(draft));
    input.write_all(b"\x15/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    cleanup.0 = None;
    assert!(
        stub.generations().is_empty(),
        "CPR bytes must not prefix /help"
    );
}

struct LateStartupCursorReply {
    writer: Box<dyn Write + Send>,
    delivered: Option<mpsc::Sender<Vec<u8>>>,
}

impl Write for LateStartupCursorReply {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.starts_with(b"\x1b[")
            && bytes.ends_with(b"R")
            && let Some(delivered) = self.delivered.take()
        {
            // Fault injection: deliver the real VT-derived CPR after the
            // production deadline, matching the observed slow host response.
            thread::sleep(Duration::from_secs(4));
            self.writer.write_all(bytes)?;
            self.writer.flush()?;
            let _ = delivered.send(bytes.to_vec());
            return Ok(bytes.len());
        }
        self.writer.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

impl Read for MissingFirstCursorQuery {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let length = self.reader.read(buffer)?;
        for byte in &mut buffer[..length] {
            self.seen.push(*byte);
            if self.seen.ends_with(DSR_QUERY)
                && !self.dropped.swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                // Hide only this query from the responder so the production
                // cursor query really times out.
                *byte = b'c';
            }
        }
        Ok(length)
    }
}
