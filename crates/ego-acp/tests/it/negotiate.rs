//! The version this agent answers `initialize` with, as a table.
//!
//! The negotiation table is tested here as a pure function and through the
//! real SDK router in `tests/initialize.rs`. Both matter because the SDK, not
//! this table alone, owns the protocol negotiation.
//!
//! What this file has that the transport test cannot is *reach*. An
//! `initialize` over a socket can carry `1` and `2`; the inputs that matter
//! most are the ones an arbitrary peer can send and no enum can express.

use agent_client_protocol_schema::ProtocolVersion;
use ego_acp::negotiate::{SERVED, negotiate};

/// The one we serve, answered with itself.
#[test]
fn the_version_we_serve_is_the_version_we_answer_with() {
    assert_eq!(negotiate(SERVED), Some(SERVED));
    assert_eq!(SERVED, ProtocolVersion::V1);
}

/// **The row the obvious rule gets wrong.**
/// Requests above v1 still select v1, not an implicitly selected unstable
/// v2 draft. The v2 router arm is deliberately not configured.
#[test]
fn a_version_newer_than_ours_gets_the_stable_one_and_never_the_draft() {
    for newer in [2u16, 3, 4, 100, 65535] {
        let answer = negotiate(ProtocolVersion::from(newer));
        assert_eq!(
            answer,
            Some(ProtocolVersion::V1),
            "a client asking for {newer} was handed {answer:?}: it never opted \
             into the v2 draft, and this build configures no arm to hand it"
        );
        assert_ne!(
            answer,
            Some(ProtocolVersion::V2),
            "the draft was selected on a client's behalf"
        );
    }
}

/// The pre-release, refused.
/// `SupportedAgentProtocols::highest_compatible` requires `requested >= V1`.
/// Zero matches no configured arm, so the SDK refuses before our handler runs.
#[test]
fn the_pre_release_version_is_refused_because_the_router_has_no_arm_below_one() {
    assert_eq!(negotiate(ProtocolVersion::V0), None);
}

/// Whatever arrives, what leaves is a version this build actually serves.
///
/// The property behind the table: `negotiate` is total, and its range is
/// [`SERVED`] and refusal — nothing else. A future row that answered with some
/// other integer would fail here without needing its own test, which is the
/// point, since the failure mode being guarded is an agent naming a protocol it
/// does not speak.
#[test]
fn every_answer_is_the_version_this_build_serves() {
    for wanted in [0u16, 1, 2, 3, 7, 255, 256, 65535] {
        let answer = negotiate(ProtocolVersion::from(wanted));
        assert!(
            answer.is_none() || answer == Some(SERVED),
            "asked for {wanted}, answered {answer:?}, which this build does not speak"
        );
    }
}

/// Answering is not echoing, stated as a difference rather than implied.
///
/// This is the assertion that fails if someone "simplifies" the `initialize`
/// handler to the SDK example's `InitializeResponse::new(request.protocol_
/// version)`. Every other test here would still pass: the router narrows the
/// request to the selected arm before a handler sees it, so today an echo would
/// return the right number for the wrong reason — and would return the wrong
/// number the moment a second arm is configured.
#[test]
fn the_answer_is_not_merely_the_question_repeated() {
    let asked = ProtocolVersion::from(9u16);
    assert_ne!(
        negotiate(asked),
        Some(asked),
        "the agent echoed the client's version back, claiming to speak 9"
    );
}
