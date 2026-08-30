//! RED contract for regression case: every CLI composition root shares evidence admission.
//!
//! The composition signatures below deliberately name the production seams.
//! Once implemented, the companion effect fixtures will drive these same
//! surfaces with a temporary JSONL store and the existing provider/protocol
//! adapters; no test-local host or admission adapter is permitted.

use std::ops::AsyncFn;
use std::sync::Arc;

use ego_cli::cli::{self, AcpArgs, Args, McpServerArgs};
use ego_cli::output::Stdout;
use ego_cli::run::Ended;
use ego_core::evidence::EvidenceAdmissionService;
use ego_core::run::RunTermination;
use ego_core::turn::CancelToken;
use ego_mcp::McpRegistryController;

fn assert_direct_surface<F>(_surface: F)
where
    F: for<'a> AsyncFn(
        Args,
        &'a Stdout,
        CancelToken,
        Arc<McpRegistryController>,
        Arc<EvidenceAdmissionService>,
    ) -> Ended,
{
}

fn assert_interactive_surface<F>(_surface: F)
where
    F: for<'a> AsyncFn(
        Args,
        &'a Stdout,
        CancelToken,
        Arc<McpRegistryController>,
        Arc<EvidenceAdmissionService>,
    ) -> Ended,
{
}

fn assert_acp_surface<F>(_surface: F)
where
    F: for<'a> AsyncFn(&'a AcpArgs, Arc<EvidenceAdmissionService>) -> RunTermination,
{
}

fn assert_mcp_surface<F>(_surface: F)
where
    F: for<'a> AsyncFn(&'a McpServerArgs, Arc<EvidenceAdmissionService>) -> RunTermination,
{
}

#[test]
fn every_composition_root_requires_the_shared_admission_service() {
    assert_direct_surface(ego_cli::run::run_with_mcp_registry);
    assert_interactive_surface(ego_cli::run::interactive_with_mcp_registry);
    assert_acp_surface(ego_cli::acp::serve);
    assert_mcp_surface(ego_cli::mcp_server::serve);

    // Keep the real parser in this contract: protocol composition starts only
    // after the existing CLI surface has selected the corresponding command.
    assert!(matches!(
        cli::parse(vec![
            "ego".to_owned(),
            "run".to_owned(),
            "prompt".to_owned()
        ]),
        Ok(cli::Invocation::Run(_))
    ));
}
