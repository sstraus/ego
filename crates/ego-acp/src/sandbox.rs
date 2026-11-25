//! ACP seat for sandbox denials: the connected host is the human who may open
//! one more path to the commands of its own session.
//!
//! The permission seat in [`crate::permission`] answers "may this call run".
//! This one answers a different question that only exists after a command ran:
//! the sandbox refused a path, and only a person can say it may be reached. A
//! session answer is journaled by the executing turn before it is applied;
//! same-ID resume and compaction restore that receipt. An "always" answer
//! also reaches [`SandboxGrantStore`] for future sessions in the workspace.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use agent_client_protocol::{Client, ConnectionTo};
use agent_client_protocol_schema::v1;
use ego_core::event::id::ItemId;
use ego_core::permit::{RefusalAnswer, SandboxAccess, SandboxGrant, SandboxGrantPort};
use futures::future::BoxFuture;

/// Grants a person asked to keep for a workspace, owned by the binary.
pub trait SandboxGrantStore: std::fmt::Debug + Send + Sync {
    fn load(&self, workspace: &Path) -> Vec<SandboxGrant>;

    fn remember<'a>(
        &'a self,
        workspace: &'a Path,
        grant: &'a SandboxGrant,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
}

/// The session's in-memory grants, shared by every prompt of that session.
pub type SessionSandboxGrants = Arc<Mutex<Vec<SandboxGrant>>>;

#[derive(Clone)]
pub struct AcpSandboxGrants {
    session: v1::SessionId,
    connection: ConnectionTo<Client>,
    workspace: PathBuf,
    store: Option<Arc<dyn SandboxGrantStore>>,
    session_grants: SessionSandboxGrants,
    rules: Option<Arc<dyn crate::permission::PermissionRuleWriter>>,
}

impl AcpSandboxGrants {
    /// Wire IDs of the three answers.
    pub const ALLOW_SESSION: &'static str = "sandbox-session";
    pub const ALLOW_ALWAYS: &'static str = "sandbox-always";
    pub const DENY: &'static str = "sandbox-deny";
    pub const DENY_ALWAYS: &'static str = "sandbox-deny-always";

    pub fn new(
        session: v1::SessionId,
        connection: ConnectionTo<Client>,
        workspace: PathBuf,
        store: Option<Arc<dyn SandboxGrantStore>>,
        session_grants: SessionSandboxGrants,
    ) -> Self {
        Self {
            session,
            connection,
            workspace,
            store,
            session_grants,
            rules: None,
        }
    }

    pub fn with_rules(
        mut self,
        rules: Option<Arc<dyn crate::permission::PermissionRuleWriter>>,
    ) -> Self {
        self.rules = rules;
        self
    }

    /// The question, addressed to the host.
    ///
    /// There is no tool-call item to hang it on: the command is already
    /// finished, and its streamed call id is not reachable from the process
    /// executor. The prompt therefore carries a call id of its own and says
    /// what it is about in its title.
    pub fn request(
        session: &v1::SessionId,
        grant: &SandboxGrant,
        command: &str,
        rememberable: bool,
    ) -> v1::RequestPermissionRequest {
        let action = match grant.access {
            SandboxAccess::Read => "read",
            SandboxAccess::Write => "write",
            SandboxAccess::Exec => "run programs from",
        };
        let title = format!(
            "The sandbox refused a command access to {path}.\nAllow it to {action} {path}? \
             No command will be retried; prior effects may already have occurred:\n{command}",
            path = grant.path
        );
        let tool_call = v1::ToolCallUpdate::new(
            v1::ToolCallId::new(format!("sandbox-{}", ItemId::new())),
            v1::ToolCallUpdateFields::new().title(title),
        );
        let mut options = vec![v1::PermissionOption::new(
            v1::PermissionOptionId::new(Self::ALLOW_SESSION),
            format!("Allow {grant} for this session"),
            v1::PermissionOptionKind::AllowAlways,
        )];
        if rememberable && grant.access != SandboxAccess::Exec {
            options.push(v1::PermissionOption::new(
                v1::PermissionOptionId::new(Self::ALLOW_ALWAYS),
                format!("Allow {grant} always as a root (confirmation follows)"),
                v1::PermissionOptionKind::AllowAlways,
            ));
        }
        if rememberable
            && ego_core::config::rule_management::RememberPlan::for_sandbox_path(grant).is_some()
        {
            options.push(v1::PermissionOption::new(
                v1::PermissionOptionId::new(Self::DENY_ALWAYS),
                "Keep blocked (remember rule)",
                v1::PermissionOptionKind::RejectAlways,
            ));
        }
        options.push(v1::PermissionOption::new(
            v1::PermissionOptionId::new(Self::DENY),
            "Deny",
            v1::PermissionOptionKind::RejectOnce,
        ));
        v1::RequestPermissionRequest::new(session.clone(), tool_call, options)
    }

    fn keep_for_session(&self, grant: &SandboxGrant) {
        let mut grants = self
            .session_grants
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !grants.contains(grant) {
            grants.push(grant.clone());
        }
    }

    async fn decide(&self, grant: &SandboxGrant, command: &str) -> RefusalAnswer {
        if let Some(rules) = &self.rules {
            match rules.denies_sandbox(grant, &self.workspace) {
                Ok(true) => return RefusalAnswer::Deny,
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(%error, "ACP sandbox deny rules could not be read");
                    return RefusalAnswer::Deny;
                }
            }
        }
        let mut wire = Self::request(&self.session, grant, command, self.store.is_some());
        if self.rules.is_none() {
            wire.options
                .retain(|option| option.option_id.0.as_ref() != Self::DENY_ALWAYS);
        }
        let response = match self.connection.send_request(wire).block_task().await {
            Ok(response) => response,
            Err(error) => {
                // A transport failure leaves the sandbox as it was.
                tracing::warn!(%error, "ACP sandbox question failed");
                return RefusalAnswer::Deny;
            }
        };
        let v1::RequestPermissionOutcome::Selected(selected) = &response.outcome else {
            return RefusalAnswer::Deny;
        };
        match selected.option_id.0.as_ref() {
            Self::ALLOW_SESSION => {
                self.keep_for_session(grant);
                RefusalAnswer::Session
            }
            Self::DENY_ALWAYS => {
                if let (Some(rules), Some(plan)) = (
                    &self.rules,
                    ego_core::config::rule_management::RememberPlan::for_sandbox_path(grant),
                ) && let Err(error) = rules
                    .remember(ego_core::config::ManagedRuleEffect::Deny, plan)
                    .await
                {
                    tracing::warn!(%error, "ACP sandbox deny could not be stored");
                }
                RefusalAnswer::Deny
            }
            Self::ALLOW_ALWAYS if self.store.is_some() && grant.access != SandboxAccess::Exec => {
                RefusalAnswer::Always
            }
            _ => RefusalAnswer::Deny,
        }
    }
}

impl SandboxGrantPort for AcpSandboxGrants {
    fn granted(&self) -> Vec<SandboxGrant> {
        let mut granted = self
            .store
            .as_ref()
            .map(|store| store.load(&self.workspace))
            .unwrap_or_default();
        let session = self
            .session_grants
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for grant in session.iter() {
            if !granted.contains(grant) {
                granted.push(grant.clone());
            }
        }
        granted
    }

    fn ask<'a>(
        &'a self,
        request: &'a SandboxGrant,
        command: &'a str,
    ) -> BoxFuture<'a, RefusalAnswer> {
        Box::pin(self.decide(request, command))
    }
}
