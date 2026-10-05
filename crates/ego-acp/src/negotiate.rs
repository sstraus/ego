//! Which protocol version this agent agrees to speak.
//!
//! See `ARCHITECTURE.md`, §3 as
//! corrected by §6.
//!
//! ## The SDK owns the negotiation; we own the configuration
//!
//! `agent-client-protocol`'s transport layer answers `initialize` before any
//! handler runs (`jsonrpc/protocol_compat.rs`), and
//! [`Agent::protocol_router`](agent_client_protocol::Agent::protocol_router)
//! picks the highest *configured* implementation at or below the requested
//! version. So the decision this module records is not "which integer does the
//! handler return" — it is **which arms [`connect`](crate::connect) configures**,
//! and this function is the statement of what that configuration answers.
//!
//! [`SERVED`] is the one arm. Everything below follows from there.
//!
//! ## It is still not an echo
//!
//! The SDK's `examples/simple_agent.rs` answers with
//! `InitializeResponse::new(initialize.protocol_version)`. That is shorthand
//! that only works for an agent whose behaviour does not depend on the answer.
//! Ours does: the version picks the projection
//! ([`project_acp_v1`](crate::project::project_acp_v1) or
//! [`project_acp_v2`](crate::project::project_acp_v2)), and those have
//! genuinely different variant sets — v1 returns `None` for events v2 can
//! carry.
//!
//! The router does rewrite the request's version to its selection before the
//! handler sees it, so echoing would *happen* to be right today. It would stop
//! being right the moment a second arm is configured, and it would be right by
//! accident even now. The handler states [`SERVED`] instead.

use agent_client_protocol_schema::ProtocolVersion;

/// The one protocol version this build serves.
///
/// Not `LATEST` — the schema crate withholds that constant under
/// `unstable_protocol_v2` precisely so a build cannot drift onto the draft by
/// writing "latest", and this crate enables that feature.
///
/// v1 and not v2, although [`project_acp_v2`](crate::project::project_acp_v2)
/// is complete: with both arms configured the router answers `2` to a client
/// asking for `3`, performing an opt-in into an unstable draft on the client's
/// behalf. That substitution is undetectable by the client and unrecoverable by
/// us; answering `1` to a client that wanted `2` is stated in the response and
/// recoverable by re-initializing. See ADR §6.2.
///
/// **This constant is the expiry switch.** When v2 stabilises, configuring the
/// second arm is what changes — not this module's shape.
pub const SERVED: ProtocolVersion = ProtocolVersion::V1;

/// The version this agent answers `initialize` with, or `None` when the
/// connection is refused before it begins.
///
/// Total by construction: `ProtocolVersion` is a `u16` newtype and *every*
/// value of it deserialises (the schema crate's own tests accept `0` and
/// `65535`, and reject `100000` only for exceeding `u16`), so this is not a
/// match over a closed enum and cannot be written as one.
///
/// # The rule
///
/// | asked | answered |
/// |---|---|
/// | `1` and above | `1` |
/// | `0` | refused |
///
/// **The upper row is the whole point, and is the opposite of
/// `min(wanted, our_max)`** — a client asking for `3` is not handed the v2
/// draft. See [`SERVED`].
///
/// **The lower row is not our choice.** `SupportedAgentProtocols::
/// highest_compatible` requires `requested >= V1`, so a `0` matches no arm and
/// the router replies with `invalid_request`. Making it anything else would
/// mean a transport shim ahead of the router rewriting the request — a parallel
/// implementation of the SDK's negotiation, to rescue a version the schema crate
/// calls a pre-release that "should likely be treated as unsupported". ADR §3.2
/// claimed we never refuse; ADR §6.3 corrects it.
pub fn negotiate(wanted: ProtocolVersion) -> Option<ProtocolVersion> {
    // `>=` and not equality: the router selects the highest configured arm at
    // or below the ask, so every version from `SERVED` upward reaches this one.
    (wanted >= SERVED).then_some(SERVED)
}
