//! Manual live capture driver. Uses the same real PTY and CPR responder as the suite.
use super::*;

#[test]
#[ignore = "launches authenticated installed CLIs; run explicitly for terminal evidence"]
fn capture_installed_cli_bottom_rows() {
    let dir = std::path::PathBuf::from(
        std::env::var_os("EGO_BOTTOM_ROW_CAPTURE_DIR").expect("capture output directory"),
    );
    std::fs::create_dir_all(&dir).unwrap();
    for (name, program, args) in [
        ("claude", "claude", Vec::<&str>::new()),
        (
            "codex",
            "codex",
            vec!["--no-alt-screen", "--no-daemon", "--disable", "hooks"],
        ),
    ] {
        if std::env::var("EGO_CAPTURE_CLI").is_ok_and(|selected| selected != name) {
            continue;
        }
        for (rows, cols) in [(24, 80), (40, 120)] {
            // Start at the bottom, so a short startup frame cannot pass merely
            // because most of the terminal has never been used.
            let mut command = portable_pty::CommandBuilder::new("/bin/bash");
            command.args([
                "-c",
                "for ((i=0;i<50;i++)); do printf 'PTY prefill %s\\r\\n' \"$i\"; done; exec \"$@\"",
                "capture",
            ]);
            command.arg(program);
            command.args(args.iter());
            command.cwd(&dir);
            command.env("TERM", "xterm-256color");
            command.env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1");
            command.env_remove("TUIC_SESSION");
            command.env_remove("TUIC_PARENT");
            command.env_remove("CLAUDECODE");
            let (mut session, mut input, reader) =
                PtySession::start_command(command, rows, cols, Some((rows, cols)));
            let mut output = Vec::new();
            collect_for(&reader, &mut output, Duration::from_secs(5));
            for _ in 0..4 {
                let mut parser = vt100::Parser::new(rows, cols, 0);
                parser.process(&output);
                let shown = parser.screen().contents();
                if shown.contains("Allow external CLAUDE.md")
                    || shown.contains("trust this folder")
                    || shown.contains("trust the files")
                    || shown.contains("Yes, I trust")
                    || shown.contains("Yes, continue")
                {
                    // Decline external imports (the default); accept this
                    // task-created capture directory at the trust dialog.
                    input.write_all(b"\r").unwrap();
                    input.flush().unwrap();
                    collect_for(&reader, &mut output, Duration::from_secs(4));
                } else {
                    break;
                }
            }
            if name == "codex" {
                let deadline = Instant::now() + Duration::from_secs(40);
                while !output
                    .windows(b"]7501;state=idle".len())
                    .any(|bytes| bytes == b"]7501;state=idle")
                {
                    let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                        break;
                    };
                    match reader.recv_timeout(remaining) {
                        Ok(chunk) => output.extend(chunk),
                        Err(_) => break,
                    }
                }
                collect_for(&reader, &mut output, Duration::from_millis(300));
            }
            save_capture(&dir, name, rows, cols, "idle", &output);
            input.write_all(b"\x1b[200~Do not use any tools. Write a detailed 2000-word explanation of terminal rendering, starting with CAPTURE-MARK.\x1b[201~").unwrap();
            input.flush().unwrap();
            // Separate Enter from the paste burst: Codex's burst detector
            // otherwise treats the trailing CR as part of the pasted draft.
            collect_for(&reader, &mut output, Duration::from_millis(500));
            input.write_all(b"\r").unwrap();
            input.flush().unwrap();
            collect_for(&reader, &mut output, Duration::from_secs(4));
            save_capture(&dir, name, rows, cols, "working", &output);
            // Only this driver owns the child PID and terminates/reaps it here.
            session.child.kill().unwrap();
            let _ = session.quit(input);
        }
    }
}

fn collect_for(reader: &mpsc::Receiver<Vec<u8>>, output: &mut Vec<u8>, duration: Duration) {
    let deadline = Instant::now() + duration;
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match reader.recv_timeout(remaining) {
            Ok(chunk) => output.extend(chunk),
            Err(_) => break,
        }
    }
}

fn save_capture(dir: &Path, name: &str, rows: u16, cols: u16, phase: &str, output: &[u8]) {
    let mut parser = vt100::Parser::new(rows, cols, 0);
    parser.process(output);
    let screen = parser.screen();
    let mut grid = format!(
        "{name} {cols}x{rows} {phase}\ncursor (zero-based): {:?}\n",
        screen.cursor_position()
    );
    for (index, row) in screen.rows(0, cols).enumerate() {
        use std::fmt::Write as _;
        writeln!(grid, "{:02}: {row}", index + 1).unwrap();
    }
    let stem = format!("{name}-{cols}x{rows}-{phase}");
    std::fs::write(dir.join(format!("{stem}.raw")), output).unwrap();
    std::fs::write(dir.join(format!("{stem}.txt")), &grid).unwrap();
    // TUICCAP2 is a public capture fixture format: geometry then output record.
    let mut fixture = b"TUICCAP2\n".to_vec();
    fixture.extend(rows.to_le_bytes());
    fixture.extend(cols.to_le_bytes());
    fixture.push(0);
    fixture.extend(0_u64.to_le_bytes());
    fixture.extend(u32::try_from(output.len()).unwrap().to_le_bytes());
    fixture.extend(output);
    std::fs::write(dir.join(format!("{stem}.tcap")), fixture).unwrap();
    println!("{grid}");
}
