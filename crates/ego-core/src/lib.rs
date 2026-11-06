//! The harness: loop, reducer, permissions, sessions, subagents, skills,
//! hooks, compaction, truncation — and the owner of the conversation types
//! and all ports.
//!
//! Provider-neutral domain and policy core; external effects live in adapters.
//!
//! This crate sees the world through traits only: it opens no sockets and no
//! files. Ports belong to the consumer — `ego-core` declares them, the
//! satellite crates implement them.

pub mod budget;
pub mod ceiling;
pub mod code_intel;
pub mod command;
pub mod compact;
pub mod config;
pub mod context;
pub mod control;
pub mod cost;
pub mod credentials;
pub mod digest;
pub mod endpoint;
pub mod event;
pub mod evidence;
pub mod export;
pub mod generation_model_source;
pub mod generator;
pub mod goal;
pub mod health;
pub mod history;
pub mod hook;
pub mod mcp;
mod mcp_task;
pub mod memory;
pub mod permit;
pub mod plan;
pub mod public;
pub mod question;
pub mod recap;
pub mod run;
pub mod session;
pub mod session_lifecycle;
pub mod sink;
pub mod skill;
pub mod state;
pub mod store;
pub mod subagent;
pub mod suspension;
pub mod turn;
pub mod usage;

pub mod secrets;

pub mod setup;
