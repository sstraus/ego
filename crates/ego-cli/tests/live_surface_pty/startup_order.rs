use super::*;
use std::path::PathBuf;

// Catches: startup diagnostics reach the terminal before the application name.
#[test]
fn startup_name_precedes_notices() {
    let mut failures = Vec::new();
    for (rows, cols) in [(24, 80), (40, 120)] {
        for (name, flags) in [
            ("bare", vec!["--bare"]),
            ("declined", vec![]),
            ("trusted", vec!["--trust-workspace"]),
        ] {
            let stub = Stub::start(Plan::default());
            let places = support::Places::new();
            places.file("AGENTS.md", "# startup ordering fixture\n");
            places.file(".ego/config.toml", "model='ollama/qwen3'\n");
            let mut ego = places.ego(&stub).conversing().args([
                "--model",
                "ollama/qwen3",
                "--mode",
                "yolo",
                "--sandbox",
                "off",
            ]);
            for flag in flags {
                ego = ego.arg(flag);
            }
            let (session, mut input, reader) =
                PtySession::start(&ego, rows, cols, Some((rows, cols)));
            let mut output = Vec::new();
            if name == "declined" {
                until(&reader, &mut output, b"Trust project files");
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
                    directory.join(format!("{name}-{cols}x{rows}.ansi")),
                    &output,
                )
                .expect("raw capture");
                std::fs::write(directory.join(format!("{name}-{cols}x{rows}.txt")), capture)
                    .expect("emulated capture");
            }
            let first = shown
                .lines()
                .find(|line| !line.trim().is_empty())
                .expect("startup output");
            if !first.contains(concat!("ego ", env!("CARGO_PKG_VERSION"))) {
                failures.push(format!(
                    "{name} {cols}x{rows}: name must be first, got {first:?}:\n{shown}"
                ));
            }
            if name != "trusted" {
                assert!(
                    shown.contains("not applied in the bare posture"),
                    "bare notice retained: {shown}"
                );
            }
            assert!(
                shown.contains("mode yolo (flags)"),
                "posture retained: {shown}"
            );
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
