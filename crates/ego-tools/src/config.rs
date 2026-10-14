//! OS adapter for stored user-configuration reads and atomic mutations.

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use ego_core::config::{
    ConfigLayer, ManagedConfigKey, ManagedConfigValue, ManagedRuleEffect, McpHttpAuth,
    McpLayerEntry, McpServerConfig, RuleMutation, RuleSnapshot, Source, UserConfigBatch,
    UserConfigEditHandle, UserConfigEditResult, UserConfigError, UserConfigMutation,
    UserConfigMutationResult, UserConfigPublication, UserConfigSnapshot, UserConfigStore,
    UserConfigTarget,
};
use ego_core::permit::rule::{Rule, RuleId};
use fd_lock::RwLock;
use sha2::{Digest as _, Sha256};
use toml_edit::{Array, DocumentMut, Item, Table, TableLike, Value, value};

use crate::privacy::{PrivatePathKind, harden_private_path};
use crate::store::{AtomicReplacePublication, StoreIo};

/// The real filesystem adapter for one user `config.toml` authority.
pub struct OsUserConfigStore<I> {
    io: I,
    path: PathBuf,
}

impl<I> OsUserConfigStore<I> {
    pub fn new(io: I, path: impl AsRef<Path>) -> Self {
        Self {
            io,
            path: path.as_ref().to_path_buf(),
        }
    }
}

impl<I> OsUserConfigStore<I>
where
    I: std::ops::Deref + Send + Sync,
    I::Target: StoreIo,
{
    pub fn prepare_edit(&self) -> Result<Box<dyn UserConfigEditHandle>, UserConfigError> {
        <Self as UserConfigStore>::prepare_edit(self)
    }

    pub fn publish_edit(
        &self,
        edit: Box<dyn UserConfigEditHandle>,
    ) -> Result<UserConfigEditResult, UserConfigError> {
        <Self as UserConfigStore>::publish_edit(self, edit)
    }

    pub fn discard_edit(&self, edit: Box<dyn UserConfigEditHandle>) -> Result<(), UserConfigError> {
        <Self as UserConfigStore>::discard_edit(self, edit)
    }
}

struct OsUserConfigEdit {
    candidate_path: PathBuf,
    authority_digest: [u8; 32],
}

impl UserConfigEditHandle for OsUserConfigEdit {
    fn candidate_path(&self) -> &Path {
        &self.candidate_path
    }

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any + Send> {
        self
    }
}

struct OsUserConfigBatch {
    path: PathBuf,
    before: String,
    candidate: String,
    changes: Vec<UserConfigMutation>,
}

impl UserConfigBatch for OsUserConfigBatch {
    fn before(&self) -> &str {
        &self.before
    }
    fn candidate(&self) -> &str {
        &self.candidate
    }
    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any + Send> {
        self
    }
}

fn batch_candidate(text: &str, changes: &[UserConfigMutation]) -> Result<String, UserConfigError> {
    let mut document = text
        .parse::<DocumentMut>()
        .map_err(|error| UserConfigError::new(format!("invalid setup config TOML: {error}")))?;
    for change in changes {
        change.validate()?;
        if !matches!(
            change,
            UserConfigMutation::Root { .. }
                | UserConfigMutation::Rules { .. }
                | UserConfigMutation::ManagedSet {
                    key: ManagedConfigKey::ExecPaths | ManagedConfigKey::PermissionsJudge,
                    ..
                }
        ) {
            return Err(UserConfigError::new(
                "setup batch contains unsupported mutation",
            ));
        }
        let parsed = parse_user_config_bytes(document.to_string().as_bytes(), "setup candidate")?;
        apply(&mut document, &parsed, change.clone())?;
    }
    let candidate = document.to_string();
    parse_user_config_bytes(candidate.as_bytes(), "setup candidate")?;
    Ok(candidate)
}

impl<I> UserConfigStore for OsUserConfigStore<I>
where
    I: std::ops::Deref + Send + Sync,
    I::Target: StoreIo,
{
    fn snapshot(&self, target: UserConfigTarget) -> Result<UserConfigSnapshot, UserConfigError> {
        let (text, parsed) = read_user_config(&*self.io, &self.path)?;
        let document = text
            .parse::<DocumentMut>()
            .map_err(|_| UserConfigError::new("invalid user config TOML"))?;
        let mut keys = ManagedConfigKey::for_target(target.clone()).to_vec();
        if target == UserConfigTarget::Base {
            let mut providers = parsed.auth.keys().cloned().collect::<Vec<_>>();
            providers.sort();
            for provider in providers {
                keys.push(ManagedConfigKey::AuthHeaders(provider.clone()));
                keys.push(ManagedConfigKey::AuthCredentialCommand(provider));
            }
        }
        let values = keys
            .into_iter()
            .map(|key| {
                let value = managed_value(&parsed, &target, &key, &document);
                (key, value)
            })
            .collect();
        Ok(UserConfigSnapshot { target, values })
    }

    fn rule_snapshot(&self, target: UserConfigTarget) -> Result<RuleSnapshot, UserConfigError> {
        let (_, parsed) = read_user_config(&*self.io, &self.path)?;
        Ok(rule_snapshot(&parsed, target))
    }

    fn mutate(
        &self,
        mutation: UserConfigMutation,
    ) -> Result<UserConfigMutationResult, UserConfigError> {
        mutation.validate()?;
        let target = mutation.target();
        let facts = mutation.facts();

        let mut lock = user_config_lock(&self.path)?;
        let _guard = lock.write().map_err(transaction_error)?;

        let (text, parsed) = read_user_config(&*self.io, &self.path)?;
        let before_rules = matches!(
            &mutation,
            UserConfigMutation::Rules { .. } | UserConfigMutation::RevokeRule { .. }
        )
        .then(|| rule_snapshot(&parsed, target.clone()));
        let mut document = text
            .parse::<DocumentMut>()
            .map_err(|error| UserConfigError::new(format!("invalid user config TOML: {error}")))?;

        let changed = apply(&mut document, &parsed, mutation)?;
        if !changed {
            let facts = rule_transaction_facts(facts, before_rules.as_ref(), before_rules.clone());
            return Ok(UserConfigMutationResult {
                target,
                facts,
                changed: false,
                publication: UserConfigPublication::NotPublished,
            });
        }
        let published = document.to_string();
        let published_layer = ConfigLayer::parse(Source::User, &published)
            .map_err(|error| {
                UserConfigError::new(format!(
                    "mutated user config failed strict validation: {error}"
                ))
            })?
            .value;
        let after_rules = before_rules
            .as_ref()
            .map(|_| rule_snapshot(&published_layer, target.clone()));
        let publication = self
            .io
            .replace_atomic_publication(&self.path, published.as_bytes())
            .map_err(transaction_error)?;
        let publication = match publication {
            AtomicReplacePublication::PublishedDurably => UserConfigPublication::PublishedDurably,
            AtomicReplacePublication::PublishedDurabilityUncertain { diagnostic } => {
                UserConfigPublication::PublishedDurabilityUncertain { diagnostic }
            }
        };
        let facts = rule_transaction_facts(facts, before_rules.as_ref(), after_rules);
        Ok(UserConfigMutationResult {
            target,
            facts,
            changed: true,
            publication,
        })
    }

    fn prepare_batch(
        &self,
        changes: Vec<UserConfigMutation>,
    ) -> Result<Box<dyn UserConfigBatch>, UserConfigError> {
        if changes.is_empty()
            || changes
                .iter()
                .any(|change| change.target() != changes[0].target())
        {
            return Err(UserConfigError::new(
                "setup batch needs changes to one exact target",
            ));
        }
        let (before, _) = read_user_config(&*self.io, &self.path)?;
        let candidate = batch_candidate(&before, &changes)?;
        Ok(Box::new(OsUserConfigBatch {
            path: self.path.clone(),
            before,
            candidate,
            changes,
        }))
    }

    fn publish_batch(
        &self,
        batch: Box<dyn UserConfigBatch>,
    ) -> Result<UserConfigEditResult, UserConfigError> {
        let batch = batch
            .into_any()
            .downcast::<OsUserConfigBatch>()
            .map_err(|_| UserConfigError::new("setup batch does not belong to OS config store"))?;
        if batch.path != self.path {
            return Err(UserConfigError::new("setup batch target changed"));
        }
        let mut lock = user_config_lock(&self.path)?;
        let _guard = lock.write().map_err(transaction_error)?;
        let current = read_user_config_bytes(&*self.io, &self.path)?;
        if sha256(&current) != sha256(batch.before.as_bytes()) {
            return Err(UserConfigError::new(
                "user config changed after setup preview; confirm a fresh diff",
            ));
        }
        let candidate = batch_candidate(&batch.before, &batch.changes)?;
        if sha256(candidate.as_bytes()) != sha256(batch.candidate.as_bytes()) {
            return Err(UserConfigError::new(
                "setup candidate changed after confirmation",
            ));
        }
        let publication = if candidate.as_bytes() == current {
            UserConfigPublication::NotPublished
        } else {
            match self
                .io
                .replace_atomic_publication(&self.path, candidate.as_bytes())
                .map_err(transaction_error)?
            {
                AtomicReplacePublication::PublishedDurably => {
                    UserConfigPublication::PublishedDurably
                }
                AtomicReplacePublication::PublishedDurabilityUncertain { diagnostic } => {
                    UserConfigPublication::PublishedDurabilityUncertain { diagnostic }
                }
            }
        };
        Ok(UserConfigEditResult {
            changed: candidate.as_bytes() != current,
            publication,
            candidate_cleanup_warning: None,
        })
    }

    fn prepare_edit(&self) -> Result<Box<dyn UserConfigEditHandle>, UserConfigError> {
        let bytes = read_user_config_bytes(&*self.io, &self.path)?;
        parse_user_config_bytes(&bytes, "user config")?;
        let authority_digest = sha256(&bytes);
        let candidate_path = create_edit_candidate(&self.path, &bytes)?;
        Ok(Box::new(OsUserConfigEdit {
            candidate_path,
            authority_digest,
        }))
    }

    fn publish_edit(
        &self,
        edit: Box<dyn UserConfigEditHandle>,
    ) -> Result<UserConfigEditResult, UserConfigError> {
        let edit = edit
            .into_any()
            .downcast::<OsUserConfigEdit>()
            .map_err(|_| {
                UserConfigError::new("edit handle does not belong to the OS config store")
            })?;
        validate_candidate_path(&self.path, &edit.candidate_path)?;
        validate_candidate_file(&edit.candidate_path)?;
        let candidate = self
            .io
            .read(&edit.candidate_path)
            .map_err(transaction_error)?
            .ok_or_else(|| UserConfigError::new("user config edit candidate is missing"))?;
        parse_user_config_bytes(&candidate, "edited user config")?;

        let mut lock = user_config_lock(&self.path)?;
        let _guard = lock.write().map_err(transaction_error)?;
        let current = read_user_config_bytes(&*self.io, &self.path)?;
        parse_user_config_bytes(&current, "user config")?;
        if sha256(&current) != edit.authority_digest {
            return Err(UserConfigError::new(
                "user config changed after the edit candidate was prepared",
            ));
        }
        if candidate == current {
            self.io
                .remove_file(&edit.candidate_path)
                .map_err(transaction_error)?;
            return Ok(UserConfigEditResult {
                changed: false,
                publication: UserConfigPublication::NotPublished,
                candidate_cleanup_warning: None,
            });
        }

        let publication = self
            .io
            .replace_atomic_publication(&self.path, &candidate)
            .map_err(transaction_error)?;
        match publication {
            AtomicReplacePublication::PublishedDurably => {
                let candidate_cleanup_warning = self
                    .io
                    .remove_file(&edit.candidate_path)
                    .err()
                    .map(|error| {
                        format!("published config candidate could not be removed: {error}")
                    });
                Ok(UserConfigEditResult {
                    changed: true,
                    publication: UserConfigPublication::PublishedDurably,
                    candidate_cleanup_warning,
                })
            }
            AtomicReplacePublication::PublishedDurabilityUncertain { diagnostic } => {
                Ok(UserConfigEditResult {
                    changed: true,
                    publication: UserConfigPublication::PublishedDurabilityUncertain { diagnostic },
                    candidate_cleanup_warning: None,
                })
            }
        }
    }

    fn discard_edit(&self, edit: Box<dyn UserConfigEditHandle>) -> Result<(), UserConfigError> {
        let edit = edit
            .into_any()
            .downcast::<OsUserConfigEdit>()
            .map_err(|_| {
                UserConfigError::new("edit handle does not belong to the OS config store")
            })?;
        validate_candidate_path(&self.path, &edit.candidate_path)?;
        self.io
            .remove_file(&edit.candidate_path)
            .map_err(transaction_error)
    }
}

fn rule_transaction_facts(
    request: ego_core::config::UserConfigMutationFacts,
    before: Option<&RuleSnapshot>,
    after: Option<RuleSnapshot>,
) -> ego_core::config::UserConfigMutationFacts {
    match (before, after) {
        (Some(before), Some(after)) => ego_core::config::UserConfigMutationFacts::RuleTransaction {
            request: Box::new(request),
            before: before.clone(),
            after,
        },
        (None, None) => request,
        _ => unreachable!("rule transaction snapshots are captured as a pair"),
    }
}

fn read_user_config_bytes<I>(io: &I, path: &Path) -> Result<Vec<u8>, UserConfigError>
where
    I: StoreIo + ?Sized,
{
    Ok(io
        .read(path)
        .map_err(transaction_error)?
        .unwrap_or_default())
}

fn parse_user_config_bytes(
    bytes: &[u8],
    description: &str,
) -> Result<ConfigLayer, UserConfigError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|error| UserConfigError::new(format!("{description} is not UTF-8: {error}")))?;
    ConfigLayer::parse(Source::User, text)
        .map(|parsed| parsed.value)
        .map_err(|error| UserConfigError::new(format!("invalid {description}: {error}")))
}

fn read_user_config<I>(io: &I, path: &Path) -> Result<(String, ConfigLayer), UserConfigError>
where
    I: StoreIo + ?Sized,
{
    let bytes = read_user_config_bytes(io, path)?;
    let parsed = parse_user_config_bytes(&bytes, "user config")?;
    let text = String::from_utf8(bytes).expect("strict user config parsing accepted UTF-8");
    Ok((text, parsed))
}

fn apply(
    document: &mut DocumentMut,
    parsed: &ConfigLayer,
    mutation: UserConfigMutation,
) -> Result<bool, UserConfigError> {
    match mutation {
        UserConfigMutation::Root {
            target,
            path,
            access,
        } => {
            let mut roots = match managed_value(parsed, &target, &ManagedConfigKey::Roots, document)
            {
                Some(ManagedConfigValue::Roots(roots)) => roots,
                _ => Vec::new(),
            };
            let before = roots.clone();
            if let Some(access) = access {
                if let Some(root) = roots.iter_mut().find(|root| root.path == path) {
                    root.access = access;
                } else {
                    roots.push(ego_core::permit::RootCfg { path, access });
                }
            } else {
                roots.retain(|root| root.path != path);
            }
            if roots == before {
                return Ok(false);
            }
            apply_managed_set(
                document,
                &target,
                ManagedConfigKey::Roots,
                &ManagedConfigValue::Roots(roots),
            )?;
            Ok(true)
        }
        UserConfigMutation::ManagedSet { target, key, value } => {
            if managed_value(parsed, &target, &key, document).as_ref() == Some(&value) {
                return Ok(false);
            }
            apply_managed_set(document, &target, key, &value)?;
            Ok(true)
        }
        UserConfigMutation::ManagedUnset { target, key } => {
            if managed_value(parsed, &target, &key, document).is_none() {
                return Ok(false);
            }
            apply_managed_unset(document, &target, key)?;
            Ok(true)
        }
        UserConfigMutation::ModelAndThinking { model, thinking } => {
            if parsed
                .model
                .as_ref()
                .is_some_and(|current| current.0 == model)
                && parsed.thinking == Some(thinking)
            {
                return Ok(false);
            }
            let root = document.as_table_mut();
            replace_string(root.entry("model").or_insert(Item::None), &model)?;
            replace_string(
                root.entry("thinking").or_insert(Item::None),
                thinking.as_str(),
            )?;
            Ok(true)
        }
        UserConfigMutation::Statusline { segments } => {
            if statusline_matches(parsed, &segments) {
                return Ok(false);
            }
            let mut array = Array::new();
            for segment in &segments {
                array.push(segment.as_str());
            }
            set_table_path(
                document.as_table_mut(),
                &managed_path(&ManagedConfigKey::StatuslineSegments),
                value(array),
            )?;
            Ok(true)
        }
        UserConfigMutation::McpSet {
            target,
            name,
            expected_user_entry,
            server,
        } => {
            let current = mcp_entry(parsed, &target, &name);
            if current != expected_user_entry.as_ref() {
                return Err(UserConfigError::new(
                    "user config changed before the MCP entry could be set",
                ));
            }
            if matches!(current, Some(McpLayerEntry::Server(current)) if current == &server) {
                return Ok(false);
            }
            let servers = mcp_servers_table_mut(document, &target)?;
            servers.insert(&name.to_string(), Item::Table(server_table(&server)?));
            Ok(true)
        }
        UserConfigMutation::McpDeleteUserEntry {
            target,
            name,
            expected_entry,
        } => {
            let current = mcp_entry(parsed, &target, &name);
            if current != Some(&expected_entry) {
                return Err(UserConfigError::new(
                    "user config changed before the MCP entry could be deleted",
                ));
            }
            let servers = existing_mcp_servers_table_mut(document, &target).ok_or_else(|| {
                UserConfigError::new("the expected MCP entry is absent from the user config")
            })?;
            if servers.remove(&name.to_string()).is_none() {
                return Err(UserConfigError::new(
                    "the expected MCP entry is absent from the user config",
                ));
            }
            Ok(true)
        }
        UserConfigMutation::McpInsertUserTombstone {
            name,
            expected_user_entry,
        } => {
            let current = parsed.mcp.servers.get(&name);
            if current != expected_user_entry.as_ref() {
                return Err(UserConfigError::new(
                    "user config changed before the MCP tombstone could be inserted",
                ));
            }
            let target = mcp_servers_table_mut(document, &UserConfigTarget::Base)?;
            let mut tombstone = Table::new();
            tombstone.insert("removed", value(true));
            target.insert(&name.to_string(), Item::Table(tombstone));
            Ok(true)
        }
        UserConfigMutation::Rules { target, changes } => {
            apply_rule_changes(document, parsed, &target, &changes)
        }
        UserConfigMutation::RevokeRule { target, id } => {
            apply_rule_revoke(document, parsed, &target, &id)
        }
    }
}

fn rule_snapshot(parsed: &ConfigLayer, target: UserConfigTarget) -> RuleSnapshot {
    let permissions = match &target {
        UserConfigTarget::Base => parsed.permissions.as_ref(),
        UserConfigTarget::Profile(name) => parsed
            .profile
            .get(name.as_str())
            .and_then(|profile| profile.permissions.as_ref()),
    };
    RuleSnapshot {
        target,
        permissions_present: permissions.is_some(),
        allow: permissions
            .map(|permissions| permissions.allow.clone())
            .unwrap_or_default(),
        deny: permissions
            .map(|permissions| permissions.deny.clone())
            .unwrap_or_default(),
    }
}

fn apply_rule_changes(
    document: &mut DocumentMut,
    parsed: &ConfigLayer,
    target: &UserConfigTarget,
    changes: &[RuleMutation],
) -> Result<bool, UserConfigError> {
    let snapshot = rule_snapshot(parsed, target.clone());
    let mut allow = snapshot.allow;
    let mut deny = snapshot.deny;
    let mut additions = Vec::new();

    for change in changes {
        let (same, opposite) = match change.effect {
            ManagedRuleEffect::Allow => (&allow, &deny),
            ManagedRuleEffect::Deny => (&deny, &allow),
        };
        if opposite.iter().any(|rule| rule == &change.rule) {
            return Err(UserConfigError::new(format!(
                "rule `{}` already exists with the opposite effect",
                change.rule
            )));
        }
        if same.iter().any(|rule| rule == &change.rule) {
            continue;
        }
        match change.effect {
            ManagedRuleEffect::Allow => allow.push(change.rule.clone()),
            ManagedRuleEffect::Deny => deny.push(change.rule.clone()),
        }
        additions.push(change);
    }

    if additions.is_empty() {
        return Ok(false);
    }
    for change in additions {
        rule_array_mut(document, target, change.effect)?.push(change.rule.to_string());
    }
    Ok(true)
}

fn apply_rule_revoke(
    document: &mut DocumentMut,
    parsed: &ConfigLayer,
    target: &UserConfigTarget,
    id: &RuleId,
) -> Result<bool, UserConfigError> {
    let snapshot = rule_snapshot(parsed, target.clone());
    let allow = matching_rule_indices(&snapshot.allow, id)?;
    let deny = matching_rule_indices(&snapshot.deny, id)?;
    if let (Some(allow_index), Some(deny_index)) = (allow.first(), deny.first())
        && snapshot.allow[*allow_index] != snapshot.deny[*deny_index]
    {
        return Err(UserConfigError::new(format!(
            "rule ID `{id}` is ambiguous in the selected user config target"
        )));
    }
    if allow.is_empty() && deny.is_empty() {
        return Ok(false);
    }
    remove_rule_indices(document, target, ManagedRuleEffect::Allow, &allow)?;
    remove_rule_indices(document, target, ManagedRuleEffect::Deny, &deny)?;
    prune_rule_target(document, target)?;
    Ok(true)
}

fn matching_rule_indices(rules: &[Rule], id: &RuleId) -> Result<Vec<usize>, UserConfigError> {
    let matches = rules
        .iter()
        .enumerate()
        .filter(|(_, rule)| rule.id() == *id)
        .collect::<Vec<_>>();
    if matches
        .iter()
        .skip(1)
        .any(|(_, rule)| *rule != matches[0].1)
    {
        return Err(UserConfigError::new(format!(
            "rule ID `{id}` is ambiguous in the selected user config target"
        )));
    }
    Ok(matches.into_iter().map(|(index, _)| index).collect())
}

fn rule_array_mut<'a>(
    document: &'a mut DocumentMut,
    target: &UserConfigTarget,
    effect: ManagedRuleEffect,
) -> Result<&'a mut Array, UserConfigError> {
    let target = managed_target_table_mut(document, target)?;
    let permissions = table_like_field_mut(target, "permissions")?;
    let field = rule_effect_field(effect);
    if !permissions.contains_key(field) {
        permissions.insert(field, value(Array::new()));
    }
    permissions
        .get_mut(field)
        .and_then(Item::as_array_mut)
        .ok_or_else(|| {
            UserConfigError::new(format!("user config `permissions.{field}` is not an array"))
        })
}

fn remove_rule_indices(
    document: &mut DocumentMut,
    target: &UserConfigTarget,
    effect: ManagedRuleEffect,
    indices: &[usize],
) -> Result<(), UserConfigError> {
    if indices.is_empty() {
        return Ok(());
    }
    let array = existing_rule_array_mut(document, target, effect)?;
    for index in indices.iter().rev() {
        array.remove(*index);
    }
    if array.is_empty() {
        let permissions = existing_permissions_table_mut(document, target)?;
        permissions.remove(rule_effect_field(effect));
    }
    Ok(())
}

fn existing_rule_array_mut<'a>(
    document: &'a mut DocumentMut,
    target: &UserConfigTarget,
    effect: ManagedRuleEffect,
) -> Result<&'a mut Array, UserConfigError> {
    existing_permissions_table_mut(document, target)?
        .get_mut(rule_effect_field(effect))
        .and_then(Item::as_array_mut)
        .ok_or_else(|| UserConfigError::new("stored permission rule list is missing"))
}

fn existing_permissions_table_mut<'a>(
    document: &'a mut DocumentMut,
    target: &UserConfigTarget,
) -> Result<&'a mut dyn TableLike, UserConfigError> {
    let target = match target {
        UserConfigTarget::Base => document.as_table_mut() as &mut dyn TableLike,
        UserConfigTarget::Profile(name) => {
            let profiles = existing_table_like_field_mut(document.as_table_mut(), "profile")?;
            existing_table_like_field_mut(profiles, name.as_str())?
        }
    };
    existing_table_like_field_mut(target, "permissions")
}

fn prune_rule_target(
    document: &mut DocumentMut,
    target: &UserConfigTarget,
) -> Result<(), UserConfigError> {
    match target {
        UserConfigTarget::Base => {
            if document
                .as_table()
                .get("permissions")
                .and_then(Item::as_table_like)
                .is_some_and(|table| table.is_empty())
            {
                document.as_table_mut().remove("permissions");
            }
        }
        UserConfigTarget::Profile(name) => {
            let root = document.as_table_mut();
            let remove_profiles = {
                let profiles = existing_table_like_field_mut(root, "profile")?;
                let remove_profile = {
                    let profile = existing_table_like_field_mut(profiles, name.as_str())?;
                    if profile
                        .get("permissions")
                        .and_then(Item::as_table_like)
                        .is_some_and(|table| table.is_empty())
                    {
                        profile.remove("permissions");
                    }
                    profile.is_empty()
                };
                if remove_profile {
                    profiles.remove(name.as_str());
                }
                profiles.is_empty()
            };
            if remove_profiles {
                root.remove("profile");
            }
        }
    }
    Ok(())
}

fn rule_effect_field(effect: ManagedRuleEffect) -> &'static str {
    match effect {
        ManagedRuleEffect::Allow => "allow",
        ManagedRuleEffect::Deny => "deny",
    }
}

fn managed_value(
    parsed: &ConfigLayer,
    target: &UserConfigTarget,
    key: &ManagedConfigKey,
    document: &DocumentMut,
) -> Option<ManagedConfigValue> {
    match target {
        UserConfigTarget::Base => match key {
            ManagedConfigKey::ExecPaths => {
                parsed.exec_paths.clone().map(ManagedConfigValue::ExecPaths)
            }
            ManagedConfigKey::AuthHeaders(provider) => {
                document
                    .get("auth")?
                    .as_table_like()?
                    .get(provider)?
                    .as_table_like()?
                    .get("headers")?;
                Some(ManagedConfigValue::AuthHeaders(
                    parsed.auth.get(provider)?.headers.clone(),
                ))
            }
            ManagedConfigKey::AuthCredentialCommand(provider) => parsed
                .auth
                .get(provider)?
                .credential_command
                .clone()
                .map(ManagedConfigValue::AuthCredentialCommand),
            ManagedConfigKey::Roots => parsed.roots.clone().map(ManagedConfigValue::Roots),
            ManagedConfigKey::Model => parsed.model.clone().map(ManagedConfigValue::Model),
            ManagedConfigKey::Thinking => parsed.thinking.map(ManagedConfigValue::Thinking),
            ManagedConfigKey::Mode => parsed.mode.map(ManagedConfigValue::Mode),
            ManagedConfigKey::Sandbox => parsed.sandbox.map(ManagedConfigValue::Sandbox),
            ManagedConfigKey::ExecEnv => parsed
                .exec
                .as_ref()
                .and_then(|exec| exec.env)
                .map(ManagedConfigValue::ExecEnv),
            ManagedConfigKey::ExecEnvAllowlist => parsed
                .exec
                .as_ref()
                .and_then(|exec| exec.env_allowlist.clone())
                .map(ManagedConfigValue::ExecEnvAllowlist),
            ManagedConfigKey::Network => parsed.network.map(ManagedConfigValue::Network),
            ManagedConfigKey::Budget => parsed.budget.clone().map(ManagedConfigValue::Budget),
            ManagedConfigKey::Bare => parsed.bare.map(ManagedConfigValue::Bare),
            ManagedConfigKey::PermissionsJudge => parsed
                .permissions
                .as_ref()
                .and_then(|permissions| permissions.judge.clone())
                .map(ManagedConfigValue::PermissionsJudge),
            ManagedConfigKey::StatuslineSegments => parsed
                .ui
                .as_ref()
                .and_then(|ui| ui.statusline.as_ref())
                .and_then(|statusline| statusline.segments.clone())
                .map(ManagedConfigValue::StatuslineSegments),
            ManagedConfigKey::SessionDir | ManagedConfigKey::ArtifactDir => None,
        },
        UserConfigTarget::Profile(name) => {
            let profile = parsed.profile.get(name.as_str())?;
            match key {
                ManagedConfigKey::ExecPaths => profile
                    .exec_paths
                    .clone()
                    .map(ManagedConfigValue::ExecPaths),
                ManagedConfigKey::AuthHeaders(_) | ManagedConfigKey::AuthCredentialCommand(_) => {
                    None
                }
                ManagedConfigKey::Roots => profile.roots.clone().map(ManagedConfigValue::Roots),
                ManagedConfigKey::Model => profile.model.clone().map(ManagedConfigValue::Model),
                ManagedConfigKey::Thinking => profile.thinking.map(ManagedConfigValue::Thinking),
                ManagedConfigKey::Mode => profile.mode.map(ManagedConfigValue::Mode),
                ManagedConfigKey::Sandbox => profile.sandbox.map(ManagedConfigValue::Sandbox),
                ManagedConfigKey::ExecEnv => profile
                    .exec
                    .as_ref()
                    .and_then(|exec| exec.env)
                    .map(ManagedConfigValue::ExecEnv),
                ManagedConfigKey::ExecEnvAllowlist => profile
                    .exec
                    .as_ref()
                    .and_then(|exec| exec.env_allowlist.clone())
                    .map(ManagedConfigValue::ExecEnvAllowlist),
                ManagedConfigKey::Network => profile.network.map(ManagedConfigValue::Network),
                ManagedConfigKey::Budget => profile.budget.clone().map(ManagedConfigValue::Budget),
                ManagedConfigKey::PermissionsJudge => profile
                    .permissions
                    .as_ref()
                    .and_then(|permissions| permissions.judge.clone())
                    .map(ManagedConfigValue::PermissionsJudge),
                ManagedConfigKey::SessionDir => {
                    profile.session_dir.clone().map(ManagedConfigValue::Path)
                }
                ManagedConfigKey::ArtifactDir => {
                    profile.artifact_dir.clone().map(ManagedConfigValue::Path)
                }
                ManagedConfigKey::Bare | ManagedConfigKey::StatuslineSegments => None,
            }
        }
    }
}

fn statusline_matches(parsed: &ConfigLayer, segments: &[String]) -> bool {
    let Some(current) = parsed
        .ui
        .as_ref()
        .and_then(|ui| ui.statusline.as_ref())
        .and_then(|statusline| statusline.segments.as_ref())
    else {
        return false;
    };
    current.len() == segments.len()
        && current
            .iter()
            .zip(segments)
            .all(|(left, right)| left.as_str() == right)
}

fn apply_managed_set(
    document: &mut DocumentMut,
    target: &UserConfigTarget,
    key: ManagedConfigKey,
    managed: &ManagedConfigValue,
) -> Result<(), UserConfigError> {
    let replacement = managed.to_toml().parse::<Value>().map_err(|error| {
        UserConfigError::new(format!(
            "managed configuration key `{key}` could not be represented as TOML: {error}"
        ))
    })?;
    let table = managed_target_table_mut(document, target)?;
    set_table_path(table, &managed_path(&key), Item::Value(replacement))
}

fn apply_managed_unset(
    document: &mut DocumentMut,
    target: &UserConfigTarget,
    key: ManagedConfigKey,
) -> Result<(), UserConfigError> {
    let path = managed_path(&key);
    let removed = match target {
        UserConfigTarget::Base => remove_table_path(document.as_table_mut(), &path)?,
        UserConfigTarget::Profile(name) => {
            let root = document.as_table_mut();
            let remove_profiles = {
                let profiles = existing_table_like_field_mut(root, "profile")?;
                let remove_profile = {
                    let profile = existing_table_like_field_mut(profiles, name.as_str())?;
                    let removed = remove_table_path(profile, &path)?;
                    if !removed {
                        return Err(missing_managed_field(target, key));
                    }
                    profile.is_empty()
                };
                if remove_profile {
                    profiles.remove(name.as_str());
                }
                profiles.is_empty()
            };
            if remove_profiles {
                root.remove("profile");
            }
            true
        }
    };
    if removed {
        Ok(())
    } else {
        Err(missing_managed_field(target, key))
    }
}

fn managed_target_table_mut<'a>(
    document: &'a mut DocumentMut,
    target: &UserConfigTarget,
) -> Result<&'a mut dyn TableLike, UserConfigError> {
    match target {
        UserConfigTarget::Base => Ok(document.as_table_mut()),
        UserConfigTarget::Profile(name) => {
            let profiles = table_like_field_mut(document.as_table_mut(), "profile")?;
            table_like_field_mut(profiles, name.as_str())
        }
    }
}

fn managed_path(key: &ManagedConfigKey) -> Vec<&str> {
    match key {
        ManagedConfigKey::Model => vec!["model"],
        ManagedConfigKey::Thinking => vec!["thinking"],
        ManagedConfigKey::Mode => vec!["mode"],
        ManagedConfigKey::Sandbox => vec!["sandbox"],
        ManagedConfigKey::ExecEnv => vec!["exec", "env"],
        ManagedConfigKey::ExecEnvAllowlist => vec!["exec", "env_allowlist"],
        ManagedConfigKey::ExecPaths => vec!["exec_paths"],
        ManagedConfigKey::AuthHeaders(provider) => vec!["auth", provider.as_str(), "headers"],
        ManagedConfigKey::AuthCredentialCommand(provider) => {
            vec!["auth", provider.as_str(), "credential_command"]
        }
        ManagedConfigKey::Roots => vec!["roots"],
        ManagedConfigKey::Network => vec!["network"],
        ManagedConfigKey::Budget => vec!["budget"],
        ManagedConfigKey::Bare => vec!["bare"],
        ManagedConfigKey::PermissionsJudge => vec!["permissions", "judge"],
        ManagedConfigKey::StatuslineSegments => vec!["ui", "statusline", "segments"],
        ManagedConfigKey::SessionDir => vec!["session_dir"],
        ManagedConfigKey::ArtifactDir => vec!["artifact_dir"],
    }
}

fn set_table_path(
    table: &mut dyn TableLike,
    path: &[&str],
    replacement: Item,
) -> Result<(), UserConfigError> {
    let Some((field, rest)) = path.split_first() else {
        return Err(UserConfigError::new(
            "managed configuration field path is empty",
        ));
    };
    if rest.is_empty() {
        let target = table.entry(field).or_insert(Item::None);
        replace_item_preserving_decor(target, replacement);
        return Ok(());
    }
    let nested = table_like_field_mut(table, field)?;
    set_table_path(nested, rest, replacement)
}

fn remove_table_path(table: &mut dyn TableLike, path: &[&str]) -> Result<bool, UserConfigError> {
    let Some((field, rest)) = path.split_first() else {
        return Err(UserConfigError::new(
            "managed configuration field path is empty",
        ));
    };
    if rest.is_empty() {
        return Ok(table.remove(field).is_some());
    }
    let Some(item) = table.get_mut(field) else {
        return Ok(false);
    };
    let (removed, prune) = {
        let nested = item
            .as_table_like_mut()
            .ok_or_else(|| UserConfigError::new(format!("user config `{field}` is not a table")))?;
        let removed = remove_table_path(nested, rest)?;
        (removed, nested.is_empty())
    };
    if removed && prune {
        table.remove(field);
    }
    Ok(removed)
}

fn table_like_field_mut<'a>(
    table: &'a mut dyn TableLike,
    field: &str,
) -> Result<&'a mut dyn TableLike, UserConfigError> {
    if !table.contains_key(field) {
        let mut nested = Table::new();
        nested.set_implicit(true);
        table.insert(field, Item::Table(nested));
    }
    existing_table_like_field_mut(table, field)
}

fn existing_table_like_field_mut<'a>(
    table: &'a mut dyn TableLike,
    field: &str,
) -> Result<&'a mut dyn TableLike, UserConfigError> {
    table
        .get_mut(field)
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| UserConfigError::new(format!("user config `{field}` is not a table")))
}

fn missing_managed_field(target: &UserConfigTarget, key: ManagedConfigKey) -> UserConfigError {
    let target = match target {
        UserConfigTarget::Base => "base user configuration".to_owned(),
        UserConfigTarget::Profile(name) => format!("user profile `{name}`"),
    };
    UserConfigError::new(format!(
        "managed configuration key `{key}` is absent from {target}"
    ))
}

fn mcp_entry<'a>(
    parsed: &'a ConfigLayer,
    target: &UserConfigTarget,
    name: &ego_core::config::McpServerName,
) -> Option<&'a McpLayerEntry> {
    match target {
        UserConfigTarget::Base => parsed.mcp.servers.get(name),
        UserConfigTarget::Profile(profile) => parsed
            .profile
            .get(profile.as_str())
            .and_then(|layer| layer.mcp.servers.get(name)),
    }
}

fn mcp_servers_table_mut<'a>(
    document: &'a mut DocumentMut,
    target: &UserConfigTarget,
) -> Result<&'a mut Table, UserConfigError> {
    let root = document.as_table_mut();
    let mcp = match target {
        UserConfigTarget::Base => table_field_mut(root, "mcp")?,
        UserConfigTarget::Profile(profile) => {
            let profiles = table_field_mut(root, "profile")?;
            let profile = table_field_mut(profiles, profile.as_str())?;
            table_field_mut(profile, "mcp")?
        }
    };
    table_field_mut(mcp, "servers")
}

fn existing_mcp_servers_table_mut<'a>(
    document: &'a mut DocumentMut,
    target: &UserConfigTarget,
) -> Option<&'a mut Table> {
    let root = document.as_table_mut();
    let mcp = match target {
        UserConfigTarget::Base => root.get_mut("mcp")?.as_table_mut()?,
        UserConfigTarget::Profile(profile) => root
            .get_mut("profile")?
            .as_table_mut()?
            .get_mut(profile.as_str())?
            .as_table_mut()?
            .get_mut("mcp")?
            .as_table_mut()?,
    };
    mcp.get_mut("servers")?.as_table_mut()
}

fn table_field_mut<'a>(
    table: &'a mut Table,
    field: &str,
) -> Result<&'a mut Table, UserConfigError> {
    if !table.contains_key(field) {
        let mut nested = Table::new();
        nested.set_implicit(true);
        table.insert(field, Item::Table(nested));
    }
    table
        .get_mut(field)
        .and_then(Item::as_table_mut)
        .ok_or_else(|| UserConfigError::new(format!("user config `{field}` is not a table")))
}

fn server_table(server: &McpServerConfig) -> Result<Table, UserConfigError> {
    let mut table = Table::new();
    match server {
        McpServerConfig::Stdio(stdio) => {
            table.insert("transport", value("stdio"));
            table.insert("command", value(stdio.command.as_str()));
            if !stdio.args.is_empty() {
                table.insert("args", string_array(&stdio.args));
            }
            if let Some(cwd) = &stdio.cwd {
                let cwd = cwd.to_str().ok_or_else(|| {
                    UserConfigError::new("MCP stdio working directory is not valid UTF-8")
                })?;
                table.insert("cwd", value(cwd));
            }
            if !stdio.inherit_env.is_empty() {
                let names = stdio
                    .inherit_env
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>();
                table.insert("inherit_env", string_array(&names));
            }
        }
        McpServerConfig::Http(http) => {
            table.insert("transport", value("http"));
            table.insert("url", value(http.url.to_string()));
            if http.auth == McpHttpAuth::Oauth {
                table.insert("auth", value("oauth"));
            }
            if http.allow_insecure_loopback {
                table.insert("allow_insecure_loopback", value(true));
            }
        }
        McpServerConfig::Acp { .. } => {
            return Err(UserConfigError::new(
                "an ACP MCP server is supplied by the host, not stored in user config",
            ));
        }
    }
    Ok(table)
}

fn string_array(values: &[String]) -> Item {
    let mut array = Array::new();
    for value in values {
        array.push(value.as_str());
    }
    value(array)
}

/// Parse the scalar through `toml_edit` so the replacement keeps the literal
/// single-quoted representation used by an existing hand-authored config.
fn replace_string(target: &mut Item, text: &str) -> Result<(), UserConfigError> {
    let uses_literal_quotes = matches!(
        target.as_value(),
        Some(Value::String(formatted))
            if formatted
                .as_repr()
                .and_then(|repr| repr.as_raw().as_str())
                .is_some_and(|repr| repr.starts_with('\''))
    );
    let replacement = if uses_literal_quotes && !text.contains(['\'', '\n', '\r']) {
        let literal = format!("'{text}'");
        let value = literal.parse::<Value>().map_err(|error| {
            UserConfigError::new(format!("config string could not be represented: {error}"))
        })?;
        Item::Value(value)
    } else {
        value(text)
    };
    replace_item_preserving_decor(target, replacement);
    Ok(())
}

fn replace_item_preserving_decor(target: &mut Item, mut replacement: Item) {
    let decor = target.as_value().map(|value| value.decor().clone());
    if let (Some(decor), Some(value)) = (decor, replacement.as_value_mut()) {
        *value.decor_mut() = decor;
    }
    *target = replacement;
}

fn user_config_lock(path: &Path) -> Result<RwLock<fs::File>, UserConfigError> {
    let lock_path = lock_path_for(path);
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent).map_err(transaction_error)?;
    }
    let lock_file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_path)
        .map_err(transaction_error)?;
    Ok(RwLock::new(lock_file))
}

fn create_edit_candidate(path: &Path, bytes: &[u8]) -> Result<PathBuf, UserConfigError> {
    let file_name = path
        .file_name()
        .ok_or_else(|| UserConfigError::new("user config path has no file name"))?;
    for _ in 0..32 {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).map_err(|error| {
            UserConfigError::new(format!("user config candidate name failed: {error}"))
        })?;
        let mut candidate_name = file_name.to_os_string();
        candidate_name.push(".ego-edit-");
        candidate_name.push(hex(&random));
        let candidate_path = path.with_file_name(candidate_name);
        match create_private_file(&candidate_path) {
            Ok(mut file) => {
                if let Err(error) = file.write_all(bytes) {
                    let _ = fs::remove_file(&candidate_path);
                    return Err(transaction_error(error));
                }
                return Ok(candidate_path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(transaction_error(error)),
        }
    }
    Err(UserConfigError::new(
        "could not allocate a unique user config edit candidate",
    ))
}

fn create_private_file(path: &Path) -> std::io::Result<fs::File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    if let Err(error) = harden_private_path(path, PrivatePathKind::File) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(error);
    }
    Ok(file)
}

fn validate_candidate_path(authority: &Path, candidate: &Path) -> Result<(), UserConfigError> {
    if candidate.parent() != authority.parent() {
        return Err(UserConfigError::new(
            "user config edit candidate is not a sibling of its authority",
        ));
    }
    let authority_name = authority
        .file_name()
        .ok_or_else(|| UserConfigError::new("user config path has no file name"))?;
    let candidate_name = candidate
        .file_name()
        .ok_or_else(|| UserConfigError::new("user config edit candidate has no file name"))?;
    let mut prefix = authority_name.to_os_string();
    prefix.push(".ego-edit-");
    let prefix = prefix.as_encoded_bytes();
    let candidate_name = candidate_name.as_encoded_bytes();
    let suffix = candidate_name.strip_prefix(prefix).ok_or_else(|| {
        UserConfigError::new("user config edit candidate name does not belong to its authority")
    })?;
    if suffix.len() != 32 || !suffix.iter().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(UserConfigError::new(
            "user config edit candidate name is malformed",
        ));
    }
    Ok(())
}

fn validate_candidate_file(path: &Path) -> Result<(), UserConfigError> {
    let metadata = fs::symlink_metadata(path).map_err(transaction_error)?;
    if !metadata.file_type().is_file() {
        return Err(UserConfigError::new(
            "user config edit candidate is not a regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o777 != 0o600 {
            return Err(UserConfigError::new(
                "user config edit candidate permissions are not mode 0600",
            ));
        }
    }
    Ok(())
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        encoded.push(DIGITS[usize::from(byte >> 4)] as char);
        encoded.push(DIGITS[usize::from(byte & 0x0f)] as char);
    }
    encoded
}

fn lock_path_for(path: &Path) -> PathBuf {
    let mut lock = OsString::from(path.as_os_str());
    lock.push(".lock");
    PathBuf::from(lock)
}

fn transaction_error(error: std::io::Error) -> UserConfigError {
    UserConfigError::new(format!("user config transaction failed: {error}"))
}
