//! Pure interactive policy-control state and application coordination.
//!
//! The presenter emits typed requests, while the application owner validates,
//! persists, resolves, and stages policy for the next generation boundary.

use std::sync::{Arc, RwLock};

use ego_core::config::rule_management::{
    EffectiveRule, RememberPlan, RuleExplanation, RuleMutationComparison,
};
use ego_core::config::{
    ManagedRuleEffect, Mode, RuleMutation, RuleSnapshot, Ruleset, UserConfigStore, UserConfigTarget,
};
use ego_core::permit::rule::RuleId;
use ego_core::permit::{CanonPath, HumanApprover, PolicyEngine, PreparedCall, ProfileRoot};
use ego_core::turn::{
    GenerationPolicyCapture, GenerationPolicySnapshot as CapturedGenerationPolicySnapshot,
    GenerationPolicySource,
};

pub use crate::rules::{RememberPreview, RememberRequest, RuleMutationReceipt};
use crate::rules::{RuleApplication, RuleApplicationError, RuleChange, RuleReadAuthority};

const MODE_OPTIONS: [Mode; 5] = [
    Mode::Plan,
    Mode::Default,
    Mode::Auto,
    Mode::Edits,
    Mode::Yolo,
];

/// A read-only operation over the generation's admitted permission rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionInput {
    List,
    Revoke(RuleId),
    Remember(RememberRequest),
    ConfirmRemember(RememberPreview),
    Explain {
        call: PreparedCall,
        workspace: CanonPath,
    },
}

/// One closed request emitted by the interactive policy presenter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyControlRequest {
    SetMode(Mode),
    Rule(RuleMutation),
    Permission(PermissionInput),
}

/// The live seat's narrow bridge to the one CLI-owned atomic rule writer.
pub trait PermissionRuleApplication: Send + Sync {
    fn remember_allow(&self, plan: RememberPlan) -> Result<String, PolicyControlError>;
}

/// Presenter-owned modal state for the interactive policy controls.
#[derive(Debug, Default)]
pub struct PolicyControlController;

impl PolicyControlController {
    pub fn new() -> Self {
        Self
    }

    pub fn mode_options(&self) -> [Mode; 5] {
        MODE_OPTIONS
    }

    /// Stage the selected mode. Choosing an explicit mode is already an
    /// intentional local control action, including the relaxed `yolo` mode.
    pub fn select_mode(&mut self, mode: Mode) -> Option<PolicyControlRequest> {
        Some(PolicyControlRequest::SetMode(mode))
    }
}

/// One immutable policy captured by a generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationPolicySnapshot {
    pub mode: Mode,
    pub ruleset: Ruleset,
}

#[derive(Debug)]
struct GenerationPolicyState {
    active: GenerationPolicySnapshot,
    staged: Option<GenerationPolicySnapshot>,
}

/// Whether an attempted staging operation changed the next-boundary state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicyStageOutcome {
    pub changed: bool,
}

/// Shared generation policy with one active and at most one staged snapshot.
#[derive(Debug, Clone)]
pub struct GenerationPolicySelection {
    state: Arc<RwLock<GenerationPolicyState>>,
}

impl GenerationPolicySelection {
    pub fn new(mode: Mode, ruleset: Ruleset) -> Self {
        Self {
            state: Arc::new(RwLock::new(GenerationPolicyState {
                active: GenerationPolicySnapshot { mode, ruleset },
                staged: None,
            })),
        }
    }

    pub fn snapshot(&self) -> Result<GenerationPolicySnapshot, PolicyControlError> {
        Ok(self.read()?.active.clone())
    }

    pub fn staged_snapshot(&self) -> Result<Option<GenerationPolicySnapshot>, PolicyControlError> {
        Ok(self.read()?.staged.clone())
    }

    /// Commit the pending selection, if any, and return that exact active snapshot.
    pub fn capture(&self) -> Result<GenerationPolicySnapshot, PolicyControlError> {
        let mut state = self.write()?;
        if let Some(staged) = state.staged.take() {
            state.active = staged;
        }
        Ok(state.active.clone())
    }

    pub(crate) fn next_snapshot(&self) -> Result<GenerationPolicySnapshot, PolicyControlError> {
        let state = self.read()?;
        Ok(state.staged.as_ref().unwrap_or(&state.active).clone())
    }

    fn stage(
        &self,
        mode: Mode,
        ruleset: Ruleset,
    ) -> Result<PolicyStageOutcome, PolicyControlError> {
        let mut state = self.write()?;
        Ok(replace_staged(
            &mut state,
            GenerationPolicySnapshot { mode, ruleset },
        ))
    }

    fn stage_mode(&self, mode: Mode) -> Result<PolicyStageOutcome, PolicyControlError> {
        let mut state = self.write()?;
        let ruleset = state
            .staged
            .as_ref()
            .unwrap_or(&state.active)
            .ruleset
            .clone();
        Ok(replace_staged(
            &mut state,
            GenerationPolicySnapshot { mode, ruleset },
        ))
    }

    fn read(
        &self,
    ) -> Result<std::sync::RwLockReadGuard<'_, GenerationPolicyState>, PolicyControlError> {
        self.state
            .read()
            .map_err(|_| PolicyControlError::Synchronization {
                diagnostic: "the generation policy selection lock is poisoned".into(),
            })
    }

    fn write(
        &self,
    ) -> Result<std::sync::RwLockWriteGuard<'_, GenerationPolicyState>, PolicyControlError> {
        self.state
            .write()
            .map_err(|_| PolicyControlError::Synchronization {
                diagnostic: "the generation policy selection lock is poisoned".into(),
            })
    }
}

/// CLI adapter from the shared selection to core's generation-boundary policy port.
pub(crate) struct CliGenerationPolicySource {
    selection: GenerationPolicySelection,
    workspace: CanonPath,
    roots: Vec<ProfileRoot>,
    human: Option<Arc<dyn HumanApprover>>,
}

impl CliGenerationPolicySource {
    pub(crate) fn new(
        selection: GenerationPolicySelection,
        workspace: CanonPath,
        human: Option<Arc<dyn HumanApprover>>,
    ) -> Self {
        Self {
            selection,
            workspace,
            roots: Vec::new(),
            human,
        }
    }

    /// The same source, reaching the profile's admitted roots too.
    pub(crate) fn with_profile_roots(mut self, roots: Vec<ProfileRoot>) -> Self {
        self.roots = roots;
        self
    }

    fn engine(&self, snapshot: GenerationPolicySnapshot) -> Arc<PolicyEngine> {
        Arc::new(
            match self.human.as_ref() {
                Some(human) => PolicyEngine::with_human(
                    self.workspace.clone(),
                    snapshot.mode,
                    snapshot.ruleset,
                    Arc::clone(human),
                ),
                None => PolicyEngine::new(self.workspace.clone(), snapshot.mode, snapshot.ruleset),
            }
            .with_profile_roots(self.roots.clone()),
        )
    }
}

impl GenerationPolicySource for CliGenerationPolicySource {
    fn current(&self) -> Result<Arc<PolicyEngine>, String> {
        self.selection
            .snapshot()
            .map(|snapshot| self.engine(snapshot))
            .map_err(|error| error.to_string())
    }

    fn capture(&self) -> Result<GenerationPolicyCapture, String> {
        let snapshot = self
            .selection
            .capture()
            .map_err(|error| error.to_string())?;
        GenerationPolicyCapture::with_profile_roots(
            self.workspace.clone(),
            self.roots.clone(),
            snapshot.mode,
            snapshot.ruleset,
            self.human.clone(),
        )
        .map_err(|error| error.to_string())
    }

    fn restore(
        &self,
        snapshot: Arc<CapturedGenerationPolicySnapshot>,
    ) -> Result<GenerationPolicyCapture, String> {
        GenerationPolicyCapture::restore(snapshot, self.human.clone())
            .map_err(|error| error.to_string())
    }
}

fn replace_staged(
    state: &mut GenerationPolicyState,
    requested: GenerationPolicySnapshot,
) -> PolicyStageOutcome {
    if state.staged.as_ref() == Some(&requested)
        || (state.staged.is_none() && state.active == requested)
    {
        return PolicyStageOutcome { changed: false };
    }

    state.staged = (state.active != requested).then_some(requested);
    PolicyStageOutcome { changed: true }
}

/// Pure preflight required before the session can enter automatic mode.
pub trait PreflightPort: Send + Sync {
    fn validate_auto(&self) -> Result<(), PolicyControlError>;
}

/// Re-resolve policy from the frozen source set admitted for this session.
pub trait FrozenPolicyResolver: Send + Sync {
    fn resolve_rule_transaction(
        &self,
        before: &RuleSnapshot,
        after: &RuleSnapshot,
        affected: &[RuleMutation],
    ) -> Result<FrozenRuleResolution, PolicyControlError>;
}

/// Frozen policy facts resolved after one published rule transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrozenRuleResolution {
    pub next: GenerationPolicySnapshot,
    pub comparisons: Vec<RuleMutationComparison>,
}

/// Result of a read-only permission-control request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionOutcome {
    List(Vec<EffectiveRule>),
    Explain(RuleExplanation),
}

/// Result that later terminal wiring can render without re-reading authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyControlOutcome {
    Mode {
        staging: PolicyStageOutcome,
    },
    Rule {
        receipt: RuleMutationReceipt,
        staging: PolicyStageOutcome,
    },
    RememberPreview(RememberPreview),
    Permission(PermissionOutcome),
}

/// Errors owned by the interactive policy-control application boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyControlError {
    JudgeUnavailable { diagnostic: String },
    Store { diagnostic: String },
    Resolution { diagnostic: String },
    Synchronization { diagnostic: String },
    PublicationContract { diagnostic: String },
    RuleManagement { diagnostic: String },
}

impl std::fmt::Display for PolicyControlError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (context, diagnostic) = match self {
            Self::JudgeUnavailable { diagnostic } => {
                ("automatic-mode preflight failed", diagnostic)
            }
            Self::Store { diagnostic } => ("user configuration mutation failed", diagnostic),
            Self::Resolution { diagnostic } => ("policy resolution failed", diagnostic),
            Self::Synchronization { diagnostic } => ("policy synchronization failed", diagnostic),
            Self::PublicationContract { diagnostic } => {
                ("user configuration publication contract failed", diagnostic)
            }
            Self::RuleManagement { diagnostic } => ("rule management failed", diagnostic),
        };
        write!(formatter, "{context}: {diagnostic}")
    }
}

impl std::error::Error for PolicyControlError {}

/// Application owner for policy preflight, persistence, resolution, and staging.
pub struct PolicyApplicationOwner {
    selection: GenerationPolicySelection,
    rules: RuleApplication,
    preflight: Arc<dyn PreflightPort>,
    resolver: Arc<dyn FrozenPolicyResolver>,
}

impl PolicyApplicationOwner {
    pub fn new(
        selection: GenerationPolicySelection,
        store: Arc<dyn UserConfigStore>,
        preflight: Arc<dyn PreflightPort>,
        resolver: Arc<dyn FrozenPolicyResolver>,
    ) -> Self {
        Self {
            selection,
            rules: RuleApplication::new(store, UserConfigTarget::Base),
            preflight,
            resolver,
        }
    }

    pub fn apply(
        &self,
        request: PolicyControlRequest,
    ) -> Result<PolicyControlOutcome, PolicyControlError> {
        match request {
            PolicyControlRequest::SetMode(mode) => self.apply_mode(mode),
            PolicyControlRequest::Rule(mutation) => self.apply_rule(mutation),
            PolicyControlRequest::Permission(input) => self.apply_permission(input),
        }
    }

    /// Inspect admitted rules without reading mutable config a second time.
    pub fn explain(&self, input: PermissionInput) -> Result<PermissionOutcome, PolicyControlError> {
        let ruleset = self.selection.snapshot()?.ruleset;
        match input {
            PermissionInput::List => self
                .rules
                .list(RuleReadAuthority::Admitted(&ruleset))
                .map(PermissionOutcome::List)
                .map_err(map_rule_error),
            PermissionInput::Explain { call, workspace } => self
                .rules
                .explain(&call, &workspace, RuleReadAuthority::Admitted(&ruleset))
                .map(PermissionOutcome::Explain)
                .map_err(map_rule_error),
            PermissionInput::Revoke(_)
            | PermissionInput::Remember(_)
            | PermissionInput::ConfirmRemember(_) => Err(PolicyControlError::RuleManagement {
                diagnostic: "a permission mutation must be applied, not inspected".into(),
            }),
        }
    }

    pub fn preview_remember(
        &self,
        request: RememberRequest,
    ) -> Result<RememberPreview, PolicyControlError> {
        self.rules.preview_remember(request).map_err(map_rule_error)
    }

    fn apply_mode(&self, mode: Mode) -> Result<PolicyControlOutcome, PolicyControlError> {
        if mode == Mode::Auto {
            self.preflight.validate_auto()?;
        }
        Ok(PolicyControlOutcome::Mode {
            staging: self.selection.stage_mode(mode)?,
        })
    }

    fn apply_rule(
        &self,
        mutation: RuleMutation,
    ) -> Result<PolicyControlOutcome, PolicyControlError> {
        let change = match mutation.effect {
            ego_core::config::ManagedRuleEffect::Allow => self.rules.allow(mutation.rule),
            ego_core::config::ManagedRuleEffect::Deny => self.rules.deny(mutation.rule),
        }
        .map_err(map_rule_error)?;
        let (receipt, staging) = self.finish_rule_change(change)?;
        Ok(PolicyControlOutcome::Rule { receipt, staging })
    }

    fn apply_permission(
        &self,
        input: PermissionInput,
    ) -> Result<PolicyControlOutcome, PolicyControlError> {
        match input {
            read @ (PermissionInput::List | PermissionInput::Explain { .. }) => {
                self.explain(read).map(PolicyControlOutcome::Permission)
            }
            PermissionInput::Revoke(id) => {
                let change = self.rules.revoke(id).map_err(map_rule_error)?;
                let (receipt, staging) = self.finish_rule_change(change)?;
                Ok(PolicyControlOutcome::Rule { receipt, staging })
            }
            PermissionInput::Remember(request) => self
                .preview_remember(request)
                .map(PolicyControlOutcome::RememberPreview),
            PermissionInput::ConfirmRemember(preview) => {
                let change = self
                    .rules
                    .confirm_remember(preview)
                    .map_err(map_rule_error)?;
                let (receipt, staging) = self.finish_rule_change(change)?;
                Ok(PolicyControlOutcome::Rule { receipt, staging })
            }
        }
    }

    fn finish_rule_change(
        &self,
        change: RuleChange,
    ) -> Result<(RuleMutationReceipt, PolicyStageOutcome), PolicyControlError> {
        if !change.changed() {
            return change
                .into_unchanged_receipt()
                .map(|receipt| (receipt, PolicyStageOutcome { changed: false }))
                .map_err(map_rule_error);
        }

        let resolution = {
            let (before, after) = change.transaction();
            self.resolver
                .resolve_rule_transaction(before, after, change.affected())?
        };
        let receipt = change
            .into_receipt(resolution.comparisons)
            .map_err(map_rule_error)?;
        let staging = self
            .selection
            .stage(resolution.next.mode, resolution.next.ruleset)?;
        Ok((receipt, staging))
    }
}

impl PermissionRuleApplication for PolicyApplicationOwner {
    fn remember_allow(&self, plan: RememberPlan) -> Result<String, PolicyControlError> {
        let rules = plan
            .candidates
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let preview = RememberPreview::presented(ManagedRuleEffect::Allow, plan);
        let PolicyControlOutcome::Rule { receipt, .. } =
            self.apply_permission(PermissionInput::ConfirmRemember(preview))?
        else {
            return Err(PolicyControlError::RuleManagement {
                diagnostic: "remembering a presented call returned a non-rule outcome".into(),
            });
        };
        let mut sentence = format!("Remembered: you will not be asked again for {rules}.");
        if let Some(warning) = crate::rules::publication_warning_text(&receipt.publication()) {
            sentence.push(' ');
            sentence.push_str(&warning);
        }
        Ok(sentence)
    }
}

fn map_rule_error(error: RuleApplicationError) -> PolicyControlError {
    match error {
        RuleApplicationError::Store(diagnostic) => PolicyControlError::Store { diagnostic },
        RuleApplicationError::Management(error) => PolicyControlError::RuleManagement {
            diagnostic: error.to_string(),
        },
        RuleApplicationError::Contract(diagnostic) => PolicyControlError::PublicationContract {
            diagnostic: diagnostic.into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use ego_core::event::id::ItemId;
    use ego_core::permit::{
        Change, Classification, Op, PermissionDecision, PermissionRequest, Resolution,
    };
    use futures::future::BoxFuture;

    #[derive(Debug)]
    struct AllowingHumanApprover;

    impl HumanApprover for AllowingHumanApprover {
        fn decide<'a>(
            &'a self,
            _item: ItemId,
            _request: &'a PermissionRequest,
        ) -> BoxFuture<'a, PermissionDecision> {
            Box::pin(std::future::ready(PermissionDecision::Allow))
        }
    }

    fn uncovered_write(workspace: &str) -> PreparedCall {
        PreparedCall::new(
            "edit",
            Op::Write {
                path: CanonPath::new(format!("{workspace}/uncovered.txt")),
                change: Change::Whole(Vec::new()),
            },
        )
    }

    #[test]
    fn poisoned_selection_reads_return_synchronization_errors() {
        let selection = GenerationPolicySelection::new(Mode::Default, Ruleset::default());
        let poisoned = selection.clone();
        std::thread::spawn(move || {
            let _guard = poisoned.state.write().unwrap();
            panic!("deliberately poison selection lock");
        })
        .join()
        .expect_err("poisoning thread must panic");

        assert!(matches!(
            selection.snapshot(),
            Err(PolicyControlError::Synchronization { .. })
        ));
        assert!(matches!(
            selection.staged_snapshot(),
            Err(PolicyControlError::Synchronization { .. })
        ));
    }

    #[test]
    fn selection_keeps_active_snapshot_until_boundary_and_plan_is_idempotent() {
        let selection = GenerationPolicySelection::new(Mode::Default, Ruleset::default());
        let active = selection.snapshot().unwrap();
        assert!(
            selection
                .stage(Mode::Plan, Ruleset::default())
                .unwrap()
                .changed
        );
        assert!(
            !selection
                .stage(Mode::Plan, Ruleset::default())
                .unwrap()
                .changed
        );
        assert_eq!(selection.snapshot().unwrap(), active);
        assert_eq!(
            selection.staged_snapshot().unwrap().unwrap().mode,
            Mode::Plan
        );
        let captured = selection.capture().unwrap();
        assert_eq!(captured.mode, Mode::Plan);
        assert_eq!(selection.snapshot().unwrap(), captured);
    }

    #[test]
    fn cli_policy_source_restores_exact_snapshot_semantics_with_only_its_live_human() {
        let snapshot_human = Arc::new(AllowingHumanApprover);
        let original = ego_core::turn::GenerationPolicyCapture::new(
            CanonPath::new("/captured-workspace"),
            Mode::Default,
            Ruleset::default(),
            Some(snapshot_human.clone()),
        )
        .expect("valid original capture");
        let snapshot = original.snapshot();
        let digest = snapshot.digest();
        drop(original);

        let configured_human = Arc::new(AllowingHumanApprover);
        let selection = GenerationPolicySelection::new(Mode::Default, Ruleset::default());
        let source = CliGenerationPolicySource::new(
            selection.clone(),
            CanonPath::new("/live-workspace"),
            Some(configured_human.clone()),
        );
        assert!(selection.stage_mode(Mode::Yolo).unwrap().changed);

        let current: Arc<PolicyEngine> =
            GenerationPolicySource::current(&source).expect("active policy");
        let Classification::Ask(pending) = current.classify(uncovered_write("/live-workspace"))
        else {
            panic!("current must read the uncommitted default policy");
        };
        assert!(matches!(
            futures::executor::block_on(current.settle(pending)),
            Resolution::Ask(_)
        ));
        assert_eq!(
            selection.staged_snapshot().unwrap().unwrap().mode,
            Mode::Yolo,
            "current must not commit the staged policy"
        );
        drop(current);

        let captured: ego_core::turn::GenerationPolicyCapture =
            GenerationPolicySource::capture(&source).expect("boundary capture");
        let Classification::Ask(pending) = captured
            .policy()
            .classify(uncovered_write("/live-workspace"))
        else {
            panic!("yolo reaches mode settlement for the uncovered write");
        };
        assert!(matches!(
            futures::executor::block_on(captured.policy().settle(pending)),
            Resolution::Settled(Ok(_))
        ));
        drop(captured);

        let restored: ego_core::turn::GenerationPolicyCapture =
            GenerationPolicySource::restore(&source, Arc::clone(&snapshot))
                .expect("captured semantics restore");
        assert!(Arc::ptr_eq(&restored.snapshot(), &snapshot));
        assert_eq!(restored.snapshot().digest(), digest);
        assert_eq!(
            restored
                .snapshot()
                .encode_confidential()
                .expose_secret_bytes(),
            snapshot.encode_confidential().expose_secret_bytes()
        );
        let restored_policy = restored.policy();
        let Classification::Ask(pending) =
            restored_policy.classify(uncovered_write("/captured-workspace"))
        else {
            panic!("restore must ignore the now-active yolo selection");
        };
        let Resolution::Ask(_pending) =
            futures::executor::block_on(restored_policy.settle(pending))
        else {
            panic!("the restored default mode must reach the human party");
        };
        assert_eq!(Arc::strong_count(&configured_human), 3);
        assert_eq!(Arc::strong_count(&snapshot_human), 1);
        drop(restored_policy);
        drop(restored);
        assert_eq!(Arc::strong_count(&configured_human), 2);
    }
}
