//! Order a real driver's idle publication inside the real model picker.
//! Only the isolated test child sees the gate; no phase is set by the test.

use crate::run::test_support as support;
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use support::{Ego, Plan, ReleaseGate, Reply, Stub, paced, text};

const CHILD: &str = "EGO_REGRESSION_PTY_CHILD";
const TEST: &str = "run::interactive_driver::modal_finish::finished_turn_inside_model_modal_accepts_the_next_prompt";
const DEADLINE: Duration = Duration::from_secs(10);

pub(crate) fn before_idle() -> bool {
    if std::env::var_os(CHILD).is_none() || !Path::new("697-arm").exists() {
        return false;
    }
    std::fs::remove_file("697-arm").unwrap();
    std::fs::write("697-before-idle", "ready").unwrap();
    wait_file(Path::new("697-release"));
    true
}

pub(crate) fn after_idle(gated: bool) {
    if gated {
        std::fs::write("697-after-idle", "idle published").unwrap();
    }
}

fn wait_file(path: &Path) {
    let deadline = Instant::now() + DEADLINE;
    while !path.exists() {
        assert!(Instant::now() < deadline, "driver gate: {}", path.display());
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_screen(
    received: &mpsc::Receiver<Vec<u8>>,
    screen: &mut vt100::Parser,
    label: &str,
    accepts: impl Fn(&str) -> bool,
) {
    let deadline = Instant::now() + DEADLINE;
    while !accepts(&screen.screen().contents()) {
        let chunk = received
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_else(|error| {
                panic!(
                    "PTY wait for {label}: {error}; {}",
                    screen.screen().contents()
                )
            });
        screen.process(&chunk);
    }
}

struct ReleaseOnDrop(Arc<ReleaseGate>);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}

struct ChildGuard(Box<dyn portable_pty::Child + Send + Sync>);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        // Reap on RED too: the owner may be waiting on the completed ingress.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// Catches: modal exit restores stale Busy, so Ctrl+C hangs on an idle ingress
// and the next prompt never reaches the provider.
#[tokio::test]
async fn finished_turn_inside_model_modal_accepts_the_next_prompt() {
    if std::env::var_os(CHILD).is_some() {
        let crate::cli::Invocation::Interactive(args) = crate::cli::parse(vec![
            "ego".into(),
            "--thinking".into(),
            "off".into(),
            "--model".into(),
            "ollama/qwen3".into(),
            "--sandbox".into(),
            "off".into(),
        ])
        .unwrap() else {
            panic!("interactive fixture");
        };
        let cancel = ego_core::turn::CancelToken::new();
        let out = crate::output::Stdout::new(cancel.clone());
        let ended = super::interactive_process(
            *args,
            &out,
            cancel,
            Arc::new(ego_core::evidence::EvidenceAdmissionService::new()),
        )
        .await;
        assert_eq!(ended.termination.exit_code(), 0);
        return;
    }

    let Reply::Paced { head, tail, .. } = paced("REGRESSION-MARKER-FIRST-DONE\n\n", Duration::ZERO) else {
        unreachable!("paced reply");
    };
    let provider_gate = ReleaseGate::new();
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::GatedPaced {
                head,
                tail,
                gate: Arc::clone(&provider_gate),
            },
            Reply::Sse(text("REGRESSION-MARKER-NEXT-ACCEPTED")),
        ],
        ..Plan::default()
    });
    let _release = ReleaseOnDrop(Arc::clone(&provider_gate));
    let ego = Ego::against(&stub)
        .conversing()
        .env("TERM", "xterm-256color");
    let instructions = ego.file("AGENTS.md", "# Modal finish fixture\n");
    let workspace = instructions.parent().unwrap();
    let mut command: CommandBuilder =
        ego.pty_command_with_program(std::env::current_exe().unwrap());
    command.get_argv_mut().truncate(1);
    command.args(["--exact", TEST, "--nocapture"]);
    command.env(CHILD, "1");
    command.env("RUST_MIN_STACK", "16777216");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut child = ChildGuard(pair.slave.spawn_command(command).unwrap());
    let writer = Arc::new(Mutex::new(pair.master.take_writer().unwrap()));
    let responder = Arc::clone(&writer);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let (sent, received) = mpsc::channel();
    let reader_thread = thread::spawn(move || {
        let mut screen = vt100::Parser::new(40, 120, 0);
        let mut pending = Vec::new();
        let mut chunk = [0; 1024];
        while let Ok(length) = reader.read(&mut chunk) {
            if length == 0 {
                break;
            }
            screen.process(&chunk[..length]);
            pending.extend_from_slice(&chunk[..length]);
            while let Some(at) = pending.windows(4).position(|bytes| bytes == b"\x1b[6n") {
                pending.drain(..at + 4);
                let (row, col) = screen.screen().cursor_position();
                let mut writer = responder.lock().unwrap();
                if write!(writer, "\x1b[{};{}R", row + 1, col + 1)
                    .and_then(|()| writer.flush())
                    .is_err()
                {
                    return;
                }
            }
            if pending.len() > 3 {
                pending.drain(..pending.len() - 3);
            }
            if sent.send(chunk[..length].to_vec()).is_err() {
                break;
            }
        }
    });
    let press = |keys: &[u8]| {
        let mut writer = writer.lock().unwrap();
        writer.write_all(keys).unwrap();
        writer.flush().unwrap();
    };
    let mut screen = vt100::Parser::new(40, 120, 0);
    let wait = |needle: &str, screen: &mut vt100::Parser| {
        wait_screen(&received, screen, needle, |visible| {
            visible.contains(needle)
        });
    };
    wait("Trust project files", &mut screen);
    press(b"yes\r");
    wait("▱▱▱", &mut screen);
    press(b"first turn\r");
    // Provider ingress proves the driver has consumed the initial Seat::read,
    // so the gate cannot accidentally hold startup idle instead of turn end.
    let deadline = Instant::now() + DEADLINE;
    while stub.generations().is_empty() {
        assert!(Instant::now() < deadline, "first provider request");
        thread::sleep(Duration::from_millis(10));
    }
    ego.file("697-arm", "arm\n");
    provider_gate.release();
    // The answer and output barrier finish before the idle call is gated.
    wait_file(&workspace.join("697-before-idle"));
    press(b"retained-697-draft");
    wait("> retained-697-draft", &mut screen);
    press(b"\x1bp");
    wait("Select model", &mut screen);
    ego.file("697-release", "release\n");
    wait_file(&workspace.join("697-after-idle"));
    press(b"\x03");
    wait_screen(
        &received,
        &mut screen,
        "restored composer after modal",
        |visible| {
            visible
                .lines()
                .last()
                .is_some_and(|row| row.trim_end() == "> retained-697-draft")
        },
    );
    // Do not inspect a phase label: require a clear, editable composer and a
    // second real provider request. Pre-fix Ctrl+C blocks before either effect.
    press(b"\x03");
    wait_screen(
        &received,
        &mut screen,
        "empty composer after Ctrl+C",
        |visible| {
            visible
                .lines()
                .last()
                .is_some_and(|row| row.trim_end() == ">")
        },
    );
    press(b"fresh-697-prompt\r");
    wait("REGRESSION-MARKER-NEXT-ACCEPTED", &mut screen);
    press(b"/quit\r");
    let deadline = Instant::now() + DEADLINE;
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "PTY child did not exit");
        thread::sleep(Duration::from_millis(10));
    };
    drop(writer);
    drop(pair);
    reader_thread.join().unwrap();
    assert!(status.success(), "interactive child failed: {status:?}");
    let generations = stub.generations();
    assert_eq!(
        generations.len(),
        2,
        "one request for each submitted prompt"
    );
    let messages = generations[1]["messages"].as_array().unwrap();
    assert!(
        messages
            .iter()
            .any(|message| message["role"] == "user" && message["content"] == "fresh-697-prompt"),
        "the cleared draft must not contaminate the next provider prompt: {messages:?}"
    );
}
