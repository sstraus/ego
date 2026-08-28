//! Builtin tools and execution backends — the OS lives here.
//!
//! See `ARCHITECTURE.md` §3.2 (tools), §3.3 (environment seam) and §3.4
//! (sandbox enforcement). Real I/O lives in this crate: filesystem tools,
//! process spawning, sandbox backends, session storage.

pub mod clock;
pub mod config;
pub mod credentials;
pub mod exec;
pub mod fs;
pub mod http;
pub mod instructions;
pub mod mcp;
pub mod mcp_continuation;
pub mod mcp_registry;
pub mod model_catalogue_cache;
pub mod pricing;
mod stdio;
pub mod store;
pub mod tools;
