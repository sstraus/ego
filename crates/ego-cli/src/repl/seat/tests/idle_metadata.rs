//! Catches: OSC publication queues metadata without waking the idle terminal owner.

use super::*;
use portable_pty::{CommandBuilder, PtySize};
use std::io::{Read, Write};

const CHILD: &str = "EGO_1020_METADATA_CHILD";
const MARKER: &str = "\x1b]777;ego-test-1020-metadata\x07";

#[test]
fn osc_signal_wakes_the_live_seat_without_key_input() {
    if let Some(directory) = std::env::var_os(CHILD) {
        let directory = PathBuf::from(directory);
        let (seat, output) = Seat::new(CancelToken::new()).unwrap();
        let output = output.expect("a real PTY creates the live seat output");
        let selection = GenerationSelection::new(
            ego_core::config::ModelRef("test/model".into()),
            ego_core::generator::types::ThinkingLevel::Off,
        )
        .snapshot()
        .unwrap();
        seat.configure_status(StatusSetup {
            session: SessionId::new(),
            state: SessionState::default(),
            selection,
            mode: Mode::Yolo,
            cwd: directory.clone(),
            trust: Trust::Untrusted,
            template: "{{prompt}}".into(),
            width: 100,
            native: false,
            git: None,
        });
        seat.idle();
        seat.activate();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !directory.join("prompt-observed").exists() {
            assert!(
                Instant::now() < deadline,
                "the real seat did not paint its prompt"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        // Let startup repaint and cursor replies settle before the only producer.
        std::thread::sleep(Duration::from_millis(250));
        output.signal(MARKER).unwrap();
        while !directory.join("metadata-observed").exists() {
            assert!(
                Instant::now() < deadline,
                "OSC publication did not wake the idle seat"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        seat.shutdown();
        drop(output);
        seat.join();
        return;
    }

    let directory = tempfile::Builder::new()
        .prefix("ego-1020-osc-")
        .tempdir_in(std::env::current_dir().unwrap())
        .unwrap();
    let pair = portable_pty::native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = CommandBuilder::new(std::env::current_exe().unwrap());
    command.args([
        "--exact",
        "repl::seat::tests::idle_metadata::osc_signal_wakes_the_live_seat_without_key_input",
        "--nocapture",
    ]);
    command.env(CHILD, directory.path());
    command.env("TERM", "xterm-256color");
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let mut writer = pair.master.take_writer().unwrap();
    let mut reader = pair.master.try_clone_reader().unwrap();
    let output = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&output);
    let reader_worker = std::thread::spawn(move || {
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
                // Terminal query replies only: no key is ever sent to this child.
                writer.write_all(b"\x1b[1;1R").unwrap();
                writer.flush().unwrap();
            }
            if pending.len() > 4096 {
                pending.drain(..pending.len() - 3);
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut prompt_observed = false;
    let mut metadata_observed = false;
    let status = loop {
        let bytes = output.lock().unwrap().clone();
        if !prompt_observed && bytes.windows(2).any(|part| part == b"> ") {
            std::fs::write(directory.path().join("prompt-observed"), b"ready").unwrap();
            prompt_observed = true;
        }
        if !metadata_observed
            && bytes
                .windows(MARKER.len())
                .any(|part| part == MARKER.as_bytes())
        {
            std::fs::write(directory.path().join("metadata-observed"), b"received").unwrap();
            metadata_observed = true;
        }
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let _ = child.wait();
            panic!(
                "live seat did not complete: {}",
                String::from_utf8_lossy(&bytes)
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    reader_worker.join().unwrap();
    assert!(
        status.success() && prompt_observed && metadata_observed,
        "no-key OSC delivery failed: {}",
        String::from_utf8_lossy(&output.lock().unwrap())
    );
}
