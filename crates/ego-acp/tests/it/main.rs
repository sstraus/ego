//! The one integration-test binary of `ego-acp`.
//!
//! Every file in `tests/` used to be its own binary, and each one linked the
//! whole dependency graph. One binary links it once. Each former binary is a
//! module here, so `cargo nextest run -p ego-acp --test it -E 'test(/^<file>::/)'`
//! selects what `--test <file>` selected before.

#[path = "../support/mod.rs"]
mod support;

mod acp_golden;
mod auth;
mod checkpoint_projection;
mod compaction_extension;
mod confinement;
mod confinement_admission;
mod controls;
mod controls_router;
mod host_hold;
mod initialize;
mod listener_transport;
mod mcp_session_scope_critic;
mod negotiate;
mod notice_projection;
mod permission;
mod permission_readmission;
mod plan_projection;
mod prompt;
mod provider_retry_projection;
mod question;
mod question_live;
mod run_controller;
mod sandbox;
mod session_composition;
mod session_lifecycle;
mod settlement;
mod tool_projection;
mod trust;
mod uncertain_session;
