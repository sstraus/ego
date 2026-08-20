//! Projection of `ego-core` domain events onto ACP `SessionUpdate` values.
//!
//! See `ARCHITECTURE.md` §7 and `plans/m0a-contracts.md` Step 9. Projection
//! functions only — no transport, no server, no state of our own. The types
//! come from the real `agent-client-protocol-schema` crate rather than
//! hand-rolled mirrors, so a schema bump is a compile error instead of a
//! silently stale golden.
