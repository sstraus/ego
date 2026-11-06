//! Setup must save only an explicitly reviewed human choice.
use crate::support;

use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use std::io::{Read, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn wait_for(
    receive: &mpsc::Receiver<Vec<u8>>,
    output: &mut Vec<u8>,
    seen: &mut usize,
    deadline: Instant,
    needle: &str,
) -> Result<(), mpsc::RecvTimeoutError> {
    assert!(!needle.is_empty(), "terminal marker must not be empty");
    loop {
        if let Some(offset) = output[*seen..]
            .windows(needle.len())
            .position(|bytes| bytes == needle.as_bytes())
        {
            *seen += offset + needle.len();
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        output.extend(receive.recv_timeout(remaining)?);
    }
}

// Catches: one read consumes a later marker before its ordered wait.
#[test]
fn wait_for_preserves_coalesced_markers() {
    let (send, receive) = mpsc::channel();
    send.send(b"Not saved.Settings (".to_vec()).unwrap();
    drop(send);
    let mut output = Vec::new();
    let mut seen = 0;
    let deadline = Instant::now() + Duration::from_secs(1);
    wait_for(&receive, &mut output, &mut seen, deadline, "Not saved.").unwrap();
    wait_for(&receive, &mut output, &mut seen, deadline, "Settings (").unwrap();
}

// Catches: split markers or lossy UTF-8 offsets skip or corrupt later matches.
#[test]
fn wait_for_matches_split_markers_with_byte_offsets() {
    let (send, receive) = mpsc::channel();
    send.send(b"\xffReady \xc3".to_vec()).unwrap();
    send.send(b"\xa9Next".to_vec()).unwrap();
    drop(send);
    let mut output = Vec::new();
    let mut seen = 0;
    let deadline = Instant::now() + Duration::from_secs(1);
    wait_for(&receive, &mut output, &mut seen, deadline, "Ready é").unwrap();
    wait_for(&receive, &mut output, &mut seen, deadline, "Next").unwrap();
}

// Catches: choosing the last match loses repeats, or reusing a consumed match passes falsely.
#[test]
fn wait_for_consumes_each_repeated_marker_once() {
    let (send, receive) = mpsc::channel();
    send.send(b"Settings (Settings (".to_vec()).unwrap();
    drop(send);
    let mut output = Vec::new();
    let mut seen = 0;
    let deadline = Instant::now() + Duration::from_secs(1);
    for _ in 0..2 {
        wait_for(&receive, &mut output, &mut seen, deadline, "Settings (").unwrap();
    }
    assert_eq!(
        wait_for(&receive, &mut output, &mut seen, deadline, "Settings ("),
        Err(mpsc::RecvTimeoutError::Disconnected)
    );
}

fn drive(approve: bool) -> String {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&home).unwrap();
    std::fs::create_dir(&workspace).unwrap();
    let file = home.join("config.toml");
    let original = "# preserve this comment\nmode='plan'\n";
    std::fs::write(&file, original).unwrap();
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_ego"));
    command.arg("setup");
    command.cwd(&workspace);
    command.env_clear();
    for (key, value) in support::coverage_environment() {
        command.env(key, value);
    }
    command.env("EGO_HOME", &home);
    command.env("HOME", &home);
    command.env("PATH", support::fixture_path());
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let mut input = pair.master.take_writer().unwrap();
    let mut reader = pair.master.try_clone_reader().unwrap();
    let (send, receive) = mpsc::channel();
    let reader_thread = std::thread::spawn(move || {
        let mut bytes = [0; 4096];
        while let Ok(count) = reader.read(&mut bytes) {
            if count == 0 || send.send(bytes[..count].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut output = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(12);
    let mut seen = 0;
    let mut wait_for = |needle: &str| {
        if let Err(error) = wait_for(&receive, &mut output, &mut seen, deadline, needle) {
            child.kill().unwrap();
            panic!(
                "missing {needle}: {error}: {}",
                String::from_utf8_lossy(&output)
            );
        }
    };
    wait_for("Choose where to save");
    input.write_all(b"\r").unwrap();
    wait_for("Settings (");
    // The base vocabulary orders model, thinking, then mode.
    input.write_all(b"\x1b[B\x1b[B\r").unwrap();
    wait_for("Stored: \"plan\"");
    input.write_all(b"\r").unwrap();
    wait_for("Choose mode");
    input.write_all(b"\x1b[B\r").unwrap();
    wait_for("mode = \"default\"");
    // End reaches Exit; one Up selects the preceding review action.
    input.write_all(b"\x1b[F\x1b[A\r").unwrap();
    wait_for("No changes have been saved.");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), original);
    input
        .write_all(if approve { b"\x1b[B\r" } else { b"\r" })
        .unwrap();
    if !approve {
        wait_for("Not saved.");
        wait_for("Settings (");
        input.write_all(b"\x1b[F\r").unwrap();
        wait_for("Discard unsaved changes?");
        input.write_all(b"\x1b[B\r").unwrap();
    }
    drop(input);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            panic!("setup did not exit: {}", String::from_utf8_lossy(&output));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    drop(pair.master);
    reader_thread.join().unwrap();
    for bytes in receive.try_iter() {
        output.extend(bytes);
    }
    assert!(status.success(), "{}", String::from_utf8_lossy(&output));
    let after = std::fs::read_to_string(&file).unwrap();
    if approve {
        let parsed =
            ego_core::config::ConfigLayer::parse(ego_core::config::Source::User, &after).unwrap();
        assert_eq!(
            parsed.value.mode.map(|mode| mode.as_str()),
            Some("default"),
            "{after}"
        );
        assert!(after.contains("# preserve this comment"));
    } else {
        assert_eq!(after, original);
    }
    String::from_utf8_lossy(&output).into_owned()
}

// Catches: setup writes a draft before the person reviews and confirms it.
#[test]
fn setup_terminal_saves_only_a_confirmed_candidate() {
    let output = drive(true);
    assert!(output.contains("Saved:"), "{output}");
}

// Catches: declining a preview still publishes the staged setting.
#[test]
fn setup_terminal_decline_preserves_the_original_document() {
    drive(false);
}
