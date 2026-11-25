//! Destination authority carried from operator consent through readmission.

use super::*;

pub struct ProviderSwitch {
    pub(super) source: SessionId,
    pub(super) id: SessionId,
    pub(super) request: ModelSwitchRequest,
    pub(super) request_id: RequestId,
    pub(super) destination: ProviderDestination,
    pub(super) fork: Option<ego_core::session::PreparedFork>,
}

pub(super) struct ProviderDestination {
    pub(super) connection: Option<String>,
    account: Option<ego_core::config::ConnectionConfig>,
    instance: Option<ego_core::config::ProviderInstance>,
    auth: Option<ego_core::config::AuthProviderLayer>,
}

impl ProviderDestination {
    pub(super) fn new(
        effective: &Effective,
        route: &str,
        connection: Option<String>,
        saved_account: Option<ego_core::config::ConnectionConfig>,
    ) -> Result<Self, String> {
        let account = if let Some(account) = saved_account {
            Some(account)
        } else {
            connection
                .as_ref()
                .map(|id| {
                    effective
                        .connections
                        .get(id)
                        .cloned()
                        .ok_or("the destination account is no longer available")
                })
                .transpose()?
        };
        let instance = effective
            .providers
            .instances
            .iter()
            .find(|(id, _)| id.as_str() == route)
            .map(|(_, entry)| entry.value.clone());
        Ok(Self {
            connection,
            account,
            instance,
            auth: effective.auth.get(route).cloned(),
        })
    }

    pub(super) fn description(&self) -> String {
        let account = match (&self.connection, &self.account) {
            (Some(id), Some(account)) => format!("account {} ({id})", account.name),
            _ if self.instance.is_some() => "no account".into(),
            _ => "the route's default credentials".into(),
        };
        match &self.instance {
            Some(instance) => format!("{account} at {}", instance.base_url.as_url()),
            None => account,
        }
    }

    pub(super) fn matches_admitted_account(&self, selection: &model::GenerationSelection) -> bool {
        // Configured instances resolve their credential reference directly and
        // intentionally leave the mutable built-in route pin empty.
        self.instance.is_some() || selection.connection() == self.connection.as_deref()
    }

    pub(super) fn verify(
        &self,
        effective: &Effective,
        target: &ModelRef,
    ) -> Result<(), RunTermination> {
        let route = target
            .0
            .split_once('/')
            .map_or(target.0.as_str(), |(route, _)| route);
        if let Ok(current) = Self::new(effective, route, self.connection.clone(), None)
            && current.account == self.account
            && current.instance == self.instance
            && current.auth == self.auth
        {
            return Ok(());
        }
        Err(RunTermination::UsageError {
            message: "the destination endpoint or credential configuration changed; reopen /model and confirm it again".into(),
        })
    }
}
