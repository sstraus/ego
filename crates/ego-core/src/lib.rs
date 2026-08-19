//! The harness: loop, reducer, permissions, sessions, subagents, skills,
//! hooks, compaction, truncation — and the owner of the conversation types
//! and all ports.
//!
//! See `ARCHITECTURE.md` §2 (crate layout) and §3 (core traits).
//!
//! This crate sees the world through traits only: it opens no sockets and no
//! files. Ports belong to the consumer — `ego-core` declares them, the
//! satellite crates implement them.
