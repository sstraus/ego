//! The inbound A2A adapter: ego, reachable by another agent over HTTP.
//!
//! A2A (Agent2Agent, Linux Foundation) is how agents discover each other and
//! delegate work across frameworks. ego already serves one host protocol — ACP,
//! over stdio — and `ego-acp` is the shape this crate mirrors: an adapter over
//! collaborators the composition root owns, with the transport as the only
//! difference.
//!
//! Three decisions were taken before any of this was written, and each is
//! visible in the module layout rather than only in a commit message.
//!
//! **The contract comes from `a2a-lf`, the bytes are ours.** `a2a-lf` is the
//! protocol maintainers' own crate and is pure data — six modules of serde
//! types, no async, no transport. What it buys is the one thing that matters in
//! an interoperability protocol: being byte-compatible with the other
//! implementations. It earned its place immediately. Read as prose, the
//! specification site gives the JSON-RPC method names as `message/send` and
//! `tasks/get`; those are v0.2/v0.3, and A2A v1.0 uses the proto-derived
//! `SendMessage` and `GetTask`, which is what `a2a_lf::jsonrpc::methods`
//! carries and what the reference server routes. Hand-rolling the wire would
//! not have failed a test — it would have failed to be answered.
//!
//! **The transport is `hyper` directly, not `axum`.** See [`serve`].
//!
//! **The listener is not this crate's.** `ego-listen` binds, accepts and
//! establishes who connected, and `ego_core::endpoint` decides which endpoint
//! kinds exist and which authentication each compels. A2A serves `hyper` over
//! the stream it is handed and ACP serves `ByteStreams` over the same one; see
//! `docs/decisions/2026-09-07-one-inbound-endpoint-serves-every-host-protocol.md`.

pub mod auth;
pub mod card;
pub mod serve;

/// The A2A protocol version this adapter speaks, from the contract crate.
pub use a2a_lf::VERSION as PROTOCOL_VERSION;
