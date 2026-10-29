use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

// Catches: completed tool rows retain launch width after a PTY grows or shrinks.
#[test]
fn tool_rows_follow_real_pty_growth_and_shrink() {
    width_fixture(false);
}

// Catches: a late cursor reply abandons the admitted prompt before generation.
#[test]
fn late_cursor_reply_does_not_drop_width_submit() {
    width_fixture(true);
}

struct LateCursorReply {
    writer: Box<dyn Write + Send>,
    delay_next: Arc<AtomicBool>,
}

impl Write for LateCursorReply {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.starts_with(b"\x1b[")
            && bytes.ends_with(b"R")
            && self.delay_next.swap(false, Ordering::SeqCst)
        {
            // Fault injection, not a readiness wait: exceed Crossterm's 2000 ms
            // CPR budget while still delivering the real terminal's reply.
            thread::sleep(Duration::from_millis(2500));
        }
        self.writer.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

fn width_fixture(late_reply: bool) {
    let command = format!("printf '%s' {}", "live-width-OGW-".repeat(20));
    let args = serde_json::json!({"argv": ["/bin/sh", "-c", command]}).to_string();
    let mut completions = Vec::new();
    for index in 0..3 {
        completions.push(Reply::Sse(tool_call(
            &format!("width-{index}"),
            "exec",
            &args,
        )));
        completions.push(Reply::Sse(text(&format!("WIDTH-OGW-DONE-{index}"))));
    }
    let stub = Stub::start(Plan {
        completions,
        ..Plan::default()
    });
    let ego = Ego::against(&stub)
        .conversing()
        .env("TERM", "xterm-256color")
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--mode",
            "yolo",
            "--trust-workspace",
        ]);
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 40,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let child = pair.slave.spawn_command(ego.pty_command()).unwrap();
    let delay_next = Arc::new(AtomicBool::new(false));
    let writer = LateCursorReply {
        writer: pair.master.take_writer().unwrap(),
        delay_next: Arc::clone(&delay_next),
    };
    let (mut input, responder, writer_join) = writer_owner(Box::new(writer));
    let size = Arc::new(Mutex::new((40, 80)));
    let (reader, reader_join) = stream_with_resizable_screen(
        pair.master.try_clone_reader().unwrap(),
        responder,
        Some(Arc::clone(&size)),
    );
    let session = PtySession {
        child,
        pair,
        reader_join,
        writer_join,
    };
    let mut output = Vec::new();
    until_prompt_from(&reader, &mut output, 0);
    let mut captures = Vec::new();
    for (index, width) in [80, 200, 60].into_iter().enumerate() {
        drain_until_quiet(&reader, &mut output);
        let resize_start = output.len();
        *size.lock().unwrap() = (40, width);
        session
            .pair
            .master
            .resize(PtySize {
                rows: 40,
                cols: width,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        if late_reply && index == 2 {
            // Isolate the admission query from the resize's own cursor queries.
            until_prompt_from(&reader, &mut output, resize_start);
            drain_until_quiet(&reader, &mut output);
            delay_next.store(true, Ordering::SeqCst);
        }
        let start = output.len();
        input.write_all(b"run the width fixture\r").unwrap();
        input.flush().unwrap();
        until_normalized(
            &reader,
            &mut output,
            start,
            &format!("WIDTH-OGW-DONE-{index}"),
        );
        until_mcp_composer_is_idle(&reader, &mut output);
        drain_until_quiet(&reader, &mut output);
        captures.push((width, normalized(&output[start..])));
    }
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    for (width, capture) in captures {
        let row = capture
            .lines()
            .map(|line| line.rsplit('\r').next().unwrap_or(line))
            .find(|line| line.starts_with("✓ $ "))
            .unwrap_or_else(|| panic!("missing completed tool row: {capture:?}"));
        let cells = unicode_width::UnicodeWidthStr::width(row.trim_end_matches('\r'));
        assert!(
            cells <= usize::from(width),
            "row overflows live width {width}: {row:?}"
        );
        assert!(
            cells > usize::from(width) - 20,
            "row retains a stale width at {width}: {row:?}"
        );
    }
}
