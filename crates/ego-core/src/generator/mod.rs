//! The `Generator` port and the provider-neutral conversation model.
//!
//! See `ARCHITECTURE.md` §2 ("ports belong to the consumer") and §3.1.
//! `ego-core` owns these types and the narrow port; `ego-ai` implements it and
//! owns HTTP, wire formats, credentials and provider tables.
//!
//! Named `generator` rather than the plan's `gen`: `gen` is a reserved keyword
//! in edition 2024.
