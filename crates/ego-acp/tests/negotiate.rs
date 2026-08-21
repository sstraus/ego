//! The version this agent answers `initialize` with.
//!
//! `docs/decisions/2026-08-21-the-version-an-agent-answers-with.md` §3 is a
//! four-row table, and this file is that table. It is tested here rather than
//! through a live connection for the reason §4 gives: a transport test can
//! reach `1` and `2`, and the inputs that matter most are the ones an
//! arbitrary peer can send and no enum can express.

use agent_client_protocol_schema::ProtocolVersion;
use ego_acp::negotiate::negotiate;

/// The two we implement, answered with themselves.
///
/// The v2 row is the only one where echoing and deciding agree, which is
/// exactly why the SDK's echoing example looks correct until it isn't.
#[test]
fn a_version_we_implement_is_the_version_we_answer_with() {
    assert_eq!(negotiate(ProtocolVersion::V1), ProtocolVersion::V1);
    assert_eq!(negotiate(ProtocolVersion::V2), ProtocolVersion::V2);
}

/// **The row the obvious rule gets wrong.**
///
/// `min(wanted, our_max)` answers `2` here. We answer `1`, because v2 is an
/// unstable draft and a client asking for `3` has not opted into it — see
/// `negotiate`'s doc comment and ADR §3.1.
///
/// `65535` is in the same row and is not padding: `ProtocolVersion` is a `u16`
/// newtype whose own tests confirm the maximum deserialises, so it is a value
/// a real peer can put on the wire.
#[test]
fn a_version_newer_than_ours_gets_the_stable_one_and_never_the_draft() {
    for newer in [3u16, 4, 100, 65535] {
        let answer = negotiate(ProtocolVersion::from(newer));
        assert_eq!(
            answer,
            ProtocolVersion::V1,
            "a client asking for {newer} was handed {answer}: it never opted \
             into the v2 draft, and only asking for 2 by name is that opt-in"
        );
        assert_ne!(
            answer,
            ProtocolVersion::V2,
            "the draft was selected on a client's behalf"
        );
    }
}

/// The pre-release, answered with the lowest thing we actually implement.
///
/// Not an error: ACP gives the client the decision about whether our answer is
/// usable, and an agent that fails `initialize` takes that decision away
/// (ADR §3.2).
#[test]
fn the_pre_release_version_is_answered_with_one_rather_than_refused() {
    assert_eq!(negotiate(ProtocolVersion::V0), ProtocolVersion::V1);
}

/// Whatever arrives, what leaves is a version this build can actually speak.
///
/// The property behind the table: `negotiate` is total, and its range is the
/// two versions `project.rs` has a projection for. A future row added to §3
/// that answered with anything else would fail here without needing its own
/// test — which is the point, since the failure mode being guarded is an agent
/// naming a protocol it does not implement.
#[test]
fn every_answer_is_a_version_this_build_projects_onto() {
    for wanted in [0u16, 1, 2, 3, 7, 255, 256, 65535] {
        let answer = negotiate(ProtocolVersion::from(wanted));
        assert!(
            answer == ProtocolVersion::V1 || answer == ProtocolVersion::V2,
            "asked for {wanted}, answered {answer}, which no projection covers"
        );
    }
}

/// Answering is not echoing, stated as a difference rather than implied.
///
/// This is the assertion that fails if someone "simplifies" `negotiate` to the
/// SDK example's `InitializeResponse::new(initialize.protocol_version)`. Every
/// other test here would still pass for the two rows where the two rules agree.
#[test]
fn the_answer_is_not_merely_the_question_repeated() {
    let asked = ProtocolVersion::from(9u16);
    assert_ne!(
        negotiate(asked),
        asked,
        "the agent echoed the client's version back, claiming to implement 9"
    );
}
