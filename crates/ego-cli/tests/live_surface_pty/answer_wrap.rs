use super::*;

// Catches: hard-wrapped answer continuations lose the TUIC answer extent.
#[test]
fn marked_answers_word_wrap_with_hanging_indent_at_live_width() {
    let errors: Vec<_> = [80, 200]
        .into_iter()
        .flat_map(|width| check_answer_wrap(width, width))
        .collect();
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

// Catches: a long-lived sink renders at startup width after the PTY shrinks.
#[test]
fn marked_answers_use_resized_pty_width_without_splitting_words() {
    let errors = check_answer_wrap(200, 120);
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

fn check_answer_wrap(initial: u16, width: u16) -> Vec<String> {
    let mut errors = Vec::new();
    for (language, prose) in [
        (
            "ascii",
            "The original executable was removed but the symbolic link still points at the previous location and the remaining checks were skipped. ",
        ),
        (
            "italian",
            "L’eseguibile è stato rimosso perché il collegamento è rimasto sul percorso precedente e gli altri controlli sono stati saltati. ",
        ),
    ] {
        let source = format!("💬 {}REGRESSION-MARKER-END", prose.repeat(5));
        let stub = Stub::start(Plan {
            completions: vec![Reply::Sse(text(&source))],
            ..Plan::default()
        });
        let ego = Ego::against(&stub)
            .conversing()
            .env("TERM", "xterm-256color")
            .args([
                "--model",
                "ollama/qwen3",
                "--sandbox",
                "off",
                "--mode",
                "yolo",
            ]);
        ego.file("AGENTS.md", "# answer wrap fixture\n");
        let pair = NativePtySystem::default()
            .openpty(PtySize {
                rows: 40,
                cols: initial,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let child = pair.slave.spawn_command(ego.pty_command()).unwrap();
        let (mut input, responder, writer_join) = writer_owner(pair.master.take_writer().unwrap());
        let size = Arc::new(Mutex::new((40, initial)));
        let (reader, reader_join) = stream_with_resizable_screen(
            pair.master.try_clone_reader().unwrap(),
            responder,
            Some(Arc::clone(&size)),
        );
        let session = PtySession {
            child,
            pair,
            reader_join,
            writer_join,
        };
        let mut output = Vec::new();
        until(&reader, &mut output, b"Trust project files");
        let start = output.len();
        input.write_all(b"yes\r").unwrap();
        input.flush().unwrap();
        until_prompt_from(&reader, &mut output, start);
        drain_until_quiet(&reader, &mut output);
        let resize_at = output.len();
        *size.lock().unwrap() = (40, width);
        session
            .pair
            .master
            .resize(PtySize {
                rows: 40,
                cols: width,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let start = output.len();
        input.write_all(b"wrap fixture\r").unwrap();
        input.flush().unwrap();
        until_normalized(&reader, &mut output, start, "REGRESSION-MARKER-END");
        until_prompt_from(&reader, &mut output, start);
        drain_until_quiet(&reader, &mut output);
        let mut terminal = vt100::Parser::new(40, initial, 0);
        terminal.process(&output[..resize_at]);
        terminal.screen_mut().set_size(40, width);
        terminal.process(&output[resize_at..]);
        input.write_all(b"/quit\r").unwrap();
        input.flush().unwrap();
        assert!(session.quit(input).success());

        // Optional raw recordings let the real TUIC canvas replay the same PTY
        // bytes; the automated oracle below remains the physical screen.
        if let Some(directory) = std::env::var_os("EGO_REGRESSION_CAPTURE_DIR") {
            let directory = std::path::PathBuf::from(directory);
            std::fs::create_dir_all(&directory).unwrap();
            let stem = format!("{language}-{initial}-to-{width}");
            std::fs::write(directory.join(format!("{stem}.ansi")), &output).unwrap();
            std::fs::write(
                directory.join(format!("{stem}.json")),
                serde_json::to_vec_pretty(&serde_json::json!({
                    "rows": 40, "initial_cols": initial, "cols": width,
                    "resize_at_byte": resize_at, "source": source,
                }))
                .unwrap(),
            )
            .unwrap();
        }
        let grid = terminal.screen();
        let rows: Vec<String> = (0..40)
            .map(|row| {
                (0..width)
                    .filter_map(|col| grid.cell(row, col))
                    .filter(|cell| !cell.is_wide_continuation())
                    .map(|cell| {
                        if cell.contents().is_empty() {
                            " "
                        } else {
                            cell.contents()
                        }
                    })
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect();
        let first = rows
            .iter()
            .position(|row| row.starts_with("💬 "))
            .expect("visible answer marker");
        let last = rows
            .iter()
            .position(|row| row.contains("REGRESSION-MARKER-END"))
            .expect("visible answer end");
        let shown = rows[first..=last].join("\n");
        let context = format!("{language} initial={initial} live={width}: {shown:?}");
        if shown.split_whitespace().collect::<Vec<_>>()
            != source.split_whitespace().collect::<Vec<_>>()
        {
            errors.push(format!("terminal split a word or lost text: {context}"));
        }
        if last <= first {
            errors.push(format!("answer did not wrap: {context}"));
        }
        for (index, row) in rows.iter().enumerate().take(last + 1).skip(first) {
            if grid.row_wrapped(index as u16) {
                errors.push(format!(
                    "terminal auto-wrapped an emitted answer row: {context}"
                ));
            }
            if index > first && !row.starts_with("  ") {
                errors.push(format!("TUIC continuation needs hanging indent: {context}"));
            }
        }
    }
    errors
}
