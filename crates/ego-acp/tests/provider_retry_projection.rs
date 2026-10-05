//! regression case: the wire shape of the live provider retry status, which
//! TUIC regression case renders.

use agent_client_protocol_schema::v1::SessionUpdate;
use ego_acp::project::{provider_retry_cleared_v1, provider_retry_v1};
use ego_core::event::id::TurnId;
use ego_core::generator::types::ErrorKind;
use ego_core::public::PublicEvent;

/// Catches: a retry status without its counter, wait or cause, or one a
/// client without ego support cannot parse.
#[test]
fn a_retry_is_a_session_info_patch_with_the_counter_the_wait_and_the_cause() {
    let event = PublicEvent::ProviderRetry {
        turn: TurnId::new(),
        attempt: 2,
        limit: 6,
        delay_ms: 1_500,
        kind: ErrorKind::Transient,
        cause: "Our servers are currently overloaded. Please try again later.".to_owned(),
    };
    let wire = serde_json::to_value(provider_retry_v1(&event).expect("a retry status")).unwrap();
    assert_eq!(wire["sessionUpdate"], "session_info_update");
    assert!(
        wire.get("title").is_none(),
        "the patch leaves the title alone"
    );
    let status = &wire["_meta"]["ego"]["providerRetry"];
    assert_eq!(status["state"], "waiting");
    assert_eq!(status["attempt"], 2);
    assert_eq!(status["limit"], 6);
    assert_eq!(status["delayMs"], 1_500);
    assert_eq!(status["kind"], "transient");
    assert_eq!(
        status["cause"],
        "Our servers are currently overloaded. Please try again later."
    );
    assert_eq!(
        status["text"],
        "Our servers are currently overloaded. Please try again later. — connection \
         problem, retrying in 2s (attempt 2/6)"
    );

    let cleared = serde_json::to_value(provider_retry_cleared_v1()).unwrap();
    assert_eq!(cleared["sessionUpdate"], "session_info_update");
    assert!(cleared["_meta"]["ego"]["providerRetry"].is_null());

    for update in [wire, cleared] {
        let mut plain = update;
        plain.as_object_mut().unwrap().remove("_meta");
        let parsed: SessionUpdate = serde_json::from_value(plain)
            .expect("a client without ego support still receives a valid ACP update");
        assert!(matches!(parsed, SessionUpdate::SessionInfoUpdate(_)));
    }
    assert!(
        provider_retry_v1(&PublicEvent::TurnStarted {
            turn: TurnId::new()
        })
        .is_none()
    );
}
