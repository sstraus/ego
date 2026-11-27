use super::*;

// Catches: decoded model control tokens reach tools, including after JSON unescaping.
#[test]
fn control_tokens_in_arguments_refuse_the_entire_response() {
    for contents in [
        "left<|control28|>right",
        "<|control0|>",
        "<|control999999|>",
    ] {
        for escaped in [false, true] {
            let args = json!({"path":"/ws/out.txt", "contents": contents}).to_string();
            let args = if escaped {
                args.replace('<', "\\u003c").replace('>', "\\u003e")
            } else {
                args
            };
            let mut fx = Fixture::new(vec![
                vec![
                    AiEvent::ToolCall {
                        call_id: "clean-sibling".into(),
                        name: "fake_write".into(),
                        args: json!({"path":"/ws/out.txt", "contents":"first"}).to_string(),
                    },
                    AiEvent::ToolCall {
                        call_id: "control-args".into(),
                        name: "fake_write".into(),
                        args,
                    },
                    AiEvent::Stop(StopReason::ToolUse),
                ],
                vec![AiEvent::Stop(StopReason::Stop)],
            ])
            .with_write_tool();
            fx.set_mode(Mode::Yolo);
            fx.fs.file("/ws/out.txt", "original");
            let (reason, ok, state) = run(&fx, SessionState::default(), "write");
            assert_eq!(
                fx.fs.read(&CanonPath::new("/ws/out.txt")).unwrap(),
                b"original"
            );
            assert!(!ok);
            assert_eq!(reason, ExitReason::Guard);
            assert_eq!(fx.generator.calls().len(), 1);
            assert!(
                state
                    .items
                    .values()
                    .filter(|item| item.kind == ItemKind::ToolCall)
                    .all(|item| item.ok == Some(false))
            );
            assert!(fx.store.events().iter().any(|event| matches!(&event.kind, EventKind::Error { kind:ErrorKind::Invalid, message } if message.contains("model control token"))));
        }
    }
}

// Catches: normal punctuation or explanatory near-spellings are incorrectly refused.
#[test]
fn similar_text_in_arguments_is_allowed() {
    for contents in [
        "<|controlx|>",
        "<|control28>",
        "<control28|>",
        "<|control|>",
        "<|Control28|>",
        "<|control28 |>",
        "<|control１２|>",
    ] {
        let mut fx = Fixture::new(vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "ordinary".into(),
                    name: "fake_write".into(),
                    args: json!({"path":"/ws/out.txt", "contents":contents}).to_string(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![AiEvent::Stop(StopReason::Stop)],
        ])
        .with_write_tool();
        fx.set_mode(Mode::Yolo);
        let (reason, ok, _) = run(&fx, SessionState::default(), "write");
        assert!(ok);
        assert_eq!(reason, ExitReason::Completed);
        assert_eq!(
            fx.fs.read(&CanonPath::new("/ws/out.txt")).unwrap(),
            contents.as_bytes()
        );
    }
}
