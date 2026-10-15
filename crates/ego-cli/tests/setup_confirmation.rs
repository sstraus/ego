//! Real PTY setup confirmation is distinct from provider and piped text.
mod support;

use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use std::io::{Read, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use support::{Plan, Reply, Stub, text};

fn confirmation_case(answer: &[u8], applied: bool) {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let workspace = root.path().join("workspace");
    let needed = root.path().join("needed");
    for path in [&home, &workspace, &needed] {
        std::fs::create_dir(path).unwrap();
    }
    let file = needed.join("task.md");
    std::fs::write(&file, "read-only task fixture").unwrap();
    let original = "model='ollama/qwen3'\nsandbox='off'\nroots=[]\n";
    std::fs::write(home.join("config.toml"), original).unwrap();
    let selection =
        serde_json::json!([{"kind":"read", "path":file, "reason":"user-selected task"}])
            .to_string();
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text(&selection))],
        ..Plan::default()
    });
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 32,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_ego"));
    command.args(["setup", "inspect this task"]);
    command.cwd(&workspace);
    command.env_clear();
    command.env("EGO_HOME", &home);
    command.env("HOME", &home);
    command.env("OLLAMA_HOST", &stub.address);
    command.env("PATH", support::fixture_path());
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let mut input = pair.master.take_writer().unwrap();
    let mut reader = pair.master.try_clone_reader().unwrap();
    let (send, receive) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        let mut bytes = [0; 4096];
        while let Ok(count) = reader.read(&mut bytes) {
            if count == 0 {
                break;
            }
            if send.send(bytes[..count].to_vec()).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut output = Vec::new();
    while !String::from_utf8_lossy(&output).contains("Apply? [y/N]") {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match receive.recv_timeout(remaining) {
            Ok(bytes) => output.extend(bytes),
            Err(error) => {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!(
                    "setup confirmation missing: {error}: {}",
                    String::from_utf8_lossy(&output)
                );
            }
        }
    }
    assert_eq!(
        std::fs::read_to_string(home.join("config.toml")).unwrap(),
        original
    );
    input.write_all(answer).unwrap();
    input.flush().unwrap();
    drop(input);
    while let Ok(bytes) = receive.recv_timeout(Duration::from_secs(5)) {
        output.extend(bytes);
    }
    let status = child.try_wait().unwrap();
    if status.is_none() {
        child.kill().unwrap();
    }
    let status = child.wait().unwrap();
    drop(pair.master);
    thread.join().unwrap();
    assert!(status.success(), "{}", String::from_utf8_lossy(&output));
    let current = std::fs::read_to_string(home.join("config.toml")).unwrap();
    if applied {
        assert!(current.contains(&needed.display().to_string()), "{current}");
        assert!(String::from_utf8_lossy(&output).contains("fixed (policy-only)"));
    } else {
        assert_eq!(current, original);
    }
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "read-only task fixture"
    );
    assert_eq!(std::fs::read_dir(&workspace).unwrap().count(), 0);
}

// Catches: model-selected probes publish before explicit terminal confirmation.
#[test]
fn setup_cli_real_terminal_confirms_exact_candidate_and_reprobes() {
    confirmation_case(b"yes\r", true);
}

// Catches: declining a setup prompt publishes the prepared candidate anyway.
#[test]
fn setup_cli_real_terminal_decline_preserves_authority() {
    confirmation_case(b"no\r", false);
}

// Catches: EOF/cancel at the human confirmation prompt publishes a candidate.
#[test]
fn setup_cli_real_terminal_eof_preserves_authority() {
    confirmation_case(b"\x04", false);
}
