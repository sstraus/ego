//! Real driver/seat in an isolated PTY with an image-kind pasteboard port.
//! The port never fetches bytes: these models must refuse before acquisition.
use crate::run::test_support as support;

use portable_pty::{NativePtySystem, PtySize, PtySystem};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use support::{Ego, Plan, Reply, Stub, text};

const PREFIX: &str = "run::interactive_driver::image_symptoms::";
const REFUSAL: &str = "image-input capability is unknown; no attachment was fetched";

// Catches: each repaint republishes the same Ctrl+V refusal.
#[tokio::test]
async fn ctrl_v_refusal_prints_once_across_redraws() {
    scenario("ctrl_v_refusal_prints_once_across_redraws", 0).await;
}

// Catches: the refusal is joined to the composer with ` | `.
#[tokio::test]
async fn ctrl_v_refusal_owns_a_transcript_row() {
    scenario("ctrl_v_refusal_owns_a_transcript_row", 1).await;
}

// Catches: a suspended host operation restores cooked VLNEXT and echoes ^M.
#[tokio::test]
async fn ctrl_v_then_enter_never_enters_literal_next() {
    scenario("ctrl_v_then_enter_never_enters_literal_next", 2).await;
}

async fn scenario(name: &str, assertion: u8) {
    if std::env::var_os("EGO_IMAGE_TEST_PTY_CHILD").is_some() {
        let arguments: Vec<String> =
            serde_json::from_str(&std::env::var("EGO_IMAGE_TEST_ARGS").unwrap()).unwrap();
        let crate::cli::Invocation::Interactive(args) = crate::cli::parse(arguments).unwrap()
        else {
            panic!("interactive fixture")
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
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("ENTER-SUBMITTED"))],
        ..Plan::default()
    });
    let ego = Ego::against(&stub)
        .conversing()
        .env("TERM", "xterm-256color")
        .env("ANTHROPIC_API_KEY", "test-key");
    ego.home_file(
        "models.toml",
        &support::vision_model_facts("image_input = \"unknown\""),
    );
    let instructions = ego.file("AGENTS.md", "# image refusal fixture\n");
    let workspace = instructions.parent().unwrap();
    let mut command = ego.pty_command_with_program(std::env::current_exe().unwrap());
    command.get_argv_mut().truncate(1);
    command.args(["--exact", &format!("{PREFIX}{name}"), "--nocapture"]);
    command.env("EGO_IMAGE_TEST_PTY_CHILD", "1");
    command.env("RUST_MIN_STACK", "16777216");
    if assertion == 2 {
        command.env("EGO_IMAGE_TEST_HOLD", "1");
    }
    command.env(
        "EGO_IMAGE_TEST_ARGS",
        serde_json::to_string(&[
            "ego",
            "--model",
            "anthropic/fixture-vision",
            "--thinking",
            "off",
            "--sandbox",
            "off",
            "--trust-workspace",
        ])
        .unwrap(),
    );
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 32,
            cols: 200,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut child = pair.slave.spawn_command(command).unwrap();
    let writer = Arc::new(Mutex::new(pair.master.take_writer().unwrap()));
    let responder = Arc::clone(&writer);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let (sent, received) = mpsc::channel();
    let reader_thread = thread::spawn(move || {
        let mut screen = vt100::Parser::new(32, 200, 0);
        let mut pending = Vec::new();
        let mut chunk = [0; 1024];
        while let Ok(n) = reader.read(&mut chunk) {
            if n == 0 {
                break;
            }
            screen.process(&chunk[..n]);
            pending.extend_from_slice(&chunk[..n]);
            while let Some(at) = pending.windows(4).position(|b| b == b"\x1b[6n") {
                pending.drain(..at + 4);
                let (r, c) = screen.screen().cursor_position();
                let mut out = responder.lock().unwrap();
                if write!(out, "\x1b[{};{}R", r + 1, c + 1)
                    .and_then(|()| out.flush())
                    .is_err()
                {
                    return;
                }
            }
            if pending.len() > 3 {
                pending.drain(..pending.len() - 3);
            }
            if sent.send(chunk[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let press = |keys: &[u8]| {
        let mut w = writer.lock().unwrap();
        w.write_all(keys).unwrap();
        w.flush().unwrap();
    };
    let mut output = Vec::new();
    let collect = |output: &mut Vec<u8>, duration: Duration, needle: Option<&str>| {
        let deadline = Instant::now() + duration;
        while Instant::now() < deadline {
            if let Some(needle) = needle {
                let mut p = vt100::Parser::new(32, 200, 0);
                p.process(output);
                if p.screen().contents().contains(needle) {
                    break;
                }
            }
            if let Ok(chunk) = received.recv_timeout(Duration::from_millis(30)) {
                output.extend(chunk);
            }
        }
    };
    collect(&mut output, Duration::from_secs(10), Some(">"));
    press(b"draft");
    collect(&mut output, Duration::from_millis(200), None);
    let start = output.len();
    press(b"\x16");
    if assertion == 2 {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !workspace.join("paste-inspected").exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let inspected = workspace.join("paste-inspected").exists();
        assert!(inspected, "pasteboard inspection was not reached");
        // A second paste followed by Enter must remain keys, not tty VLNEXT.
        press(b"\x16\r");
        collect(&mut output, Duration::from_millis(200), None);
        ego.file("paste-release", "release");
    }
    collect(&mut output, Duration::from_secs(5), Some(REFUSAL));
    if assertion == 2 {
        collect(
            &mut output,
            Duration::from_secs(10),
            Some("ENTER-SUBMITTED"),
        );
    }
    if assertion != 2 {
        press(b"\x1b[D");
        collect(&mut output, Duration::from_millis(200), None);
        press(b"\x1b[C");
        collect(&mut output, Duration::from_millis(200), None);
    }
    collect(&mut output, Duration::from_millis(200), None);
    let observed = output[start..].to_vec();
    let mut grid = vt100::Parser::new(32, 200, 0);
    grid.process(&output);
    let visible = grid.screen().contents();
    // Clean up even on RED: assertion failures must not leave a child behind.
    child.kill().unwrap();
    let _ = child.wait();
    drop(writer);
    drop(pair);
    reader_thread.join().unwrap();
    let raw = String::from_utf8_lossy(&observed);
    match assertion {
        0 => assert_eq!(
            raw.matches(REFUSAL).count(),
            1,
            "refusal repeated across redraws; grid={visible:?}"
        ),
        1 => assert!(
            visible
                .lines()
                .any(|row| row.contains(REFUSAL) && !row.contains(" | ") && !row.contains('>')),
            "refusal shares composer row: {visible:?}"
        ),
        2 => {
            assert!(
                visible.contains("ENTER-SUBMITTED"),
                "queued Enter did not submit: {visible:?}"
            );
            let requests = stub.generations();
            assert_eq!(
                requests.len(),
                1,
                "queued keys must submit exactly one turn"
            );
            let messages = requests[0]["messages"].as_array().unwrap();
            let user = messages
                .iter()
                .find(|message| message["role"] == "user")
                .unwrap();
            let content = user["content"]
                .as_array()
                .expect("Anthropic content blocks");
            assert_eq!(content.len(), 1, "queued keys must not add content blocks");
            assert_eq!(content[0]["type"], "text");
            assert_eq!(
                content[0]["text"], "draft",
                "literal CR or Ctrl+V polluted submission"
            );
        }
        _ => unreachable!(),
    }
}
