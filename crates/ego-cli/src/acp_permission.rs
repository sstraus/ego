//! User-rule writer behind the ACP permission seat.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use ego_acp::permission::PermissionRuleWriter;
use ego_core::config::rule_management::RememberPlan;
use ego_core::config::{ManagedRuleEffect, UserConfigStore, UserConfigTarget};
use ego_tools::config::OsUserConfigStore;
use ego_tools::store::OsIo;

use crate::rules::{RememberPreview, RuleApplication};

#[derive(Debug)]
pub struct CliAcpPermissionRules {
    path: PathBuf,
    target: UserConfigTarget,
}

impl CliAcpPermissionRules {
    pub fn new(path: PathBuf, target: UserConfigTarget) -> Self {
        Self { path, target }
    }
}

impl PermissionRuleWriter for CliAcpPermissionRules {
    fn remember<'a>(
        &'a self,
        effect: ManagedRuleEffect,
        plan: RememberPlan,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        let path = self.path.clone();
        let target = self.target.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let store: Arc<dyn UserConfigStore> =
                    Arc::new(OsUserConfigStore::new(Arc::new(OsIo), path));
                RuleApplication::new(store, target)
                    .confirm_remember(RememberPreview::presented(effect, plan))
                    .map(|_| ())
                    .map_err(|error| format!("{error:?}"))
            })
            .await
            .map_err(|error| format!("the ACP rule writer task failed: {error}"))?
        })
    }
}
