use super::*;
use crate::output::TextSink;
use ego_core::event::domain::ToolTiming;
use ego_core::sink::EventSink;
use ego_core::state::SessionState;

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
                    args: r#"{"cmd":"pwd"}"#.into(),
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
        render(&events, &replayed, Capabilities::plain());
        let live = Recorded::default();
        let sink = TextSink::new(&live, false, Capabilities::plain());
        let mut state = SessionState::default();
        for event in &events {
            state.apply(event);
            if let Some(public) = ego_core::public::project_public(event) {
                sink.publish(&public, &state);
            }
        }
        let actual = replayed.0.lock().unwrap();
        assert_eq!(*live.0.lock().unwrap(), *actual);
        match execution_ms {
            Some(200) => assert!(actual.contains("200ms"), "{actual}"),
            Some(0) => assert!(actual.contains("0ms"), "{actual}"),
            None => assert!(actual.contains("duration unavailable"), "{actual}"),
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
