//! The one integration-test binary of `ego-mcp`.
//!
//! Every file in `tests/` used to be its own binary, and each one linked the
//! whole dependency graph. One binary links it once. Each former binary is a
//! module here, so `cargo nextest run -p ego-mcp --test it -E 'test(registry::)'`
//! selects what `--test registry` selected before.

#[allow(dead_code)]
#[path = "../support/run_controller.rs"]
mod run_fixture;

mod compaction;
mod hook_adapter;
mod hook_lifecycle;
mod legacy_sse;
mod mcp_dynamic_mrtr;
mod mcp_execution_wiring;
mod mcp_server;
mod mcp_tasks_client;
mod oauth;
mod oauth_local_server;
mod protocol;
mod registry;
mod registry_critic_round_fifteen;
mod registry_critic_round_ten;
mod runtime;
mod state;
mod surfaces;
