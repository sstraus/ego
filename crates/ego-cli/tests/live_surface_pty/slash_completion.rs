use super::*;

// Catches: accepting an active slash menu consumes Enter without dispatching its command.
#[test]
fn selected_slash_completion_enter_runs_once() {
    for (rows, cols) in [(40, 120), (24, 80)] {
        let stub = Stub::start(Plan::default());
        let ego =
            Ego::against(&stub)
                .conversing()
                .args(["--model", "ollama/qwen3", "--sandbox", "off"]);
        ego.file("AGENTS.md", "# Slash completion fixture\n");
        let (session, mut input, reader) = PtySession::start(&ego, rows, cols, None);
        let mut output = Vec::new();
        until(&reader, &mut output, b"Trust project files");
        let start = output.len();
        input.write_all(b"yes\r").expect("trust");
        input.flush().expect("flush");
        until_prompt_from(&reader, &mut output, start);
        let start = output.len();
        input.write_all(b"/mc").expect("prefix");
        input.flush().expect("flush");
        until_normalized(&reader, &mut output, start, "/mcp");
        drain_until_quiet(&reader, &mut output);
        input.write_all(b"\r").expect("accept selection");
        input.flush().expect("flush");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            until_normalized_without_new_generation(&reader, &mut output, start, "mcp:", &stub, 0);
            until_prompt_from(&reader, &mut output, start);
            drain_until_quiet(&reader, &mut output);
            assert_eq!(normalized(&output[start..]).matches("mcp:").count(), 1);
        }));
        if let Some(directory) = std::env::var_os("EGO_SLASH_RECORD_DIR") {
            let directory = std::path::PathBuf::from(directory);
            std::fs::create_dir_all(&directory).expect("record directory");
            std::fs::write(directory.join(format!("{cols}x{rows}.ansi")), &output)
                .expect("raw capture");
            std::fs::write(
                directory.join(format!("{cols}x{rows}.json")),
                format!("{{\"cols\":{cols},\"rows\":{rows}}}\n"),
            )
            .expect("geometry");
        }
        input.write_all(b"\x15/quit\r").expect("exit");
        input.flush().expect("flush");
        session.quit(input);
        if let Err(error) = result {
            std::panic::resume_unwind(error);
        }
    }
}
