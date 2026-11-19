//! ACP projection: core events → `session/update`, ACP requests → core
//! operations.
//!
//! A thin projection over `ego-core`; it retains only protocol correlation and active
//! opaque cancellation keys while core owns the durable run lifecycle.

pub mod auth;
pub mod controls;
pub mod negotiate;
pub mod permission;
pub mod project;
pub mod question;
pub mod sandbox;
pub mod serve;
pub mod sink;
