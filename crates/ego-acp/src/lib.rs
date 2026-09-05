//! ACP projection: core events → `session/update`, ACP requests → core
//! operations.
//!
//! See `ARCHITECTURE.md` §7. A thin projection over `ego-core`; it retains only
//! protocol correlation and active opaque cancellation keys while core owns
//! the durable run lifecycle.

pub mod negotiate;
pub mod permission;
pub mod project;
pub mod question;
pub mod serve;
pub mod sink;
