//! Round-ten critic cases: the private diagnostic port as seen by the registry.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use ego_core::config::{ConfigLayer, McpServerConfig, ResolvedMcpEntry, Source, Trust, resolve};
use ego_core::turn::{
    McpRegistryEvent, McpRegistryJournal, McpRegistryJournalError, McpRegistryJournalErrorKind,
    McpRegistryJournalOperation,
};
use ego_mcp::{
    McpAdmittedCandidate, McpCandidateAdmission, McpCandidateError, McpCandidateRequest,
    McpRegistryCandidate, McpRegistryController, McpRegistryRefresh, McpServerAdmission,
    McpTransportMaterial, RegistryScope, RegistryTrigger,
};

const MARKER: &str = "CRITIC-R10-RAW-CAUSE-9d41";

#[derive(Default)]
struct Journal {
    events: Mutex<Vec<McpRegistryEvent>>,
    private: Mutex<Vec<(String, String)>>,
    fail_private: bool,
}

impl McpRegistryJournal for Journal {
    fn append(&self, event: McpRegistryEvent) -> Result<(), McpRegistryJournalError> {
        self.events.lock().expect("events").push(event);
        Ok(())
    }
    fn record_private_diagnostic(
        &self,
        server: &str,
        cause: &str,
    ) -> Result<(), McpRegistryJournalError> {
        if self.fail_private {
            return Err(McpRegistryJournalError::new(
                McpRegistryJournalOperation::PrivateDiagnostic,
                McpRegistryJournalErrorKind::Unavailable,
            ));
        }
        self.private
            .lock()
            .expect("private")
            .push((server.to_owned(), cause.to_owned()));
        Ok(())
    }
    fn replay(&self) -> Result<Vec<McpRegistryEvent>, McpRegistryJournalError> {
        Ok(self.events.lock().expect("events").clone())
    }
    fn begin_refresh(
        &self,
        request_id: String,
        trigger: String,
        server_scope: Option<String>,
    ) -> Result<u64, McpRegistryJournalError> {
        let mut events = self.events.lock().expect("events");
        let epoch = events.len() as u64 + 1;
        events.push(McpRegistryEvent::RefreshRequested {
            request_id,
            trigger,
            server_scope,
            config_revision: epoch,
        });
        Ok(epoch)
    }
}

enum Outcome {
    Invalid(String),
    Authorization,
}

struct Admission {
    queue: Mutex<VecDeque<Outcome>>,
}

impl McpCandidateAdmission for Admission {
    fn admit(
        &self,
        _request: McpCandidateRequest,
    ) -> ego_mcp::McpFuture<'static, Result<McpAdmittedCandidate, McpCandidateError>> {
        let outcome = self
            .queue
            .lock()
            .expect("queue")
            .pop_front()
            .expect("outcome");
        Box::pin(async move {
            Err(match outcome {
                Outcome::Invalid(cause) => McpCandidateError::Invalid { cause },
                Outcome::Authorization => McpCandidateError::AuthorizationRequired {
                    issuer: "https://issuer.example".to_owned(),
                    protected_resource: "https://mcp.example".to_owned(),
                    scopes: vec!["tools".to_owned()],
                },
            })
        })
    }
}

fn refresh() -> McpRegistryRefresh {
    let layer = ConfigLayer::parse(
        Source::User,
        "[mcp.servers.alpha]\ntransport = 'stdio'\ncommand = 'mcp-alpha'\n",
    )
    .expect("layer");
    let config = resolve(&[layer], Trust::Trusted, None)
        .expect("resolved")
        .mcp;
    let candidates = config
        .entries()
        .iter()
        .filter_map(|(name, entry)| match entry {
            ResolvedMcpEntry::Configured { server, provenance } => {
                let McpServerConfig::Stdio(_) = server else {
                    unreachable!()
                };
                Some(McpRegistryCandidate::ready(
                    Arc::new(
                        McpServerAdmission::new(
                            name.clone(),
                            server.clone(),
                            McpTransportMaterial::stdio(Vec::new()),
                        )
                        .expect("admission"),
                    ),
                    provenance.clone(),
                ))
            }
            ResolvedMcpEntry::Tombstone { .. } => None,
        })
        .collect::<Vec<_>>();
    McpRegistryRefresh::new(candidates, RegistryTrigger::Add, RegistryScope::All).expect("refresh")
}

fn controller(outcomes: Vec<Outcome>, journal: Arc<Journal>) -> Arc<McpRegistryController> {
    McpRegistryController::new(
        journal as Arc<dyn McpRegistryJournal>,
        Arc::new(Admission {
            queue: Mutex::new(outcomes.into()),
        }) as Arc<dyn McpCandidateAdmission>,
    )
    .expect("controller")
}

/// Catches: a private-log failure that is swallowed (fail-open publish) or whose
/// error text, Display or Debug, carries the backend cause.
#[tokio::test(flavor = "current_thread")]
async fn private_log_failure_stops_reconcile_and_hides_the_cause() {
    let journal = Arc::new(Journal {
        fail_private: true,
        ..Journal::default()
    });
    let registry = controller(
        vec![Outcome::Invalid(MARKER.to_owned())],
        Arc::clone(&journal),
    );
    let before = registry.publication().generation().to_owned();
    let error = registry
        .reconcile(refresh())
        .await
        .expect_err("fail closed");
    assert!(!format!("{error}").contains(MARKER));
    assert!(!format!("{error:?}").contains(MARKER));
    assert_eq!(registry.publication().generation(), before);
    assert!(
        registry
            .status()
            .mcp_errors()
            .iter()
            .all(|e| !e.contains(MARKER))
    );
    let events = format!("{:?}", journal.events.lock().expect("events"));
    assert!(!events.contains(MARKER), "{events}");
}

/// Catches: the port receiving the cause twice, a record for an authorization
/// failure (whose fields are not backend text), or a record under the wrong name.
#[tokio::test(flavor = "current_thread")]
async fn private_port_gets_exactly_one_record_per_invalid_failure_and_none_for_authorization() {
    let journal = Arc::new(Journal::default());
    let registry = controller(
        vec![Outcome::Invalid(MARKER.to_owned()), Outcome::Authorization],
        Arc::clone(&journal),
    );
    registry
        .reconcile(refresh())
        .await
        .expect("invalid publishes status");
    assert_eq!(
        *journal.private.lock().expect("private"),
        [("alpha".to_owned(), MARKER.to_owned())]
    );
    registry
        .reconcile(refresh())
        .await
        .expect("authorization publishes status");
    assert_eq!(journal.private.lock().expect("private").len(), 1);
}
