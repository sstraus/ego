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

use crate::repl::policy_controls::{
    FrozenPolicyResolver, GenerationPolicySelection, finish_rule_change,
};
use crate::rules::{RememberPreview, RuleApplication};

pub struct CliAcpPermissionRules {
    path: PathBuf,
    target: UserConfigTarget,
    live_policy: Option<(GenerationPolicySelection, Arc<dyn FrozenPolicyResolver>)>,
}

impl std::fmt::Debug for CliAcpPermissionRules {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CliAcpPermissionRules")
            .field("path", &self.path)
            .field("target", &self.target)
            .field("live_policy", &self.live_policy.is_some())
            .finish()
    }
}

impl CliAcpPermissionRules {
    pub fn new(path: PathBuf, target: UserConfigTarget) -> Self {
        Self {
            path,
            target,
            live_policy: None,
        }
    }

    /// Bind root-seat publications to the same session's next policy capture.
    pub(crate) fn with_live_policy(
        mut self,
        selection: GenerationPolicySelection,
        resolver: Arc<dyn FrozenPolicyResolver>,
    ) -> Self {
        self.live_policy = Some((selection, resolver));
        self
    }
}

impl PermissionRuleWriter for CliAcpPermissionRules {
    fn denies_sandbox(
        &self,
        grant: &ego_core::permit::SandboxGrant,
        workspace: &std::path::Path,
    ) -> Result<bool, String> {
        let store = OsUserConfigStore::new(Arc::new(OsIo), self.path.clone());
        let mut snapshot = store
            .rule_snapshot(self.target.clone())
            .map_err(|error| error.to_string())?;
        if !snapshot.permissions_present && self.target != UserConfigTarget::Base {
            snapshot = store
                .rule_snapshot(UserConfigTarget::Base)
                .map_err(|error| error.to_string())?;
        }
        let rules = ego_core::config::Ruleset::from_layer(
            ego_core::config::PermCfg {
                judge: None,
                allow: snapshot.allow,
                deny: snapshot.deny,
            },
            ego_core::config::Source::User,
        );
        Ok(rules.denies_sandbox(grant, workspace))
    }

    fn remember<'a>(
        &'a self,
        effect: ManagedRuleEffect,
        plan: RememberPlan,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        let path = self.path.clone();
        let target = self.target.clone();
        let live_policy = self.live_policy.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let store: Arc<dyn UserConfigStore> =
                    Arc::new(OsUserConfigStore::new(Arc::new(OsIo), path));
                let _publication = live_policy
                    .as_ref()
                    .map(|(selection, _)| selection.lock_rule_publication())
                    .transpose()
                    .map_err(|error| error.to_string())?;
                let change = RuleApplication::new(store, target)
                    .confirm_remember(RememberPreview::presented(effect, plan))
                    .map_err(|error| format!("{error:?}"))?;
                if let Some((selection, resolver)) = live_policy.as_ref() {
                    finish_rule_change(selection, resolver.as_ref(), change)
                        .map_err(|error| error.to_string())?;
                }
                Ok(())
            })
            .await
            .map_err(|error| format!("the ACP rule writer task failed: {error}"))?
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ego_core::permit::{CanonPath, SandboxAccess, SandboxGrant};

    // Catches: a later Base edit restores a stale selected-profile layer,
    // dropping a published deny or admitting an allow shadowed by that profile.
    #[tokio::test]
    async fn profile_publications_survive_later_base_edits_without_authority_widening() {
        use crate::repl::policy_controls::{
            PolicyApplicationOwner, PolicyControlError, PolicyControlRequest, PreflightPort,
        };
        use ego_core::config::{
            ConfigLayer, Mode, PermCfg, RuleMutation, Ruleset, Source, Sourced, Trust,
        };
        use ego_core::permit::rule::Rule;
        struct Preflight;
        impl PreflightPort for Preflight {
            fn validate_auto(&self) -> Result<(), PolicyControlError> {
                Ok(())
            }
        }
        for existing_profile in [true, false] {
            let home = tempfile::tempdir().unwrap();
            let path = home.path().join("config.toml");
            let mut fixture = "[permissions]\nallow = [\"read(/outside/base)\"]\n".to_owned();
            if existing_profile {
                fixture
                    .push_str("[profile.work.permissions]\nallow = [\"read(/outside/profile)\"]\n");
            }
            std::fs::write(&path, &fixture).unwrap();
            let layers = vec![Sourced {
                source: Source::User,
                value: toml::from_str::<ConfigLayer>(&fixture).unwrap(),
            }];
            let initial_rule = if existing_profile {
                "read(/outside/profile)"
            } else {
                "read(/outside/base)"
            };
            let selection = GenerationPolicySelection::new(
                Mode::Default,
                Ruleset::from_layer(
                    PermCfg {
                        judge: None,
                        allow: vec![Rule::load(initial_rule).unwrap()],
                        deny: vec![],
                    },
                    Source::User,
                ),
            );
            let admitted = selection.snapshot().unwrap();
            let resolver = || {
                Arc::new(crate::run::FrozenRuleResolver::new(
                    layers.clone(),
                    Trust::Untrusted,
                    Some(("work".into(), Source::User)),
                    selection.clone(),
                ))
            };
            let profile =
                UserConfigTarget::Profile(ego_core::config::ProfileName::parse("work").unwrap());
            let writer = CliAcpPermissionRules::new(path.clone(), profile.clone())
                .with_live_policy(selection.clone(), resolver());
            let grant =
                SandboxGrant::new(CanonPath::new("/outside/published"), SandboxAccess::Read);
            writer
                .remember(
                    if existing_profile {
                        ManagedRuleEffect::Deny
                    } else {
                        ManagedRuleEffect::Allow
                    },
                    RememberPlan::for_sandbox_path(&grant).unwrap(),
                )
                .await
                .unwrap();
            let store: Arc<dyn UserConfigStore> =
                Arc::new(OsUserConfigStore::new(Arc::new(OsIo), path));
            let owner = PolicyApplicationOwner::new(
                selection.clone(),
                store.clone(),
                Arc::new(Preflight),
                resolver(),
            );
            owner
                .apply(PolicyControlRequest::Rule(RuleMutation {
                    effect: ManagedRuleEffect::Allow,
                    rule: Rule::load("read(/outside/base-later)").unwrap(),
                }))
                .unwrap();
            assert_eq!(
                selection.snapshot().unwrap(),
                admitted,
                "published defaults changed admitted authority"
            );
            let captured = selection.capture().unwrap();
            let user_rules = |entries: &[ego_core::config::SourcedRule]| {
                entries
                    .iter()
                    .filter(|entry| entry.origins().iter().any(|origin| origin == Source::User))
                    .map(|entry| entry.rule().to_string())
                    .collect::<Vec<_>>()
            };
            let allow = user_rules(&captured.ruleset.allow);
            let deny = user_rules(&captured.ruleset.deny);
            if existing_profile {
                assert_eq!(allow, ["read(/outside/profile)"]);
                assert_eq!(deny, ["read(/outside/published)"]);
            } else {
                assert_eq!(allow, ["read(/outside/base)", "read(/outside/published)"]);
                assert!(deny.is_empty());
            }
            let profile = store.rule_snapshot(profile).unwrap();
            assert_eq!(
                allow,
                profile
                    .allow
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                deny,
                profile
                    .deny
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
            );
        }
    }

    // Catches: a failed rule publication still stages a deny in the live session.
    #[tokio::test]
    async fn failed_root_deny_publication_does_not_stage_policy() {
        let home = tempfile::tempdir().unwrap();
        let selection = GenerationPolicySelection::new(
            ego_core::config::Mode::Default,
            ego_core::config::Ruleset::default(),
        );
        let resolver = Arc::new(crate::run::FrozenRuleResolver::new(
            vec![],
            ego_core::config::Trust::Untrusted,
            None,
            selection.clone(),
        ));
        // A directory cannot be replaced by the atomic config-file writer.
        let writer = CliAcpPermissionRules::new(home.path().to_path_buf(), UserConfigTarget::Base)
            .with_live_policy(selection.clone(), resolver);
        let grant = SandboxGrant::new(CanonPath::new("/outside/data"), SandboxAccess::Write);
        let before = selection.snapshot().unwrap();
        let result = writer
            .remember(
                ManagedRuleEffect::Deny,
                RememberPlan::for_sandbox_path(&grant).unwrap(),
            )
            .await;
        assert!(result.is_err());
        assert!(home.path().is_dir());
        assert_eq!(selection.capture().unwrap(), before);
        assert!(selection.staged_snapshot().unwrap().is_none());
    }

    // Catches: a profile with no rules ignores its inherited base deny.
    #[tokio::test]
    async fn acp_profile_without_rules_inherits_base_sandbox_deny() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("config.toml");
        let grant = SandboxGrant::new(CanonPath::new("/outside/data"), SandboxAccess::Write);
        let base = CliAcpPermissionRules::new(path.clone(), UserConfigTarget::Base);
        base.remember(
            ManagedRuleEffect::Deny,
            RememberPlan::for_sandbox_path(&grant).unwrap(),
        )
        .await
        .unwrap();
        let profile = CliAcpPermissionRules::new(
            path,
            UserConfigTarget::Profile(ego_core::config::ProfileName::parse("work").unwrap()),
        );
        assert!(
            profile
                .denies_sandbox(&grant, std::path::Path::new("/workspace"))
                .unwrap()
        );
    }

    // Catches: an ACP keep-blocked answer is lost when the next seat is created.
    #[tokio::test]
    async fn acp_sandbox_deny_survives_new_rule_writer() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("config.toml");
        let grant = SandboxGrant::new(CanonPath::new("/outside/data"), SandboxAccess::Write);
        let writer = CliAcpPermissionRules::new(path.clone(), UserConfigTarget::Base);
        let plan = RememberPlan::for_sandbox_path(&grant).unwrap();
        writer
            .remember(ManagedRuleEffect::Deny, plan)
            .await
            .unwrap();
        let next = CliAcpPermissionRules::new(path, UserConfigTarget::Base);
        assert!(
            next.denies_sandbox(&grant, std::path::Path::new("/workspace"))
                .unwrap()
        );
    }
}
