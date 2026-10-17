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
pub mod budget;
pub mod build_info;
pub(crate) mod capability_roots;
pub mod catalogue;
pub mod cli;
pub(crate) mod command;
pub mod config;
pub(crate) mod confinement_diagnostics;
pub mod diagnostics;
pub mod doctor;
pub mod facts;
pub mod hook;
pub mod hooks;
pub mod host_ports;
pub mod mcp;
pub mod mcp_server;
pub mod memory;
pub mod metadata;
pub mod model;
pub mod model_picker;
pub mod models;
pub(crate) mod named_agents;
pub(crate) mod onboarding;
pub(crate) mod orchestration_observer;
pub mod orchestrator;
pub mod output;
pub mod perimeter;
pub mod prompt;
pub mod provider_http;
pub mod reference;
pub mod render;
pub mod repl;
pub mod rules;
pub mod run;
pub mod schema;
pub mod session;
pub mod setup;
mod setup_proposals;
#[allow(dead_code)] // Connected to the turn service by M4 S6 after admission is green.
pub(crate) mod skill;
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
