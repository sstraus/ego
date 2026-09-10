//! `ego a2a`: production composition for the inbound A2A adapter.
//!
//! Deliberately much smaller than `acp.rs`, and it says something true that it
//! is: this milestone serves discovery and refuses every method, so there is no
//! run to compose. Building the controller, the lifecycle, the MCP registry and
//! the rest here now would be nine collaborators held and never called, and a
//! reader would reasonably conclude the methods work. They arrive with the
//! routes that need them.

use std::sync::Arc;

use ego_a2a::auth::Credential;
use ego_core::endpoint::{Admitted, Refusal};
use ego_core::permit::{CanonPath, RunConfinement};
use ego_core::run::RunTermination;
use ego_core::turn::CancelToken;
use ego_tools::exec::{ExecLimits, RunEffectPorts};

use crate::cli::A2aArgs;
use crate::confinement_diagnostics::Reporter;
use crate::{output::Stderr, run};

/// Serve until the process is stopped.
///
/// Every refusal happens before a socket exists: an address reachable from
/// outside without the operator having said so, and — for an endpoint the
/// transport cannot authenticate — a listener with no credential to check
/// callers against. A local endpoint reaches neither: the operating system
/// answers for its peers, so there is no credential to be missing.
pub async fn serve(args: &A2aArgs) -> RunTermination {
    let admitted = match admit_endpoint(args) {
        Ok(admitted) => admitted,
        Err(refusal) => return refused(refusal.to_string()),
    };
    let credential = if ego_listen::credential_required(&admitted) {
        match Credential::from_env(|name| std::env::var(name).ok()) {
            Ok(credential) => Some(credential),
            Err(refusal) => return refused(refusal.to_string()),
        }
    } else {
        None
    };

    let root = match run::workspace(args.directory.as_ref()) {
        Ok(root) => root,
        Err(error) => {
            return RunTermination::UsageError {
                message: error.to_string(),
            };
        }
    };
    let policy = RunConfinement::new(
        args.sandbox.unwrap_or_default(),
        false,
        CanonPath::new(root.as_path()),
    )
    .expect("the resolved A2A process root is absolute");
    let reporter = Reporter::new(Arc::new(Stderr));
    reporter.report(
        RunEffectPorts::diagnostic_capabilities(&policy, CancelToken::new(), ExecLimits::default())
            .await,
    );

    // Never resolves: the listener runs until the process is signalled, which
    // is the same lifetime `ego acp` has when its host keeps the pipe open.
    match ego_a2a::serve::serve(&admitted, credential, std::future::pending()).await {
        Ok(()) => RunTermination::Done,
        Err(error) => refused(format!("the A2A listener could not serve: {error}")),
    }
}

/// Read `--listen` as the endpoint kind it names.
///
/// Anything that parses as an address is one, and everything else is a local
/// endpoint. The rule is deliberately this blunt: a socket path does not parse
/// as a `SocketAddr`, so there is no spelling both readings accept, and a
/// scheme prefix (`unix:`, `tcp:`) would be a second vocabulary for a question
/// the value already answers.
fn admit_endpoint(args: &A2aArgs) -> Result<Admitted, Refusal> {
    crate::cli::admit_endpoint(&args.listen, args.allow_public_bind)
}

fn refused(message: String) -> RunTermination {
    RunTermination::Failed { message }
}

/// Named so the startup refusals are reachable from a test without opening a
/// socket, which is the whole reason they are separated from `serve`.
#[cfg(test)]
mod tests {
    use ego_a2a::auth::CredentialRefusal;
    use ego_core::endpoint::Authentication;

    use super::*;

    fn args(listen: &str, allow_public_bind: bool) -> A2aArgs {
        A2aArgs {
            session_dir: None,
            directory: None,
            listen: listen.to_string(),
            allow_public_bind,
            sandbox: None,
        }
    }

    #[test]
    fn a_public_address_is_refused_until_the_operator_asks_for_it() {
        let refusal = admit_endpoint(&args("0.0.0.0:8080", false))
            .expect_err("a public address was admitted silently");
        assert!(refusal.to_string().contains("--allow-public-bind"));

        let admitted =
            admit_endpoint(&args("0.0.0.0:8080", true)).expect("an explicit request is honoured");
        assert_eq!(admitted.to_string(), "0.0.0.0:8080");
    }

    #[test]
    fn loopback_needs_no_further_permission() {
        assert!(admit_endpoint(&args("127.0.0.1:0", false)).is_ok());
    }

    /// The one ambiguity worth pinning: a path and an address are told apart by
    /// whether the value parses, and nothing else.
    #[test]
    fn a_value_that_is_not_an_address_is_a_local_endpoint() {
        let admitted = admit_endpoint(&args("/run/user/1000/ego.sock", false))
            .expect("a socket path is a local endpoint");
        assert_eq!(
            admitted,
            Admitted::Local {
                name: "/run/user/1000/ego.sock".into()
            }
        );
        assert_eq!(admitted.authentication(), Authentication::PeerIdentity);
        assert!(
            !ego_listen::credential_required(&admitted),
            "a local endpoint demanded a credential"
        );
    }

    /// The startup refusal an operator is most likely to hit on an endpoint
    /// that needs one, and the one that must not be answerable by serving
    /// anonymously.
    #[test]
    fn a_missing_token_is_a_refusal_that_names_the_variable() {
        let admitted = admit_endpoint(&args("127.0.0.1:0", false)).expect("loopback is admitted");
        assert!(ego_listen::credential_required(&admitted));
        let refusal = Credential::from_env(|_| None).expect_err("no token was demanded");
        assert_eq!(refusal, CredentialRefusal::Unset);
        assert!(refusal.to_string().contains("EGO_A2A_TOKEN"));
    }
}
