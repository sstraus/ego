use super::*;
use crate::output::TextSink;
use ego_core::event::domain::ToolTiming;
use ego_core::sink::EventSink;
use ego_core::state::SessionState;

// Catches: replay or live output reuses one file's before-image for both writes.
#[test]
fn multifile_write_live_and_replay_show_each_files_own_diff() {
    use ego_core::permit::{BeforeImage, FileWriteEvidence, ToolResult};
    let item = ItemId::new();
    let turn = TurnId::new();
    let mut seq = Seq::ZERO;
    let mut event = |kind| DomainEvent {
        env: Envelope {
            seq: seq.next(),
            id: EventId::new(),
            turn,
            item: Some(item),
            parent: None,
            ts: Timestamp::from_millis(1),
            producer: Producer::Main,
        },
        kind,
    };
    let result = ToolResult::builtin(
        r#"{"files":[{"path":"a","status":"completed"},{"path":"b","status":"completed"}]}"#,
    )
    .with_file_writes(vec![
        FileWriteEvidence {
            path: "/ws/a".into(),
            completed: true,
            before_image: Some(BeforeImage::Text("old-a\n".into())),
            after_image: Some("new-a\n".into()),
            checkpoint: Some("a".into()),
            possibly_modified: true,
            summary: String::new(),
            notice: None,
        },
        FileWriteEvidence {
            path: "/ws/b".into(),
            completed: true,
            before_image: Some(BeforeImage::Text("old-b\n".into())),
            after_image: Some("new-b\n".into()),
            checkpoint: Some("b".into()),
            possibly_modified: true,
            summary: String::new(),
            notice: None,
        },
    ]);
    let events = [
        event(EventKind::ToolCallStarted {
            name: "write".into(),
            args:
                r#"{"files":[{"path":"a","contents":"new-a\n"},{"path":"b","contents":"new-b\n"}]}"#
                    .into(),
            call_id: None,
        }),
        event(EventKind::ToolCallCompleted { ok: true, result }),
    ];
    let replayed = Recorded::default();
    render(&events, &replayed, Capabilities::plain(), None);
    let live = Recorded::default();
    let sink = TextSink::new(&live, false, Capabilities::plain(), None);
    let mut state = SessionState::default();
    for event in &events {
        state.apply(event);
        if let Some(public) = ego_core::public::project_public(event) {
            sink.publish(&public, &state);
        }
    }
    let actual = replayed.0.lock().unwrap();
    assert_eq!(*live.0.lock().unwrap(), *actual);
    for expected in ["old-a", "new-a", "old-b", "new-b"] {
        assert!(actual.contains(expected), "{actual}");
    }
}

// Catches: amended outcomes disappear or are paired with the original request's bytes.
#[test]
fn amended_write_live_and_replay_show_actual_outcomes_not_original_bytes() {
    use ego_core::permit::{BeforeImage, FileWriteEvidence, ToolResult};
    let item = ItemId::new();
    let turn = TurnId::new();
    let mut seq = Seq::ZERO;
    let mut event = |kind| DomainEvent {
        env: Envelope {
            seq: seq.next(),
            id: EventId::new(),
            turn,
            item: Some(item),
            parent: None,
            ts: Timestamp::from_millis(1),
            producer: Producer::Main,
        },
        kind,
    };
    let result = ToolResult::builtin(r#"{"files":[{"index":0,"path":"actual-a","status":"completed","summary":" (+1 -1 lines)"},{"index":1,"path":"actual-b","status":"failed","error":"disk is full","possibly_modified":true,"summary":""},{"index":2,"path":"actual-c","status":"unattempted"}]}"#).with_file_writes(vec![
        FileWriteEvidence { path: "/ws/actual-a".into(), completed: true, before_image: Some(BeforeImage::Text("old-a\n".into())), after_image: Some("new-a\n".into()), checkpoint: None, possibly_modified: true, summary: " (+1 -1 lines)".into(), notice: None },
        FileWriteEvidence { path: "/ws/actual-b".into(), completed: false, before_image: Some(BeforeImage::Text("old-b\n".into())), after_image: None, checkpoint: None, possibly_modified: true, summary: String::new(), notice: None },
    ]);
    let events = [
        event(EventKind::ToolCallStarted {
            name: "write".into(),
            args: r#"{"files":[{"path":"actual-a","contents":"bad-a\n"}]}"#.into(),
            call_id: None,
        }),
        event(EventKind::ToolCallCompleted { ok: false, result }),
    ];
    let replayed = Recorded::default();
    render(&events, &replayed, Capabilities::plain(), None);
    let live = Recorded::default();
    let sink = TextSink::new(&live, false, Capabilities::plain(), None);
    let mut state = SessionState::default();
    for event in &events {
        state.apply(event);
        if let Some(public) = ego_core::public::project_public(event) {
            sink.publish(&public, &state);
        }
    }
    let actual = replayed.0.lock().unwrap();
    assert_eq!(*live.0.lock().unwrap(), *actual);
    for expected in [
        "actual-a",
        "actual-b",
        "actual-c",
        "disk is full",
        "may modify",
        "new-a",
        "+1 -1",
    ] {
        assert!(actual.contains(expected), "{actual}");
    }
    assert!(!actual.contains("bad-a"), "{actual}");
}

// Catches: edit batches render the requested fragments instead of completed file evidence.
#[test]
fn multifile_edit_live_and_replay_render_actual_partial_outcomes() {
    use ego_core::permit::{BeforeImage, FileWriteEvidence, ToolResult};
    let item = ItemId::new();
    let turn = TurnId::new();
    let mut seq = Seq::ZERO;
    let mut event = |kind| DomainEvent {
        env: Envelope {
            seq: seq.next(),
            id: EventId::new(),
            turn,
            item: Some(item),
            parent: None,
            ts: Timestamp::from_millis(1),
            producer: Producer::Main,
        },
        kind,
    };
    let result = ToolResult::builtin(r#"{"files":[{"path":"./a","status":"completed","summary":" (+1 -1 lines)"},{"path":"./b","status":"completed","summary":" (+1 -1 lines)"},{"path":"c","status":"failed","error":"disk full","possibly_modified":true},{"path":"d","status":"unattempted"}]}"#).with_file_writes(vec![
        FileWriteEvidence { path: "/ws/a".into(), completed: true, before_image: Some(BeforeImage::Text("old-a\n".into())), after_image: Some("new-a\n".into()), checkpoint: None, possibly_modified: true, summary: String::new(), notice: None },
        FileWriteEvidence { path: "/ws/b".into(), completed: true, before_image: Some(BeforeImage::Text("old-b\n".into())), after_image: Some("new-b\n".into()), checkpoint: None, possibly_modified: true, summary: String::new(), notice: None },
        FileWriteEvidence { path: "/ws/c".into(), completed: false, before_image: None, after_image: None, checkpoint: None, possibly_modified: true, summary: String::new(), notice: None },
    ]);
    let events = [event(EventKind::ToolCallStarted { name: "edit".into(), args: r#"{"files":[{"path":"wrong","chunks":[{"old_string":"bogus","new_string":"invented"}]}]}"#.into(), call_id: None }), event(EventKind::ToolCallCompleted { ok: false, result })];
    let replayed = Recorded::default();
    render(&events, &replayed, Capabilities::plain(), None);
    let live = Recorded::default();
    let sink = TextSink::new(&live, false, Capabilities::plain(), None);
    let mut state = SessionState::default();
    for event in &events {
        state.apply(event);
        if let Some(public) = ego_core::public::project_public(event) {
            sink.publish(&public, &state);
        }
    }
    for output in [&replayed, &live] {
        let actual = output.0.lock().unwrap();
        for expected in [
            "edit ./a",
            "old-a",
            "new-a",
            "+1 -1",
            "edit ./b",
            "old-b",
            "new-b",
            "edit c",
            "disk full",
            "may modify",
            "edit d",
            "unattempted",
        ] {
            assert!(actual.contains(expected), "missing {expected}: {actual}");
        }
        assert!(!actual.contains("invented"), "{actual}");
    }
}

// Catches: wall-clock jumps changing replay duration, approval counted as execution,
// and live output ignoring the measurements already reduced before completion.
#[test]
fn live_and_replay_preserve_execution_and_approval_measurements() {
    for (execution_ms, approval_wait_ms) in [(Some(200), 15_000), (Some(0), 0), (None, 15_000)] {
        let turn = TurnId::new();
        let item = ItemId::new();
        let mut seq = Seq::ZERO;
        let mut event = |ts, kind| DomainEvent {
            env: Envelope {
                seq: seq.next(),
                id: EventId::new(),
                turn,
                item: Some(item),
                parent: None,
                ts: Timestamp::from_millis(ts),
                producer: Producer::Main,
            },
            kind,
        };
        let events = [
            event(
                16_000,
                EventKind::ToolCallStarted {
                    name: "exec".into(),
                    args: r#"{"argv":["pwd"]}"#.into(),
                    call_id: None,
                },
            ),
            event(
                100,
                EventKind::ToolCallMeasured {
                    timing: ToolTiming {
                        execution_ms,
                        approval_wait_ms,
                    },
                },
            ),
            event(
                50,
                EventKind::ToolCallCompleted {
                    ok: execution_ms.is_some(),
                    result: ego_core::permit::ToolResult::builtin("/ws"),
                },
            ),
        ];
        let replayed = Recorded::default();
        render(&events, &replayed, Capabilities::plain(), None);
        let live = Recorded::default();
        let sink = TextSink::new(&live, false, Capabilities::plain(), None);
        let mut state = SessionState::default();
        for event in &events {
            state.apply(event);
            if let Some(public) = ego_core::public::project_public(event) {
                sink.publish(&public, &state);
            }
        }
        let actual = replayed.0.lock().unwrap();
        let live_text = live.0.lock().unwrap();
        assert!(live_text.contains("requested: $"), "{live_text}");
        assert!(live_text.contains(actual.as_str()), "{live_text}");
        match execution_ms {
            Some(200) => assert!(actual.contains("200ms"), "{actual}"),
            Some(0) => assert!(actual.contains("0ms"), "{actual}"),
            None => assert!(!actual.contains("duration unavailable"), "{actual}"),
            _ => unreachable!(),
        }
        assert_eq!(
            actual.contains("approval 15.0s"),
            approval_wait_ms > 0,
            "{actual}"
        );
        assert!(!actual.contains("15.9s"));
        let encoded = ego_core::store::codec::encode_record(events[1].env.seq, &events[1]).unwrap();
        let ego_core::store::codec::StoreLine::Record(record) =
            ego_core::store::codec::decode_line(&encoded).unwrap()
        else {
            panic!("measurement must decode as a durable record");
        };
        let decoded: DomainEvent = serde_json::from_str(record.payload.get()).unwrap();
        assert_eq!(decoded, events[1]);
    }
}
