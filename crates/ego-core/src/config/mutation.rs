//! The consumer-owned port for stored user configuration.
//!
//! Core names the closed stored projection and each complete policy change. The
//! filesystem adapter is responsible for side-effect-free reads and for locking,
//! comment-preserving editing, strict validation, and durable publication as one
//! mutation operation.

use std::any::Any;
use std::path::Path;

use crate::generator::types::ThinkingLevel;
use crate::permit::rule::{Rule, RuleId};

use super::layer::{McpLayerEntry, McpServerConfig, McpServerName, validate_status_segment_names};
use super::vocabulary::{ManagedConfigKey, ManagedConfigValue, UserConfigTarget};

/// One complete, atomic change to the user configuration authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserConfigMutation {
    /// Add/update or remove one root under the adapter's atomic lock.
    Root {
        target: UserConfigTarget,
        path: std::path::PathBuf,
        access: Option<crate::permit::RootAccess>,
    },
    /// Set one field from the closed managed configuration vocabulary.
    ManagedSet {
        target: UserConfigTarget,
        key: ManagedConfigKey,
        value: ManagedConfigValue,
    },
    /// Remove one field from the closed managed configuration vocabulary.
    ManagedUnset {
        target: UserConfigTarget,
        key: ManagedConfigKey,
    },
    /// Change the model default and its reasoning effort together.
    ModelAndThinking {
        model: String,
        thinking: ThinkingLevel,
    },
    /// Replace the final ordered status-line segment list.
    Statusline { segments: Vec<String> },
    /// Set one complete server in the exact base or profile slot.
    McpSet {
        target: UserConfigTarget,
        name: McpServerName,
        expected_user_entry: Option<McpLayerEntry>,
        server: McpServerConfig,
    },
    /// Delete exactly the entry the caller resolved from a user-owned slot.
    McpDeleteUserEntry {
        target: UserConfigTarget,
        name: McpServerName,
        expected_entry: McpLayerEntry,
    },
    /// Hide a system entry only if the base user slot is still absent.
    McpInsertUserTombstone {
        name: McpServerName,
        expected_user_entry: Option<McpLayerEntry>,
    },
    /// Add ordered permission rules to one exact user-owned target.
    Rules {
        target: UserConfigTarget,
        changes: Vec<RuleMutation>,
    },
    /// Remove a predicate, regardless of its stored effect, from one target.
    RevokeRule {
        target: UserConfigTarget,
        id: RuleId,
    },
}

impl UserConfigMutation {
    /// Reject a mutation whose policy cannot be represented in config.
    ///
    /// This runs before the adapter acquires a lock or reads the file. A
    /// duplicate status segment is not an ordering instruction: it is an
    /// ambiguous configuration and therefore fails as a whole.
    pub fn validate(&self) -> Result<(), UserConfigError> {
        match self {
            Self::Root { .. } => Ok(()),
            Self::ManagedSet { target, key, value } => {
                validate_managed_target_key(target, key.clone())?;
                if !managed_value_matches_key(key.clone(), value) {
                    return Err(UserConfigError::new(format!(
                        "managed configuration key `{key}` received a value of the wrong type"
                    )));
                }
                let canonical = value.to_toml();
                let reparsed = ManagedConfigValue::parse(key.clone(), &canonical).map_err(|error| {
                    UserConfigError::new(format!(
                        "managed configuration key `{key}` cannot store its typed value: {error}"
                    ))
                })?;
                if &reparsed != value {
                    return Err(UserConfigError::new(format!(
                        "managed configuration key `{key}` cannot store its typed value without loss"
                    )));
                }
                Ok(())
            }
            Self::ManagedUnset { target, key } => validate_managed_target_key(target, key.clone()),
            Self::Statusline { segments } => {
                validate_status_segment_names(segments).map_err(UserConfigError::new)
            }
            Self::McpSet {
                expected_user_entry,
                server,
                ..
            } => {
                if let Some(McpLayerEntry::Server(expected) | McpLayerEntry::Disabled(expected)) =
                    expected_user_entry
                {
                    expected
                        .validate()
                        .map_err(|error| UserConfigError::new(error.to_string()))?;
                }
                server
                    .validate()
                    .map_err(|error| UserConfigError::new(error.to_string()))
            }
            Self::McpDeleteUserEntry {
                expected_entry: McpLayerEntry::Server(server) | McpLayerEntry::Disabled(server),
                ..
            } => server
                .validate()
                .map_err(|error| UserConfigError::new(error.to_string())),
            Self::McpInsertUserTombstone {
                expected_user_entry: Some(_),
                ..
            } => Err(UserConfigError::new(
                "an MCP tombstone insertion must compare against an absent user entry",
            )),
            Self::ModelAndThinking { .. }
            | Self::McpDeleteUserEntry {
                expected_entry: McpLayerEntry::Tombstone,
                ..
            }
            | Self::McpInsertUserTombstone {
                expected_user_entry: None,
                ..
            }
            | Self::Rules { .. } => Ok(()),
            Self::RevokeRule { id, .. } if !id.is_known() => Err(UserConfigError::new(format!(
                "rule ID `{id}` uses an unsupported grammar generation"
            ))),
            Self::RevokeRule { .. } => Ok(()),
        }
    }

    /// The exact user-owned authority this mutation addresses.
    pub fn target(&self) -> UserConfigTarget {
        match self {
            Self::Root { target, .. }
            | Self::ManagedSet { target, .. }
            | Self::ManagedUnset { target, .. }
            | Self::McpSet { target, .. }
            | Self::McpDeleteUserEntry { target, .. }
            | Self::Rules { target, .. }
            | Self::RevokeRule { target, .. } => target.clone(),
            Self::ModelAndThinking { .. }
            | Self::Statusline { .. }
            | Self::McpInsertUserTombstone { .. } => UserConfigTarget::Base,
        }
    }

    /// Canonical facts returned after this mutation succeeds.
    pub fn facts(&self) -> UserConfigMutationFacts {
        match self {
            Self::Root { path, access, .. } => UserConfigMutationFacts::Root {
                path: path.clone(),
                access: *access,
            },
            Self::ManagedSet { key, value, .. } => UserConfigMutationFacts::Set {
                key: key.clone(),
                value: value.clone(),
            },
            Self::ManagedUnset { key, .. } => UserConfigMutationFacts::Unset { key: key.clone() },
            Self::ModelAndThinking { model, thinking } => {
                UserConfigMutationFacts::ModelAndThinking {
                    model: model.clone(),
                    thinking: *thinking,
                }
            }
            Self::Statusline { segments } => UserConfigMutationFacts::Statusline {
                segments: segments.clone(),
            },
            Self::McpSet { name, server, .. } => UserConfigMutationFacts::McpSet {
                name: name.clone(),
                server: server.clone(),
            },
            Self::McpDeleteUserEntry {
                name,
                expected_entry,
                ..
            } => UserConfigMutationFacts::McpDeleteUserEntry {
                name: name.clone(),
                removed: expected_entry.clone(),
            },
            Self::McpInsertUserTombstone { name, .. } => {
                UserConfigMutationFacts::McpInsertUserTombstone { name: name.clone() }
            }
            Self::Rules { changes, .. } => UserConfigMutationFacts::Batch {
                changes: changes.clone(),
            },
            Self::RevokeRule { id, .. } => UserConfigMutationFacts::Revoke { id: id.clone() },
        }
    }
}

fn validate_managed_target_key(
    target: &UserConfigTarget,
    key: ManagedConfigKey,
) -> Result<(), UserConfigError> {
    if key.available_in(target) {
        return Ok(());
    }
    let target = match target {
        UserConfigTarget::Base => "base user configuration".to_owned(),
        UserConfigTarget::Profile(profile) => format!("user profile `{profile}`"),
    };
    Err(UserConfigError::new(format!(
        "managed configuration key `{key}` is not available for {target}"
    )))
}

fn managed_value_matches_key(key: ManagedConfigKey, value: &ManagedConfigValue) -> bool {
    matches!(
        (key, value),
        (ManagedConfigKey::Roots, ManagedConfigValue::Roots(_))
            | (
                ManagedConfigKey::ExecPaths,
                ManagedConfigValue::ExecPaths(_)
            )
            | (
                ManagedConfigKey::AuthHeaders(_),
                ManagedConfigValue::AuthHeaders(_)
            )
            | (
                ManagedConfigKey::AuthCredentialCommand(_),
                ManagedConfigValue::AuthCredentialCommand(_)
            )
            | (ManagedConfigKey::Model, ManagedConfigValue::Model(_))
            | (ManagedConfigKey::Thinking, ManagedConfigValue::Thinking(_))
            | (ManagedConfigKey::Mode, ManagedConfigValue::Mode(_))
            | (ManagedConfigKey::Sandbox, ManagedConfigValue::Sandbox(_))
            | (ManagedConfigKey::ExecEnv, ManagedConfigValue::ExecEnv(_))
            | (
                ManagedConfigKey::ExecEnvAllowlist,
                ManagedConfigValue::ExecEnvAllowlist(_)
            )
            | (ManagedConfigKey::Network, ManagedConfigValue::Network(_))
            | (ManagedConfigKey::Budget, ManagedConfigValue::Budget(_))
            | (ManagedConfigKey::Bare, ManagedConfigValue::Bare(_))
            | (
                ManagedConfigKey::PermissionsJudge,
                ManagedConfigValue::PermissionsJudge(_)
            )
            | (
                ManagedConfigKey::StatuslineSegments,
                ManagedConfigValue::StatuslineSegments(_)
            )
            | (
                ManagedConfigKey::SessionDir | ManagedConfigKey::ArtifactDir,
                ManagedConfigValue::Path(_)
            )
    )
}

/// Canonical description of the successfully requested user-config change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserConfigMutationFacts {
    Root {
        path: std::path::PathBuf,
        access: Option<crate::permit::RootAccess>,
    },
    Set {
        key: ManagedConfigKey,
        value: ManagedConfigValue,
    },
    Unset {
        key: ManagedConfigKey,
    },
    ModelAndThinking {
        model: String,
        thinking: ThinkingLevel,
    },
    Statusline {
        segments: Vec<String>,
    },
    McpSet {
        name: McpServerName,
        server: McpServerConfig,
    },
    McpDeleteUserEntry {
        name: McpServerName,
        removed: McpLayerEntry,
    },
    McpInsertUserTombstone {
        name: McpServerName,
    },
    Batch {
        changes: Vec<RuleMutation>,
    },
    Revoke {
        id: RuleId,
    },
    /// Rule-specific before/after facts captured by the store while its
    /// existing user-document write lock is held.
    RuleTransaction {
        request: Box<UserConfigMutationFacts>,
        before: RuleSnapshot,
        after: RuleSnapshot,
    },
}

/// Canonical facts for a persistent permission-rule mutation.
pub type RuleMutationFacts = UserConfigMutationFacts;

/// Which permission list owns a managed rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedRuleEffect {
    Allow,
    Deny,
}

/// One typed rule addition within an atomic user-config transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleMutation {
    pub effect: ManagedRuleEffect,
    pub rule: Rule,
}

/// A failed user-config mutation. Callers must keep their previous in-memory
/// authority whenever this is returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserConfigError {
    message: String,
    conflict: bool,
}

impl UserConfigError {
    pub fn conflict() -> Self {
        Self {
            message: "config revision conflict; read a fresh snapshot".into(),
            conflict: true,
        }
    }

    pub fn is_conflict(&self) -> bool {
        self.conflict
    }

    /// Preserve the boundary's actionable cause without exposing an outer
    /// filesystem error type through the core-owned port.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            conflict: false,
        }
    }
}

impl std::fmt::Display for UserConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for UserConfigError {}

/// What became authoritative after a successful user-config publication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserConfigPublication {
    /// The requested state was already authoritative; no replace was attempted.
    NotPublished,
    /// The new config was published and its directory entry was synced.
    PublishedDurably,
    /// The new config is authoritative, but its final durability barrier
    /// failed. The caller must surface `diagnostic` and must not retry.
    PublishedDurabilityUncertain { diagnostic: String },
}

/// The final state of one successful user-config transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserConfigMutationResult {
    pub target: UserConfigTarget,
    pub facts: UserConfigMutationFacts,
    pub changed: bool,
    pub publication: UserConfigPublication,
}

impl UserConfigMutationResult {
    /// The requested rule mutation and its atomic stored snapshots.
    pub fn rule_transaction(
        &self,
    ) -> Option<(&UserConfigMutationFacts, &RuleSnapshot, &RuleSnapshot)> {
        match &self.facts {
            UserConfigMutationFacts::RuleTransaction {
                request,
                before,
                after,
            } => Some((request, before, after)),
            _ => None,
        }
    }
}

/// The closed managed projection of one stored user-config authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserConfigSnapshot {
    pub target: UserConfigTarget,
    pub values: Vec<(ManagedConfigKey, Option<ManagedConfigValue>)>,
}

/// One adapter-owned whole-document edit prepared from the current authority.
///
/// The CLI may discover only the candidate path. The adapter retains the
/// compare-and-publish state and consumes the handle exactly once.
pub trait UserConfigEditHandle: Any + Send {
    fn candidate_path(&self) -> &Path;

    #[doc(hidden)]
    fn into_any(self: Box<Self>) -> Box<dyn Any + Send>;
}

/// The final state of one successful whole-document edit transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserConfigEditResult {
    pub changed: bool,
    pub publication: UserConfigPublication,
    /// A post-publication candidate cleanup failure cannot make the old
    /// authority current again. Report it without falsifying publication.
    pub candidate_cleanup_warning: Option<String>,
}

/// The exact stored rule lists in one user-owned target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleSnapshot {
    pub target: UserConfigTarget,
    pub permissions_present: bool,
    pub allow: Vec<Rule>,
    pub deny: Vec<Rule>,
}

/// An adapter-owned, in-memory preview of one complete setup transaction.
/// Consumed once after the host confirms the exact displayed candidate.
pub trait UserConfigBatch: Any + Send {
    fn before(&self) -> &str;
    fn candidate(&self) -> &str;
    #[doc(hidden)]
    fn into_any(self: Box<Self>) -> Box<dyn Any + Send>;
}

/// The narrow consumer port for stored user configuration.
pub trait UserConfigStore: Send + Sync {
    /// Read one stored user authority without resolving inherited values.
    fn snapshot(&self, target: UserConfigTarget) -> Result<UserConfigSnapshot, UserConfigError>;

    /// Read the ordered stored rules without resolving inherited authority.
    fn rule_snapshot(&self, target: UserConfigTarget) -> Result<RuleSnapshot, UserConfigError>;

    /// Apply one complete mutation. `Err` guarantees that publication did not
    /// occur and the previous config remains authoritative; an unchanged
    /// success reports [`UserConfigPublication::NotPublished`].
    fn mutate(
        &self,
        mutation: UserConfigMutation,
    ) -> Result<UserConfigMutationResult, UserConfigError>;

    /// Validate and publish 1..64 managed operations to one exact target.
    fn managed_batch(
        &self,
        _changes: Vec<UserConfigMutation>,
    ) -> Result<UserConfigEditResult, UserConfigError> {
        Err(UserConfigError::new("managed batches are unavailable"))
    }

    /// Capture an opaque target-bound revision with its stored projection.
    fn revision_snapshot(
        &self,
        _target: UserConfigTarget,
    ) -> Result<(UserConfigSnapshot, String), UserConfigError> {
        Err(UserConfigError::new("config revisions are unavailable"))
    }

    /// Compare and publish under the same cross-process lock as every writer.
    fn managed_batch_conditional(
        &self,
        _changes: Vec<UserConfigMutation>,
        _expected: &str,
    ) -> Result<UserConfigEditResult, UserConfigError> {
        Err(UserConfigError::new(
            "conditional config writes are unavailable",
        ))
    }

    /// Preview without creating lock, directory, temporary file, or candidate.
    fn prepare_batch(
        &self,
        _changes: Vec<UserConfigMutation>,
    ) -> Result<Box<dyn UserConfigBatch>, UserConfigError> {
        Err(UserConfigError::new(
            "prepared batches are not supported by this config store",
        ))
    }

    /// Compare and publish the exact preview once under the existing lock.
    fn publish_batch(
        &self,
        _batch: Box<dyn UserConfigBatch>,
    ) -> Result<UserConfigEditResult, UserConfigError> {
        Err(UserConfigError::new(
            "prepared batches are not supported by this config store",
        ))
    }

    /// Setup host rechecks policy/path identities while the publication lock is held.
    fn publish_batch_checked(
        &self,
        _batch: Box<dyn UserConfigBatch>,
        _check: &dyn Fn() -> Result<(), UserConfigError>,
    ) -> Result<UserConfigEditResult, UserConfigError> {
        Err(UserConfigError::new(
            "checked setup publication is unavailable",
        ))
    }

    /// Copy the exact current user document into a private sibling candidate.
    fn prepare_edit(&self) -> Result<Box<dyn UserConfigEditHandle>, UserConfigError> {
        Err(UserConfigError::new(
            "whole-document editing is not supported by this user config store",
        ))
    }

    /// Strictly validate and compare-and-publish one prepared candidate.
    fn publish_edit(
        &self,
        _edit: Box<dyn UserConfigEditHandle>,
    ) -> Result<UserConfigEditResult, UserConfigError> {
        Err(UserConfigError::new(
            "whole-document editing is not supported by this user config store",
        ))
    }

    /// Remove only the candidate named by one prepared edit handle.
    fn discard_edit(&self, _edit: Box<dyn UserConfigEditHandle>) -> Result<(), UserConfigError> {
        Err(UserConfigError::new(
            "whole-document editing is not supported by this user config store",
        ))
    }
}
