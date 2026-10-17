//! A test-only child runs the real interactive driver and Seat::new. Unlike
//! the CLI binary, its terminal owner can hold its actual printer deterministically.

use crate::run::test_support as support;

use std::io::{Read, Write};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{NativePtySystem, PtySize, PtySystem};
use support::{Ego, Plan, Reply, Stub, text, tool_call};

const CHILD: &str = "EGO_REGRESSION_PTY_CHILD";
const TEST: &str = "run::interactive_driver::held_permission_receipt::permission_decision_reaches_run_with_a_held_printer_row";

// Catches: the real seat sends a permission receipt to a full bounded printer
// before delivering the decision, so the approved tool never starts.
#[tokio::test]
async fn permission_decision_reaches_run_with_a_held_printer_row() {
    if std::env::var_os(CHILD).is_some() {
        let arguments: Vec<String> =
            serde_json::from_str(&std::env::var("EGO_REGRESSION_ARGS").expect("isolated driver argv"))
                .unwrap();
        let crate::cli::Invocation::Interactive(args) = crate::cli::parse(arguments).unwrap()
        else {
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

    let call = r#"{"argv":["/bin/sh","-c","printf decision-received > decision-received"],"timeout_ms":5000}"#;
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call("regression_case", "exec", call)),
            Reply::Sse(text("regression_case-DONE")),
        ],
        ..Plan::default()
    });
    let ego = Ego::against(&stub)
        .conversing()
        .env("TERM", "xterm-256color");
    let instructions = ego.file("AGENTS.md", "# held printer fixture\n");
    let workspace = instructions.parent().unwrap();
    // Obtain the harness's isolated provider/home environment without running
    // its ordinary CLI binary. Only this child has the cfg(test) printer seam.
    let mut command = ego.pty_command_with_program(std::env::current_exe().unwrap());
    command.get_argv_mut().truncate(1);
    command.args(["--exact", TEST, "--nocapture"]);
    command.env(CHILD, "1");
    command.env("RUST_MIN_STACK", "16777216");
    command.env(
        "EGO_REGRESSION_ARGS",
        serde_json::to_string(&[
            "ego",
            "--thinking",
            "off",
            "--model",
            "ollama/qwen3",
            "--mode",
            "default",
            "--sandbox",
            "off",
        ])
        .unwrap(),
    );
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut child = pair.slave.spawn_command(command).unwrap();
    let writer = Arc::new(std::sync::Mutex::new(pair.master.take_writer().unwrap()));
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
    let mut output = Vec::new();
    let mut screen = vt100::Parser::new(40, 120, 0);
    let wait = |needle: &str, output: &mut Vec<u8>, screen: &mut vt100::Parser| {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !screen.screen().contents().contains(needle) {
            let chunk = received
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|error| {
                    panic!(
                        "PTY wait for {needle}: {error}; {}",
                        screen.screen().contents()
                    )
                });
            screen.process(&chunk);
            output.extend(chunk);
        }
    };
    let press = |keys: &[u8]| {
        let mut writer = writer.lock().unwrap();
        writer.write_all(keys).unwrap();
        writer.flush().unwrap();
    };
    wait("Trust project files", &mut output, &mut screen);
    press(b"yes\r");
    wait("ctx", &mut output, &mut screen);
    press(b"use the tool\r");
    wait("[y] yes", &mut output, &mut screen);
    press(b"y");
    let held = workspace.join("printer-held");
    let decision = workspace.join("decision-received");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !held.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(held.exists(), "the live bounded(1) printer was not filled");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !decision.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    let progressed_before_release = std::fs::read_to_string(&decision).ok();
    assert!(!screen.screen().contents().contains("regression_case-ROW"));
    // Release even on RED and reap this child's threads before the assertion.
    ego.file("printer-release", "release\n");
    if progressed_before_release.is_none() {
        child.kill().unwrap();
    } else {
        wait("regression_case-DONE", &mut output, &mut screen);
        let shown = screen.screen().contents();
        assert!(shown.contains("regression_case-ROW"), "held row missing: {shown}");
        assert!(
            shown
                .lines()
                .any(|row| row.starts_with("Allowed run /bin/sh")),
            "receipt missing: {shown}"
        );
        press(b"/quit\r");
    }
    let status = child.wait().unwrap();
    drop(writer);
    drop(pair);
    reader_thread.join().unwrap();
    assert_eq!(
        progressed_before_release.as_deref(),
        Some("decision-received"),
        "receipt blocked the real run decision behind the held bounded(1) printer row"
    );
    assert!(status.success(), "interactive PTY child failed: {status:?}");
}
