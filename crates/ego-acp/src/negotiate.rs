//! Which protocol version this agent agrees to speak.
//!

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
/// Serve stable v1 only. Configuring the draft v2 arm could opt a client
/// requesting a higher version into an unstable protocol it did not request.
/// The router's configured arms are the switch when another version stabilizes.
pub const SERVED: ProtocolVersion = ProtocolVersion::V1;

/// The version this agent answers `initialize` with, or `None` when the
/// connection is refused before it begins.
/// Negotiation is total over the schema's `u16` version value. Requests for
/// v1 or higher select v1; zero matches no configured SDK arm and is refused.
/// Do not use `min(wanted, max)` to select an unstable draft implicitly.
pub fn negotiate(wanted: ProtocolVersion) -> Option<ProtocolVersion> {
    // `>=` and not equality: the router selects the highest configured arm at
    // or below the ask, so every version from `SERVED` upward reaches this one.
    (wanted >= SERVED).then_some(SERVED)
}
