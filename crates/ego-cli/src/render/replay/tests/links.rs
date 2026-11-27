use super::*;
use crate::output::TextSink;
use ego_core::sink::EventSink;
use ego_core::state::SessionState;

// Catches: live output or durable replay drops the admitted workspace or OSC 8 capability.
#[test]
fn live_and_replayed_answers_emit_the_same_workspace_link() {
    let root = tempfile::tempdir().unwrap();
    let mut seq = Seq::ZERO;
    let event = DomainEvent {
        env: Envelope {
            seq: seq.next(),
            id: EventId::new(),
            turn: TurnId::new(),
            item: Some(ItemId::new()),
            parent: None,
            ts: Timestamp::from_millis(1),
            producer: Producer::Main,
        },
        kind: EventKind::AssistantMessage {
            text: "[Rapporto](docs/report.md)".into(),
        },
    };
    let caps = Capabilities {
        depth: crate::render::Depth::TrueColor,
        hyperlinks: true,
        osc: crate::render::OscProfile::Standard,
        ..Capabilities::plain()
    };
    // Live output consumes deltas; replay prefers the durable final message.
    let mut delta = event.clone();
    delta.kind = EventKind::AssistantDelta {
        text: "[Rapporto](docs/report.md)".into(),
    };
    let mut event = event;
    event.env.seq = seq.next();
    event.env.id = EventId::new();
    let events = [delta, event];
    let replayed = Recorded::default();
    render(&events, &replayed, caps, Some(root.path()));
    let live = Recorded::default();
    let sink = TextSink::new(&live, false, caps, Some(root.path()));
    let mut state = SessionState::default();
    for event in &events {
        state.apply(event);
        sink.publish(&ego_core::public::project_public(event).unwrap(), &state);
    }
    sink.finish();
    let base = url::Url::from_directory_path(root.path()).unwrap();
    let expected = format!("\x1b]8;;{base}docs/report.md\x07");
    for text in [&*replayed.0.lock().unwrap(), &*live.0.lock().unwrap()] {
        assert!(text.contains(&expected), "{text:?}");
        assert!(text.contains("Rapporto"));
        assert!(!text.contains("(docs/report.md)"));
    }
}
