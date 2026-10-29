//! The `Generator` port and the provider-neutral conversation model.
//!
//! `ego-core` owns these types and the narrow port; `ego-ai` implements it and owns HTTP,
//! wire formats, credentials and provider tables.
//!
//! Named `generator` rather than the plan's `gen`: `gen` is a reserved keyword
//! in edition 2024.

pub mod attempt;
pub mod audio;
pub mod capabilities;
pub mod fake;
pub mod history;
pub mod port;
pub mod types;
