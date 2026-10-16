//! Provider layer: wire formats, provider table, usage, delivery certainty,
//! model catalogues.
//!
//! See `ARCHITECTURE.md` §3.1. Implements `ego-core`'s `Generator` port and
//! owns `Http`, credentials and the provider tables.

pub mod catalogue;
mod codex_session;
pub mod conformance;
pub mod generator;
pub mod http;
pub mod kimi;
pub mod oauth;
pub mod ollama;
mod openrouter;
pub mod pricing;
pub mod provider;
pub mod resolver;
pub mod service_tier;
mod tool_names;
pub mod wire;

mod subscription_pricing;
