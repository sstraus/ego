use super::*;

fn writes(count: usize, identical: bool) -> Vec<AiEvent> {
    let mut response: Vec<_> = (0..count)
        .map(|index| AiEvent::ToolCall {
            call_id: format!("call-{index}"),
            name: "fake_write".into(),
            args: json!({"path":"/ws/out.txt", "contents": if identical { 0 } else { index }.to_string()}).to_string(),
        })
        .collect();
    response.push(AiEvent::Stop(StopReason::ToolUse));
    response
}

// Catches: distinct provider ids bypass a response limit and execute every write.
#[test]
fn excessive_response_executes_no_tools() {
    for (count, identical) in [(129, false), (9, true)] {
        let mut response = writes(count, identical);
        response.insert(
            response.len() - 1,
            AiEvent::TextDelta {
                text: "unconsumed suffix".into(),
            },
        );
        let mut fx =
            Fixture::new(vec![response, vec![AiEvent::Stop(StopReason::Stop)]]).with_write_tool();
        fx.set_mode(Mode::Yolo);
        fx.fs.file("/ws/out.txt", "original");
        let (reason, ok, state) = run(&fx, SessionState::default(), "write");
        assert_eq!(
            fx.fs.read(&CanonPath::new("/ws/out.txt")).unwrap(),
            b"original"
        );
        assert_eq!(reason, ExitReason::Guard);
        assert!(!ok);
        assert_eq!(fx.generator.calls().len(), 1, "no automatic retry");
        assert!(
            state
                .items
                .values()
                .all(|item| !item.text.contains("unconsumed suffix"))
        );
        assert!(
            state
                .items
                .values()
                .filter(|item| item.kind == ItemKind::ToolCall)
                .all(|item| item.ok == Some(false))
        );
        assert!(fx.store.events().iter().any(|event| matches!(&event.kind,
            EventKind::Error { kind: ErrorKind::Invalid, message } if message.contains("tool call limit"))));
    }
}

// Catches: an off-by-one response guard refuses the last admitted call.
#[test]
fn response_limits_allow_exact_boundaries() {
    for (count, identical) in [(128, false), (8, true)] {
        let mut fx = Fixture::new(vec![
            writes(count, identical),
            vec![AiEvent::Stop(StopReason::Stop)],
        ])
        .with_write_tool();
        fx.set_mode(Mode::Yolo);
        let (reason, ok, state) = run(&fx, SessionState::default(), "write");
        assert_eq!(reason, ExitReason::Completed);
        assert!(ok);
        assert_eq!(
            state
                .items
                .values()
                .filter(|item| item.kind == ItemKind::ToolCall && item.ok == Some(true))
                .count(),
            count
        );
    }
}

// Catches: a large or unknown model maximum becomes the default output allowance.
#[test]
fn response_output_defaults_to_a_bounded_request() {
    for maximum in [None, Some(200_000), Some(1_024)] {
        let fx = Fixture::new(vec![vec![AiEvent::Stop(StopReason::Stop)]]);
        let mut settings = config();
        settings.max_output_tokens = maximum;
        let (reason, _, _) = run_with(
            &fx,
            SessionState::default(),
            "answer",
            metering(),
            Some(rates()),
            None,
            settings,
        );
        assert_eq!(reason, ExitReason::Completed);
        assert_eq!(
            fx.generator.calls()[0].max_output_tokens,
            Some(if maximum == Some(1_024) {
                1_024
            } else {
                32_768
            })
        );
    }
}

// Catches: JSON formatting changes bypass the turn-wide repeat guard.
#[test]
fn repeated_responses_warn_then_stop() {
    let script = (0..8)
        .map(|index| {
            vec![
                AiEvent::ToolCall {
                    call_id: format!("repeat-{index}"),
                    name: "fake_write".into(),
                    args: if index == 1 {
                        "{ \"contents\": \"changed\", \"path\": \"/ws/out.txt\" }".into()
                    } else {
                        r#"{"path":"/ws/out.txt","contents":"changed"}"#.into()
                    },
                },
                AiEvent::Stop(StopReason::ToolUse),
            ]
        })
        .chain([vec![AiEvent::Stop(StopReason::Stop)]])
        .collect();
    let mut fx = Fixture::new(script).with_write_tool();
    fx.set_mode(Mode::Yolo);
    let (reason, ok, state) = run(&fx, SessionState::default(), "write");
    assert_eq!(reason, ExitReason::Guard);
    assert!(!ok);
    assert_eq!(
        state
            .items
            .values()
            .filter(|item| item.kind == ItemKind::ToolCall && item.ok == Some(true))
            .count(),
        7
    );
    let requests = fx.generator.calls();
    assert_eq!(requests.len(), 8);
    assert!(requests[3].messages.messages().iter().flat_map(|message| &message.content).any(|block|
        matches!(block, ContentBlock::Text { text } if text.contains("Tool loop warning"))));
}

// Catches: a loop evades repetition detection by changing every argument.
#[test]
fn different_calls_stop_at_turn_budget() {
    let scripts = (0..33)
        .map(|batch| {
            let mut calls: Vec<_> = (0..128)
                .map(|index| AiEvent::ToolCall {
                    call_id: format!("unique-{batch}-{index}"),
                    name: "fake_write".into(),
                    args:
                        json!({"path":"/ws/out.txt","contents":(batch * 128 + index).to_string()})
                            .to_string(),
                })
                .collect();
            calls.push(AiEvent::Stop(StopReason::ToolUse));
            calls
        })
        .chain([vec![AiEvent::Stop(StopReason::Stop)]])
        .collect();
    let mut fx = Fixture::new(scripts).with_write_tool();
    fx.set_mode(Mode::Yolo);
    let (reason, ok, _) = run(&fx, SessionState::default(), "write");
    assert_eq!(reason, ExitReason::Guard);
    assert!(!ok);
    assert_eq!(fx.fs.read(&CanonPath::new("/ws/out.txt")).unwrap(), b"4095");
    assert_eq!(fx.generator.calls().len(), 33);
}

// Catches: a default ceiling overrides an explicit setting or exceeds the model maximum.
#[test]
fn explicit_output_limit_replaces_default_and_respects_model_maximum() {
    for (maximum, requested, expected) in [
        (None, 50_000, 50_000),
        (Some(200_000), 50_000, 50_000),
        (Some(1_024), 50_000, 1_024),
    ] {
        let generator = Arc::new(FakeGenerator::new([]));
        let mut settings = config();
        settings.max_output_tokens = maximum;
        let source = FixedGenerationSource {
            generator,
            settings,
            metering: metering(),
            rates: Some(rates()),
        };
        let mut profile = block_on(
            ego_core::generation_model_source::GenerationModelSource::resolve(
                &source,
                ego_core::generation_model_source::GenerationModelRequest::new(
                    GenerationRole::Main,
                ),
            ),
        )
        .unwrap();
        profile.output_limit = std::num::NonZeroU64::new(requested);
        assert_eq!(profile.output_ceiling(), Some(expected));
    }
}
