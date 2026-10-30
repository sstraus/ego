use super::*;
use std::path::PathBuf;

// Catches: logo bytes/colours drift, duplicate, wrap on narrow screens, or precede identity incorrectly.
#[test]
fn startup_name_precedes_notices() {
    let mut failures = Vec::new();
    for (rows, cols, colour) in [
        (24, 80, "colour"),
        (40, 120, "colour"),
        (24, 80, "no_color"),
        (24, 80, "dumb"),
        (24, 17, "colour"),
        (24, 16, "colour"),
    ] {
        for (name, flags) in [
            ("bare", vec!["--bare"]),
            ("declined", vec![]),
            ("trusted", vec!["--trust-workspace"]),
        ] {
            let stub = Stub::start(Plan::default());
            let places = support::Places::new();
            places.file("AGENTS.md", "# startup ordering fixture\n");
            places.file(".ego/config.toml", "model='ollama/qwen3'\n");
            let mut ego = places
                .ego(&stub)
                .conversing()
                .env(
                    "TERM",
                    if colour == "dumb" {
                        "dumb"
                    } else {
                        "xterm-256color"
                    },
                )
                .args([
                    "--model",
                    "ollama/qwen3",
                    "--mode",
                    "yolo",
                    "--sandbox",
                    "off",
                ]);
            if colour == "no_color" {
                ego = ego.env("NO_COLOR", "1");
            }
            for flag in flags {
                ego = ego.arg(flag);
            }
            let (session, mut input, reader) =
                PtySession::start(&ego, rows, cols, Some((rows, cols)));
            let mut output = Vec::new();
            if name == "declined" {
                until(&reader, &mut output, b"[y/N]");
                input.write_all(b"no\r").expect("decline trust");
                input.flush().expect("trust flush");
            }
            until_prompt_from(&reader, &mut output, 0);
            drain_until_quiet(&reader, &mut output);
            let shown = normalized(&output);
            let mut terminal = vt100::Parser::new(rows, cols, 100);
            terminal.process(&output);
            let capture = terminal.screen().contents();
            input.write_all(b"\x04").expect("EOF");
            input.flush().expect("EOF flush");
            assert!(session.quit(input).success());
            if let Some(directory) = std::env::var_os("EGO_STARTUP_CAPTURES") {
                let directory = PathBuf::from(directory);
                std::fs::create_dir_all(&directory).expect("capture directory");
                std::fs::write(
                    directory.join(format!("{name}-{cols}x{rows}-{colour}.ansi")),
                    &output,
                )
                .expect("raw capture");
                std::fs::write(
                    directory.join(format!("{name}-{cols}x{rows}-{colour}.txt")),
                    capture,
                )
                .expect("emulated capture");
            }
            let logo = [
                "⠀⢀⡴⠋⠉⠉⠀⣴⠋⠉⣷⠀⢠⡞⠉⢳⡄",
                "⠀⢴⡋⠉⠁⠀⠀⠻⣄⣀⣿⠀⠸⣇⠀⣸⠇",
                "⠀⠀⠉⠉⠉⠀⠀⠲⣄⣠⠟⠀⠀⠈⠉⠁⠀",
            ];
            let identity = concat!("εgo: ego ", env!("CARGO_PKG_VERSION"));
            let raw = String::from_utf8_lossy(&output);
            let identity_at = raw.find(identity).expect("identity line");
            let prefix = &raw[..identity_at];
            let mut header = vt100::Parser::new(rows, cols, 0);
            header.process(prefix.as_bytes());
            if cols >= 17 {
                let mut expected = String::new();
                for (row, text) in logo.iter().enumerate() {
                    if colour == "colour" {
                        let (epsilon, go) = text.split_at(6 * 3);
                        expected.push_str(&format!("\x1b[1;35m{epsilon}\x1b[36m{go}\x1b[0m\r\n"));
                    } else {
                        expected.push_str(&format!("{text}\r\n"));
                    }
                    for (column, glyph) in text.chars().enumerate() {
                        let cell = header
                            .screen()
                            .cell(row as u16, column as u16)
                            .expect("logo cell");
                        assert_eq!(cell.contents(), glyph.to_string(), "{name} {cols} {colour}");
                        if colour == "colour" {
                            assert_eq!(
                                cell.fgcolor(),
                                vt100::Color::Idx(if column < 6 { 5 } else { 6 })
                            );
                            assert!(cell.bold());
                        } else {
                            assert_eq!(cell.fgcolor(), vt100::Color::Default);
                            assert!(!cell.bold());
                        }
                    }
                }
                if prefix != expected {
                    failures.push(format!("{name} {cols}x{rows} {colour}: logo bytes before identity differ: {prefix:?}"));
                }
                assert_eq!(
                    raw.matches(logo[0]).count(),
                    usize::from(colour != "colour")
                );
                assert_eq!(
                    shown.matches(logo[0]).count(),
                    1,
                    "logo printed more than once"
                );
            } else {
                assert!(
                    prefix.is_empty(),
                    "narrow terminal printed logo: {prefix:?}"
                );
                assert!(!shown.contains(logo[0]));
            }
            if name != "trusted" {
                assert!(
                    support::notice_prose(&shown).contains("not applied in the bare posture"),
                    "bare notice retained: {shown}"
                );
            }
            assert!(
                support::notice_prose(&shown).contains("mode yolo (flags)"),
                "posture retained: {shown}"
            );
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// Catches: decorative startup bytes leak into headless output or stdio protocols.
#[test]
fn startup_logo_stays_out_of_non_tty_and_protocol_output() {
    for mode in ["conversation", "run", "acp", "mcp-server"] {
        let stub = Stub::start(Plan {
            completions: vec![Reply::Sse(text("HEADLESS-LOGO-ANSWER"))],
            ..Plan::default()
        });
        let places = support::Places::new();
        let ego = places.ego(&stub).env("TERM", "xterm-256color");
        let ego = if mode == "conversation" {
            ego.conversing()
                .args(["--model", "ollama/qwen3", "--sandbox", "off"])
        } else if mode == "run" {
            ego.args(["--model", "ollama/qwen3", "--sandbox", "off", "hello"])
        } else {
            ego.without_fixture_thinking().subcommand(mode)
        };
        let result = ego.run();
        assert!(!result.stdout.contains("⢀⡴"), "{mode}: {}", result.stdout);
        assert!(!result.stderr.contains("⢀⡴"), "{mode}: {}", result.stderr);
    }
}

// Catches: interactive stdin alone decorates redirected stdout with the logo.
#[cfg(unix)]
#[test]
fn startup_logo_skips_piped_stdout_with_tty_stdin() {
    let stub = Stub::start(Plan::default());
    let places = support::Places::new();
    let ego = places
        .ego(&stub)
        .conversing()
        .env("TERM", "xterm-256color")
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--trust-workspace",
        ]);
    let ego_command = ego.pty_command();
    let mut command = portable_pty::CommandBuilder::new("/bin/sh");
    command.env_clear();
    for (key, value) in ego_command.iter_full_env_as_str() {
        command.env(key, value);
    }
    if let Some(cwd) = ego_command.get_cwd() {
        command.cwd(cwd);
    }
    command.args(["-c", "exec \"$@\" > piped-output.txt", "startup-pipe"]);
    command.args(ego_command.get_argv());
    let (session, mut input, reader) = PtySession::start_command(command, 24, 80, None);
    let mut output = Vec::new();
    until(
        &reader,
        &mut output,
        concat!("ego ", env!("CARGO_PKG_VERSION")).as_bytes(),
    );
    input.write_all(b"\x04").expect("EOF");
    input.flush().expect("EOF flush");
    session.quit(input);
    let piped = std::fs::read_to_string(places.workspace_path().join("piped-output.txt"))
        .expect("piped stdout");
    assert!(!piped.contains("⢀⡴"), "{piped}");
    assert!(!normalized(&output).contains("⢀⡴"));
}

// Catches: replacing the startup quiet/TTY conjunction with OR prints identity and logo.
#[test]
fn quiet_tty_start_prints_no_identity_or_logo() {
    let ego = startup_gate_refusal().arg("--quiet");
    let result = ego.run_pty_to_exit();
    assert_eq!(result.code, Some(2), "{}", result.stdout);
    assert!(result.stdout.contains("config.toml"), "{}", result.stdout);
    assert!(!result.stdout.contains(concat!("ego ", env!("CARGO_PKG_VERSION"))), "{}", result.stdout);
    assert!(!normalized(result.stdout.as_bytes()).contains("⢀⡴"), "{}", result.stdout);
}

// Catches: the same OR mutation prints startup identity with piped stdin.
#[test]
fn piped_stdin_start_prints_no_identity_or_logo() {
    let result = startup_gate_refusal().stdin("").run();
    assert_eq!(result.code, Some(2), "{} {}", result.stdout, result.stderr);
    let shown = format!("{}{}", result.stdout, result.stderr);
    assert!(shown.contains("config.toml"), "{shown}");
    assert!(!shown.contains(concat!("ego ", env!("CARGO_PKG_VERSION"))), "{shown}");
    assert!(!shown.contains("⢀⡴"), "{shown}");
}

// Catches: deleting interactive_process's flag_plain initializer styles the logo.
#[test]
fn plain_flag_start_has_no_sgr() {
    let result = startup_gate_refusal().arg("--plain").run_pty_to_exit();
    assert_eq!(result.code, Some(2), "{}", result.stdout);
    assert!(result.stdout.contains("config.toml"), "{}", result.stdout);
    let (startup, _) = result.stdout.split_once(concat!("ego ", env!("CARGO_PKG_VERSION")))
        .expect("startup identity proves the interactive driver ran");
    assert!(startup.contains("⠀⢀⡴⠋⠉⠉⠀⣴⠋⠉⣷⠀⢠⡞⠉⢳⡄"), "{startup:?}");
    assert!(!startup.contains("\x1b["), "plain startup contains terminal styling: {startup:?}");
}

// Refuse immediately after the real startup block, before terminal-editor or
// provider output can hide which stage emitted an escape sequence. The malformed
// user file is read by keyboard admission, not a mocked driver state.
fn startup_gate_refusal() -> Ego {
    let ego = Ego::nowhere()
        .conversing()
        .env("TERM", "xterm-256color");
    ego.home_file("config.toml", "model = [\n");
    ego
}
