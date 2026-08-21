//! Which protocol version this agent agrees to speak.
//!
//! See `docs/decisions/2026-08-21-the-version-an-agent-answers-with.md` §3.
//!
//! One function, because the whole decision is one: a client states the
//! version it wants in `initialize`, and the agent's response states the
//! version it will actually use. Those are different questions, and the SDK's
//! own `examples/simple_agent.rs` answers the second with the first —
//! `InitializeResponse::new(initialize.protocol_version)` — which is shorthand
//! that only works for an agent whose behaviour does not depend on the answer.
//!
//! Ours does. The negotiated version picks the projection
//! ([`project_acp_v1`](crate::project::project_acp_v1) or
//! [`project_acp_v2`](crate::project::project_acp_v2)), and those have
//! genuinely different variant sets — v1 returns `None` for events v2 can
//! carry. Echoing would let the agent answer "2" and then speak v1 at a client
//! that believes it agreed to v2.

use agent_client_protocol_schema::ProtocolVersion;

/// The version this agent speaks when the client asks for `wanted`.
///
/// Total by construction: `ProtocolVersion` is a `u16` newtype and *every*
/// value of it deserialises (the schema crate's own tests cover `0`, `100000`
/// rejected only for exceeding `u16`, and `65535` accepted), so this is not a
/// match over a closed enum and cannot be written as one. An arbitrary integer
/// arrives and an implemented version leaves.
///
/// # The rule
///
/// `V2` exactly when asked for by name; `V1` otherwise — including for
/// versions *newer* than either. That last clause is the whole point and is
/// the opposite of `min(wanted, our_max)`:
///
/// v2 is an unstable draft. The schema crate withholds
/// `ProtocolVersion::LATEST` under `unstable_protocol_v2` precisely so that
/// opting into the draft has to be written out, and answering `V2` to a client
/// that asked for `3` would perform that opt-in on its behalf. It asked for a
/// stable future version and would be handed a draft whose shape can change
/// under it — a substitution it cannot detect.
///
/// The cost is real and bounded the other way: a v2-capable client that asked
/// for `3` gets v1's documented losses (see `project.rs`) until it asks for
/// `2` by name. A wrong answer here is recoverable; the other wrong answer is
/// not.
///
/// **This function expires when v2 stabilises.** Delete it rather than
/// generalising it — it encodes one fact about one draft, not a theory of
/// negotiation.
pub fn negotiate(wanted: ProtocolVersion) -> ProtocolVersion {
    match wanted {
        ProtocolVersion::V2 => ProtocolVersion::V2,
        // Everything else, and deliberately not a range check. `V0` is
        // documented by the schema crate as a pre-release that "should likely
        // be treated as unsupported", and a version above ours has not opted
        // into the draft. Both get the one version we implement that is not a
        // draft, and the client decides whether it can live with it — ACP
        // gives that decision to the client, so refusing here would take it
        // away (ADR §3.2).
        _ => ProtocolVersion::V1,
    }
}
