//! `ego`, the binary — argv, stdout, the process exit.
//!
//! `ARCHITECTURE.md` §3: this crate owns the command line, the two output
//! streams, signal handling and the process's exit status. It owns no
//! accounting, no projection and no exit-code table: each of those is a value
//! it reads from `ego-core`, which is what keeps an ACP host and this binary
//! saying the same thing about the same run.
//!
//! A library beside the binary so the parse and the assembly can be tested as
//! functions, and the *process* — exit codes, stdout, signals — can be tested
//! from outside by running it. Both are needed: `crates/ego-cli/tests/` runs
//! the built binary precisely because an exit code is not observable from
//! inside the process that produces it.

#[cfg(test)]
#[path = "../../ego-ai/tests/support/mod.rs"]
mod ai_test_support;

/// The product name shown to a person. Machine-facing names remain `ego`.
pub(crate) const DISPLAY_NAME: &str = "εgo";

pub mod a2a;
pub mod acp;
pub mod acp_confinement;
pub mod acp_controls;
pub mod acp_permission;
pub mod acp_sandbox_grants;
pub mod acp_trust;
pub(crate) mod agent_file;
pub(crate) mod attachment;
pub mod auth;
mod auth_usage;
pub mod budget;
pub mod build_info;
pub mod build_recipes;
pub(crate) mod capability_roots;
pub mod catalogue;
pub mod cli;
pub(crate) mod command;
pub mod config;
pub(crate) mod confinement_diagnostics;
pub mod diagnostics;
pub mod doctor;
pub mod exclusive_env;
pub mod facts;
pub mod hook;
pub mod hooks;
pub mod host_ports;
pub mod instruction_init;
pub mod mcp;
pub(crate) mod mcp_attention;
pub mod mcp_server;
pub(crate) mod memory;
pub mod metadata;
pub mod model;
pub mod model_picker;
pub mod models;
pub(crate) mod named_agents;
pub(crate) mod onboarding;
pub mod output;
pub(crate) mod output_style;
pub mod perimeter;
pub mod prompt;
mod protocol_filter;
pub mod provider_http;
pub mod queue;
pub mod quota;
pub mod reference;
pub mod render;
pub mod repl;
pub mod rewind;
pub mod rules;
pub mod run;
pub mod schema;
pub mod session;
pub mod setup;
pub(crate) mod setup_config;
pub(crate) mod setup_terminal;
#[allow(dead_code)] // Connected to the turn service by M4 S6 after admission is green.
pub(crate) mod skill;
pub mod status_provider;
pub mod trust;
pub mod usage;
pub mod worktree;
pub(crate) mod yaml_envelope;

// Hosts the same child envelope as the production offline probe. The script
// lets the cancellation regression hold a real probe until cancellation.
#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
#[test]
#[ignore = "offline probe child host"]
fn __ego_internal_network_probe_v1() {
    if std::path::Path::new("readmission-probe.sh").exists() {
        assert!(
            std::process::Command::new("/bin/sh")
                .arg("readmission-probe.sh")
                .status()
                .expect("scripted probe")
                .success()
        );
    }
    assert!(ego_tools::exec::dispatch_network_probe(std::env::args_os()).expect("probe envelope"));
}

pub mod checkpoint;

// The library test host must dispatch Landlock probes like the CLI binary.
#[cfg(all(test, target_os = "linux"))]
#[test]
#[ignore = "hosts the Landlock helper; not a test of its own"]
fn __ego_internal_landlock_exec_v1() {
    let refused = ego_tools::exec::dispatch_landlock_helper(std::env::args_os())
        .expect("libtest did not recognise the Landlock helper envelope");
    panic!("the Landlock helper refused execution: {refused:?}");
}
