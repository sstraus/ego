//! The one integration-test binary of `ego-tools`.
//!
//! Every file in `tests/` used to be its own binary, and each one linked the
//! whole dependency graph. One binary links it once. Each former binary is a
//! module here, so `cargo nextest run -p ego-tools --test it -E 'test(/^<file>::/)'`
//! selects what `--test <file>` selected before.

#[path = "../support/admission_fixture.rs"]
mod admission_support;
#[path = "../support/private_logs.rs"]
mod private_logs;
#[cfg(target_os = "macos")]
#[path = "../support/reexec.rs"]
mod reexec;

mod adaptive_admission;
mod adaptive_critic;
mod adaptive_git_critic;
mod admission_fixture;
mod blob_store;
mod checkpoint_contract;
mod clock;
mod compaction_store;
mod compaction_target;
mod config_edit;
mod config_management;
mod config_projection;
mod config_rules;
mod config_transaction;
mod diagnostic_exec_caps;
mod editor_process;
mod evidence_contract;
mod execution_observation_contract;
mod export_publication;
mod faults;
mod fork_at_critic;
mod fork_publication;
mod fs;
mod history;
mod hook_adapters;
mod hook_lifecycle;
mod hooks_snapshot_contract;
mod hooks_store_contract;
mod ignore_critic;
mod instructions;
mod lease;
mod legacy_elicitation_fixture;
mod list_from_log_critic;
mod lsp;
mod m4_codec_snapshot;
mod managed_batch;
mod mcp_config;
mod mcp_continuation;
mod mcp_registry_journal;
mod mcp_registry_private_critic;
mod model_catalogue_cache;
mod pid_index;
mod preflight_timeout;
mod pricing_cache;
mod private_diagnostic_confinement_critic_r15;
mod private_log_owner;
mod probe_cleanup;
mod protocol_session;
mod quarantine_critic;
mod read_tool_result_contract;
mod resume;
mod run_artifact;
mod run_evidence;
mod sandbox_grants;
mod sandbox_grants_critic;
mod secret_masking;
mod session_lifecycle;
mod session_order;
mod setup_probe;
mod snapshot;
mod store;
mod store_io_line_reads;
mod suspension_ledger;
mod system_root_ceilings;
mod tools;
mod usage_manifest;
mod windows_private_paths;
