use super::*;

// Catches: a sent image is invisible after the draft chip disappears or after session replay.
#[test]
fn submitted_image_is_named_in_live_and_resumed_transcripts() {
    for count in [1, 2] {
        submitted_images(count);
    }
}

fn submitted_images(count: usize) {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("IMAGE-ANSWER"))],
        tags_refusal: Some((
            200,
            serde_json::json!({
                "models": [{ "name": "qwen3", "capabilities": ["completion", "tools", "vision"] }]
            })
            .to_string(),
        )),
        ..Plan::default()
    });
    let places = support::Places::new();
    let path = places.file("visible-mark.png", "");
    std::fs::write(&path, support::PNG_1X1).unwrap();
    let ego = places.ego(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--sandbox",
        "off",
        "--trust-workspace",
    ]);
    let (session, mut input, reader) = PtySession::start(&ego, 32, 160, Some((32, 160)));
    let mut output = Vec::new();
    until_prompt_from(&reader, &mut output, 0);
    input.write_all(b"describe the image").unwrap();
    input.flush().unwrap();
    for _ in 0..count {
        input
            .write_all(format!("\x1b[200~{}\x1b[201~", path.display()).as_bytes())
            .unwrap();
        input.flush().unwrap();
        until_normalized(&reader, &mut output, 0, "[image: visible-mark.png]");
    }
    let start = output.len();
    input.write_all(b"\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "IMAGE-ANSWER");
    until_prompt_from(&reader, &mut output, start);
    let live = normalized(&output[start..]);
    let marker = if count == 1 {
        "1 image attached"
    } else {
        "2 images attached"
    };
    assert!(live.contains(marker), "{live}");
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    let journal = std::fs::read_to_string(
        places
            .home_path()
            .join("sessions")
            .join(support::FIXTURE_SESSION_ID)
            .join("events.jsonl"),
    )
    .unwrap();
    let prompt = journal
        .lines()
        .filter_map(|row| serde_json::from_str::<serde_json::Value>(row).ok())
        .find(|row| row["payload"]["event"]["kind"]["kind"] == "UserPrompt")
        .unwrap();
    assert_eq!(
        prompt["payload"]["event"]["kind"]["images"]
            .as_array()
            .unwrap()
            .len(),
        count
    );
    assert_eq!(stub.generations().len(), 1);

    let resume = places
        .ego(&stub)
        .subcommand("resume")
        .arg(support::FIXTURE_SESSION_ID)
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--trust-workspace",
        ]);
    let (session, mut input, reader) = PtySession::start(&resume, 32, 160, Some((32, 160)));
    let mut output = Vec::new();
    until_prompt_from(&reader, &mut output, 0);
    drain_until_quiet(&reader, &mut output);
    let replayed = normalized(&output);
    assert!(replayed.contains("describe the image"), "{replayed}");
    assert!(replayed.contains(marker), "{replayed}");
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert_eq!(
        stub.generations().len(),
        1,
        "replay must not send a new turn"
    );
}
