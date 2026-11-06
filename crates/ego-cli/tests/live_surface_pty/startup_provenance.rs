use super::*;

const COLS: u16 = 160;
const ROWS: u16 = 40;

// Catches: a configured yolo/off posture looks like an ordinary safe launch
// (the startup note names only the version and the model, never the mode,
// the sandbox, or what chose them).
#[test]
fn startup_displays_effective_mode_and_sandbox_sources() {
    let platform = if cfg!(target_os = "macos") {
        "adaptive"
    } else {
        "workspace"
    };
    let profile = "user profile strict selected by env";
    let cases = [
        (
            "flags",
            "",
            None,
            &["--mode", "yolo", "--sandbox", "off"][..],
            "mode yolo (flags) · sandbox off (flags)".to_owned(),
        ),
        (
            "user",
            "mode='plan'\nsandbox='off'\n",
            None,
            &[][..],
            "mode plan (user) · sandbox off (user)".to_owned(),
        ),
        (
            "env profile",
            "[profile.strict]\nmode='yolo'\nsandbox='off'\n",
            Some("strict"),
            &[][..],
            format!("mode yolo ({profile}) · sandbox off ({profile})"),
        ),
        (
            "implicit",
            "",
            None,
            &[][..],
            format!("mode auto (trusted interactive default) · sandbox {platform} (default)"),
        ),
    ];
    for (name, config, env_profile, flags, expected) in cases {
        let stub = Stub::start(Plan::default());
        let places = support::Places::new();
        // Adaptive requires its writable temporary root to be disjoint from home and workspace.
        let temporary = tempfile::tempdir().expect("temporary root");
        places.file("AGENTS.md", "# provenance fixture\n");
        let mut ego = places
            .ego(&stub)
            .conversing()
            .args(["--model", "ollama/qwen3"])
            .env("TMPDIR", temporary.path().to_str().expect("temporary path"));
        if !config.is_empty() {
            ego.home_file("config.toml", config);
        }
        if let Some(profile) = env_profile {
            ego = ego.env("EGO_PROFILE", profile);
        }
        for flag in flags {
            ego = ego.arg(flag);
        }
        let shown = normalized(&launch(ego));
        assert!(
            shown.contains(&expected),
            "{name}: startup does not show {expected:?}:\n{shown}"
        );
    }
}

/// Answer trust, wait for the idle composer, end with Ctrl+D and return the
/// raw terminal output.
fn launch(ego: Ego) -> Vec<u8> {
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: ROWS,
            cols: COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    drop(pair.slave);
    let (mut input, dsr, writer_join) = writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream_with_screen(
        pair.master.try_clone_reader().expect("reader"),
        dsr,
        Some((ROWS, COLS)),
    );
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust answer flush");
    until_prompt_from(&reader, &mut output, start);
    drain_until_quiet(&reader, &mut output);
    input.write_all(b"\x04").expect("interactive EOF");
    input.flush().expect("interactive EOF flush");
    let status = support::wait_pty_child(child, PROGRESS_DEADLINE, &output)
        .unwrap_or_else(|diagnostic| panic!("{diagnostic}"));
    drain_until_quiet(&reader, &mut output);
    drop(input);
    drop(pair.master);
    reader_join.join().expect("PTY reader");
    writer_join.join().expect("PTY writer");
    while let Ok(chunk) = reader.try_recv() {
        output.extend(chunk);
    }
    assert!(status.success(), "{}", normalized(&output));
    output
}
