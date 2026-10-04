//! Round-fifteen critic cases: no public surface carries the raw cause after masker removal.

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

const MARKER: &str = "CRITIC-R15-RAW-CAUSE-5c7e";

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

/// Catches: a raw cause reaching any public surface on the SUCCESS path of a
/// per-server failure (a journal event field, status Debug, publication Debug),
/// not only `mcp_errors()` and `RefreshFailed.cause`, for several consecutive
/// failures and for a refresh that follows a failure.
#[tokio::test(flavor = "current_thread")]
async fn no_public_surface_carries_the_raw_cause_across_repeated_failures() {
    let journal = Arc::new(Journal::default());
    let registry = controller(
        vec![
            Outcome::Invalid(format!("{MARKER} token=LEAKX")),
            Outcome::Invalid(format!("password: {MARKER}\n\u{1b}[31m")),
            Outcome::Authorization,
            Outcome::Invalid(MARKER.to_owned()),
        ],
        Arc::clone(&journal),
    );
    for _ in 0..4 {
        let _publication = registry
            .reconcile(refresh())
            .await
            .expect("publishes status");
        let status = registry.status();
        let surfaces = [
            format!("{:?}", journal.events.lock().expect("events")),
            status
                .servers()
                .iter()
                .map(|server| {
                    server
                        .diagnostic()
                        .map_or(String::new(), |d| d.message().to_owned())
                })
                .collect::<Vec<_>>()
                .join(
                    "
",
                ),
            status.mcp_errors().join("\n"),
            format!("{:?}", journal.replay().expect("replay")),
        ];
        for surface in surfaces {
            assert!(!surface.contains(MARKER), "raw cause on a public surface");
            assert!(!surface.contains("LEAKX"), "raw cause fragment published");
        }
    }
    assert_eq!(journal.private.lock().expect("private").len(), 3);
}

/// Catches: the cause stored after truncation/translation rather than as
/// received, i.e. the private port seeing a transformed (masked/trimmed) cause.
#[tokio::test(flavor = "current_thread")]
async fn private_port_receives_the_cause_byte_for_byte() {
    let raw = format!(
        "\u{0}\n{MARKER}\u{200d} token=\"a b\" \u{1f600}{}",
        "x".repeat(5000)
    );
    let journal = Arc::new(Journal::default());
    let registry = controller(vec![Outcome::Invalid(raw.clone())], Arc::clone(&journal));
    registry
        .reconcile(refresh())
        .await
        .expect("publishes status");
    assert_eq!(
        *journal.private.lock().expect("private"),
        [("alpha".to_owned(), raw)]
    );
}
