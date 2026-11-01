use super::*;
use ego_core::event::id::{ItemId, SessionId, TurnId};
use serde_json::json;

// Catches: metadata escapes its frame, exceeds the decoder cap or leaks into ordinary output.
#[test]
fn native_frames_are_bounded_and_tuic_only() {
    let session = SessionId::new();
    let turn = TurnId::new();
    let item = ItemId::new();
    for profile in [OscProfile::None, OscProfile::Standard, OscProfile::Tuic] {
        let osc = Osc::new(Capabilities {
            osc: profile,
            ..Capabilities::plain()
        });
        let bytes = osc.agent_event(
            session,
            Some(turn),
            "modal_open",
            Some(item),
            json!({"prompt": "你好;\u{7}\u{1b}\\"}),
        );
        if profile != OscProfile::Tuic {
            assert!(bytes.is_empty());
            continue;
        }
        let payload = bytes
            .strip_prefix("\x1b]7770;agent-event=")
            .unwrap()
            .strip_suffix("\x1b\\")
            .unwrap();
        assert!(!payload.contains(['\x07', '\x1b']));
        let frame: serde_json::Value = serde_json::from_slice(
            &base64::engine::general_purpose::STANDARD
                .decode(payload)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            frame,
            json!({"version": 1, "agent": "ego", "session": session, "turn": turn, "event": "modal_open", "id": item, "data": {"prompt": "你好;\u{7}\u{1b}\\"}})
        );
        assert!(
            osc.agent_event(
                session,
                Some(turn),
                "modal_open",
                Some(item),
                json!({"prompt": "x".repeat(16 * 1024)})
            )
            .is_empty()
        );
        assert_eq!(osc.state(State::Awaiting), "\x1b]7770;state=awaiting\x07");
    }
}
