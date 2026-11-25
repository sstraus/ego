use super::*;

fn title_events(journal: &std::path::Path) -> Vec<Option<String>> {
    std::fs::read_to_string(journal)
        .expect("read session journal")
        .lines()
        .filter_map(|line| {
            let record: serde_json::Value =
                serde_json::from_str(line).expect("journal record is JSON");
            // Non-event records carry no `payload.event`; skip only those.
            let event = record["payload"].get("event")?.clone();
            let event: ego_core::event::domain::DomainEvent =
                serde_json::from_value(event).expect("journal event decodes");
            match event.kind {
                ego_core::event::domain::EventKind::SessionTitleChanged { title } => Some(title),
                _ => None,
            }
        })
        .collect()
}

// Catches: `/name set` or `/name clear` is refused or queued behind a held
// model reply, publishes the wrong or a duplicate title event, or reaches the
// model as another generation.
#[test]
fn native_name_set_and_clear_publish_one_title_event_each_before_held_reply_release() {
    let gate = ReleaseGate::new();
    let Reply::Paced { head, .. } = paced_line("NAME-CONTROL-HELD\n", Duration::ZERO) else {
        unreachable!()
    };
    let stub = Stub::start(Plan {
        completions: vec![Reply::GatedPaced {
            head,
            tail: text("NAME-CONTROL-REPLY-FINISHED"),
            gate: Arc::clone(&gate),
        }],
        ..Plan::default()
    });
    struct ReleaseOnDrop(Arc<ReleaseGate>);
    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            self.0.release();
        }
    }
    let _release = ReleaseOnDrop(Arc::clone(&gate));
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "off",
        "--trust-workspace",
        "--session-id",
        support::FIXTURE_SESSION_ID,
    ]);
    let journal = ego
        .home_path()
        .join("sessions")
        .join(support::FIXTURE_SESSION_ID)
        .join("events.jsonl");
    let (session, mut input, reader) = PtySession::start(&ego, 40, 120, Some((40, 120)));
    let mut output = Vec::new();
    until_prompt_from(&reader, &mut output, 0);
    input.write_all(b"hold the reply while naming\r").unwrap();
    input.flush().unwrap();
    until(&reader, &mut output, b"NAME-CONTROL-HELD");
    let deadline = std::time::Instant::now() + PROGRESS_DEADLINE;
    while stub.generations().is_empty() {
        output.extend(
            reader
                .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                .expect("held generation started"),
        );
    }
    assert!(title_events(&journal).is_empty());
    for (command, expected, published) in [
        (
            "/name set NAME-CONTROL-TITLE\r",
            "name: NAME-CONTROL-TITLE",
            vec![Some("NAME-CONTROL-TITLE".to_owned())],
        ),
        (
            "/name clear\r",
            "name: (none)",
            vec![Some("NAME-CONTROL-TITLE".to_owned()), None],
        ),
    ] {
        let start = output.len();
        input.write_all(command.as_bytes()).unwrap();
        input.flush().unwrap();
        until_normalized_without_new_generation(&reader, &mut output, start, expected, &stub, 1);
        assert_eq!(
            title_events(&journal),
            published,
            "exactly one durable title event per command before release"
        );
    }
    assert!(
        !normalized(&output).contains("cannot change the session name"),
        "busy refusal"
    );
    let start = output.len();
    gate.release();
    until_prompt_from(&reader, &mut output, start);
    assert!(normalized(&output).contains("NAME-CONTROL-REPLY-FINISHED"));
    assert_eq!(
        stub.generations().len(),
        1,
        "title control reached the model"
    );
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert_eq!(
        title_events(&journal),
        vec![Some("NAME-CONTROL-TITLE".to_owned()), None]
    );
}
