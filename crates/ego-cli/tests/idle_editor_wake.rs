//! Catches: installing the seat's asynchronous producers turns an idle editor into a timer loop.
#![cfg(unix)]

use std::io::{Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[test]
fn idle_editor_does_not_wake_periodically() {
    const CHILD: &str = "EGO_1020_IDLE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let callbacks = Arc::new(AtomicUsize::new(0));
        let observed = callbacks.clone();
        let break_signal = reedline::BreakSignal::default();
        let mut editor = reedline::Reedline::create()
            .with_break_signal(break_signal.clone())
            .with_external_printer(reedline::ExternalPrinter::default())
            .with_geometry_idle_callback(Box::new(move || {
                observed.fetch_add(1, Ordering::SeqCst);
                false
            }));
        let repaint = editor.repaint_signal();
        let producer = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            while callbacks.load(Ordering::SeqCst) == 0 {
                assert!(
                    Instant::now() < deadline,
                    "editor never entered its input loop"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            let before = callbacks.load(Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(650));
            let idle_wakes = callbacks.load(Ordering::SeqCst) - before;
            break_signal.request_break();
            repaint.request_repaint();
            idle_wakes
        });
        assert!(matches!(
            editor
                .read_line(&reedline::DefaultPrompt::default())
                .unwrap(),
            reedline::Signal::ExternalBreak(_)
        ));
        let idle_wakes = producer.join().unwrap();
        assert!(
            idle_wakes <= 1,
            "idle editor woke {idle_wakes} times in 650 ms without input or a producer"
        );
        return;
    }

    run_in_pty("idle_editor_does_not_wake_periodically", "idle");
}

fn run_in_pty(test: &str, case: &str) {
    let pair = portable_pty::native_pty_system()
        .openpty(portable_pty::PtySize {
            rows: 24,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = portable_pty::CommandBuilder::new(std::env::current_exe().unwrap());
    command.args(["--exact", test, "--nocapture"]);
    command.env("EGO_1020_IDLE_CHILD", "1");
    command.env("EGO_1020_CASE", case);
    command.env("TERM", "xterm-256color");
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let writer = Arc::new(Mutex::new(pair.master.take_writer().unwrap()));
    let input = writer.clone();
    let mut reader = pair.master.try_clone_reader().unwrap();
    let output = Arc::new(Mutex::new(Vec::new()));
    let captured = output.clone();
    std::thread::spawn(move || {
        let mut pending = Vec::new();
        let mut buffer = [0; 4096];
        while let Ok(count) = reader.read(&mut buffer) {
            if count == 0 {
                break;
            }
            captured.lock().unwrap().extend_from_slice(&buffer[..count]);
            pending.extend_from_slice(&buffer[..count]);
            while let Some(at) = pending.windows(4).position(|bytes| bytes == b"\x1b[6n") {
                pending.drain(..at + 4);
                let mut writer = writer.lock().unwrap();
                writer.write_all(b"\x1b[1;1R").unwrap();
                writer.flush().unwrap();
            }
            if pending.len() > 4096 {
                pending.drain(..pending.len() - 3);
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut resized = false;
    let mut submitted = false;
    loop {
        // Terminal output is the oracle. Only after the producer's bytes arrive
        // do we resize and type, so a missed wake cannot hide behind key input.
        if case != "idle" && case != "break" {
            let bytes = output.lock().unwrap().clone();
            let marker: &[u8] = match case {
                "printer" => b"PRINTER-1020",
                "repaint" => b"REPAINT-1020",
                _ => b"METADATA-1020",
            };
            if !resized && bytes.windows(marker.len()).any(|part| part == marker) {
                pair.master
                    .resize(portable_pty::PtySize {
                        rows: 30,
                        cols: 80,
                        pixel_width: 0,
                        pixel_height: 0,
                    })
                    .unwrap();
                resized = true;
            }
            if resized && !submitted && bytes.windows(7).any(|part| part == b"WIDTH80") {
                let mut input = input.lock().unwrap();
                input.write_all(b"typed\r").unwrap();
                input.flush().unwrap();
                submitted = true;
            }
        }
        if let Some(status) = child.try_wait().unwrap() {
            assert!(
                status.success(),
                "{}",
                String::from_utf8_lossy(&output.lock().unwrap())
            );
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let _ = child.wait();
            panic!(
                "idle editor did not wake: {}",
                String::from_utf8_lossy(&output.lock().unwrap())
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

// Catches: a producer publishes work without waking an editor blocked on input,
// or changing the wait loses subsequent resize and key events.
#[test]
fn idle_editor_wakes_each_external_producer_without_input() {
    if std::env::var_os("EGO_1020_IDLE_CHILD").is_none() {
        for case in ["printer", "repaint", "metadata", "before_wait", "break"] {
            run_in_pty(
                "idle_editor_wakes_each_external_producer_without_input",
                case,
            );
        }
        return;
    }
    let case = std::env::var("EGO_1020_CASE").unwrap();
    let callbacks = Arc::new(AtomicUsize::new(0));
    let observed = callbacks.clone();
    let changed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let prompt = WakePrompt(changed.clone());
    let signal = reedline::BreakSignal::default();
    let printer = reedline::ExternalPrinter::<String>::default();
    let sender = printer.sender();
    let (metadata, incoming) = crossbeam::channel::unbounded::<String>();
    let early_metadata = metadata.clone();
    let early = case == "before_wait";
    let mut editor = reedline::Reedline::create()
        .with_break_signal(signal.clone())
        .with_external_printer(printer)
        .with_geometry_idle_callback(Box::new(move || {
            for bytes in incoming.try_iter() {
                let mut out = std::io::stdout().lock();
                out.write_all(bytes.as_bytes()).unwrap();
                out.flush().unwrap();
            }
            if observed.fetch_add(1, Ordering::SeqCst) == 0 && early {
                // Publish after the callback drained its queue, before the first
                // wait has registered an OS waker. The pending latch must survive.
                early_metadata
                    .send("\x1b]777;METADATA-1020\x07".into())
                    .unwrap();
                crossterm::event::wake().unwrap();
            }
            false
        }));
    let repaint = editor.repaint_signal();
    let producer_case = case.clone();
    let producer = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        while callbacks.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(150));
        match producer_case.as_str() {
            "printer" => sender.send("PRINTER-1020".into()).unwrap(),
            "repaint" => {
                changed.store(true, Ordering::Release);
                repaint.request_repaint();
            }
            "metadata" => {
                metadata.send("\x1b]777;METADATA-1020\x07".into()).unwrap();
                crossterm::event::wake().unwrap();
            }
            "break" => signal.request_break(),
            "before_wait" => {}
            _ => unreachable!(),
        }
    });
    let result = editor.read_line(&prompt).unwrap();
    producer.join().unwrap();
    if case == "break" {
        assert!(matches!(result, reedline::Signal::ExternalBreak(buffer) if buffer.is_empty()));
    } else {
        assert!(matches!(result, reedline::Signal::Success(buffer) if buffer == "typed"));
    }
}

struct WakePrompt(Arc<std::sync::atomic::AtomicBool>);

impl reedline::Prompt for WakePrompt {
    fn render_prompt_left(&self) -> std::borrow::Cow<'_, str> {
        if self.0.load(Ordering::Acquire) {
            "REPAINT-1020".into()
        } else {
            "READY-1020".into()
        }
    }
    fn render_prompt_right(&self) -> std::borrow::Cow<'_, str> {
        format!("WIDTH{}", crossterm::terminal::size().unwrap().0).into()
    }
    fn render_prompt_indicator(&self, _: reedline::PromptEditMode) -> std::borrow::Cow<'_, str> {
        "> ".into()
    }
    fn render_prompt_multiline_indicator(&self) -> std::borrow::Cow<'_, str> {
        ".. ".into()
    }
    fn render_prompt_history_search_indicator(
        &self,
        _: reedline::PromptHistorySearch,
    ) -> std::borrow::Cow<'_, str> {
        "search ".into()
    }
}
