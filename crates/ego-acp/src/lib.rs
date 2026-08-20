//! ACP projection: core events → `session/update`, ACP requests → core
//! operations.
//!
//! See `ARCHITECTURE.md` §7. A thin projection over `ego-core`; it holds no
//! state of its own.

pub mod project;
