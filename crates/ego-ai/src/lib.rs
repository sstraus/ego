//! Provider layer: wire formats, provider table, usage, delivery certainty,
//! model catalogues.
//!
//! See `ARCHITECTURE.md` §3.1. Implements `ego-core`'s `Generator` port and
//! owns `Http`, credentials and the provider tables.

pub mod catalogue;
pub mod generator;
pub mod http;
pub mod oauth;
pub mod ollama;
pub mod pricing;
pub mod provider;
pub mod resolver;
pub mod wire;
pub mod xai;
