use super::*;

// Catches: accepting a prompt reuses its receipt row for the next draft, or
// forgets the final CRLF scroll and paints the live prompt over the receipt.
#[test]
fn owned_footprint_tracks_rows_without_querying_the_terminal() {
    for (rows, fill_screen) in [(40, false), (12, true)] {
        let (reply, gate) = gated_paced_line("FOOTPRINT-RUNNING");
        let stub = Stub::start(Plan {
            completions: vec![reply],
            ..Plan::default()
        });
        let ego = Ego::against(&stub).conversing().args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--mode",
            "yolo",
            "--trust-workspace",
        ]);
        ego.file("AGENTS.md", "# owned footprint screen fixture\n");
        let replies = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let (session, mut input, reader) = PtySession::start_command_with_cursor_replies(
            ego.pty_command(),
            rows,
            140,
            Some((rows, 140)),
            Some(Arc::clone(&replies)),
        );
        let mut output = Vec::new();
        until_post_trust_prompt(&reader, &mut output, 0);
        if fill_screen {
            let start = output.len();
            input.write_all(b"/help\r").unwrap();
            input.flush().unwrap();
            // The command-return frame can precede the output pump's final
            // help batch. That late batch would legitimately scroll a new
            // receipt off screen, so wait for the last shortcut and its prompt.
            until_normalized(
                &reader,
                &mut output,
                start,
                "Option+R  Retry a waiting provider now",
            );
            let footer = start + find(&output[start..], b"Option+R  Retry a waiting provider now");
            until_prompt_from(&reader, &mut output, footer);
            drain_until_quiet(&reader, &mut output);
        }
        replies.store(false, std::sync::atomic::Ordering::SeqCst);
        let submitted = output.len();
        input.write_all(b"RECEIPT-KEEP\r").unwrap();
        input.flush().unwrap();
        until_normalized(&reader, &mut output, submitted, "FOOTPRINT-RUNNING");
        input.write_all(b"DRAFT-NEXT").unwrap();
        input.flush().unwrap();
        until_normalized(&reader, &mut output, submitted, "DRAFT-NEXT");
        drain_until_quiet(&reader, &mut output);
        let screen = screen_rows(&output, rows, 140);
        let receipt: Vec<_> = screen
            .iter()
            .enumerate()
            .filter(|(_, row)| row.contains("RECEIPT-KEEP"))
            .map(|(row, _)| row)
            .collect();
        let draft: Vec<_> = screen
            .iter()
            .enumerate()
            .filter(|(_, row)| row.contains("DRAFT-NEXT"))
            .map(|(row, _)| row)
            .collect();
        assert_eq!(
            receipt.len(),
            1,
            "receipt remains once: {screen:?}; raw={:?}",
            String::from_utf8_lossy(&output[submitted..])
        );
        assert_eq!(draft.len(), 1, "next prompt remains once: {screen:?}");
        assert!(
            receipt[0] < draft[0],
            "next prompt is below receipt: {screen:?}"
        );
        gate.release();
        until_mcp_composer_is_idle(&reader, &mut output);
        input.write_all(b"\x15/quit\r").unwrap();
        input.flush().unwrap();
        assert!(session.quit(input).success(), "{}", normalized(&output));
    }
}
