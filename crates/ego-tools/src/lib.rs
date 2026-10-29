//! Builtin tools and execution backends — the OS lives here.
//!
//! Real I/O lives in this crate: filesystem tools, process spawning, sandbox backends,
//! session storage.

pub mod checkpoint;
pub mod clock;
pub mod config;
pub mod credentials;
pub mod exec;
pub mod fs;
pub mod hook;
pub mod http;
pub mod instructions;
pub mod mcp;
pub mod mcp_continuation;
mod mcp_oauth;
pub mod mcp_registry;
pub mod model_catalogue_cache;
pub mod orchestration_events;
pub mod orchestrator;
pub mod orchestrator_active;
pub mod permission_analysis_cache;
pub mod pricing;
pub mod privacy;
mod stdio;
pub mod store;
pub mod suspension;
pub mod tools;

pub mod secrets;

pub mod setup;
