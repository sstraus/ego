//! The harness: loop, reducer, permissions, sessions, subagents, skills,
//! hooks, compaction, truncation — and the owner of the conversation types
//! and all ports.
//!
//! See `ARCHITECTURE.md` §2 (crate layout) and §3 (core traits).
//!
//! This crate sees the world through traits only: it opens no sockets and no
//! files. Ports belong to the consumer — `ego-core` declares them, the
//! satellite crates implement them.

pub mod budget;
pub mod config;
pub mod cost;
pub mod event;
pub mod generator;
pub mod permit;
pub mod public;
pub mod run;
pub mod sink;
pub mod state;
pub mod store;
pub mod turn;
