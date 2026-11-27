//! Pure interactive policy-control state and application coordination.
//!
//! The presenter emits typed requests, while the application owner validates,
//! persists, resolves, and stages policy for the next generation boundary.

use std::sync::{Arc, Mutex, MutexGuard, RwLock};

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

const MODE_OPTIONS: [Mode; 5] = Mode::SELECTABLE;

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
    CycleMode,
    Rule(RuleMutation),
    Permission(PermissionInput),
}

/// The live seat's narrow bridge to the one CLI-owned atomic rule writer.
pub trait PermissionRuleApplication: Send + Sync {
    fn remember_allow(&self, plan: RememberPlan) -> Result<String, PolicyControlError>;
    /// Keep what the plan names refused: a deny rule, so it is not asked again.
    fn remember_deny(&self, plan: RememberPlan) -> Result<String, PolicyControlError>;
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
    published_rules: Vec<RuleSnapshot>,
    invalidated: Option<String>,
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
    rule_publication: Arc<Mutex<()>>,
}

impl GenerationPolicySelection {
    pub fn new(mode: Mode, ruleset: Ruleset) -> Self {
        Self {
            rule_publication: Arc::new(Mutex::new(())),
            state: Arc::new(RwLock::new(GenerationPolicyState {
                active: GenerationPolicySnapshot { mode, ruleset },
                staged: None,
                published_rules: Vec::new(),
                invalidated: None,
            })),
        }
    }

    /// Keep store publication, frozen resolution and staging in one order.
    /// Separate from the state lock: resolvers read the selected snapshot.
    pub(crate) fn lock_rule_publication(&self) -> Result<MutexGuard<'_, ()>, PolicyControlError> {
        let guard = self
            .rule_publication
            .lock()
            .map_err(|_| rule_publication_poisoned())?;
        drop(self.read()?);
        Ok(guard)
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

    pub(crate) fn published_rule_snapshots(&self) -> Result<Vec<RuleSnapshot>, PolicyControlError> {
        Ok(self.read()?.published_rules.clone())
    }

    fn stage_rules(
        &self,
        mut ruleset: Ruleset,
        preserve_denies: bool,
        published: RuleSnapshot,
    ) -> Result<PolicyStageOutcome, PolicyControlError> {
        let mut state = self.write()?;
        let selected = state.staged.as_ref().unwrap_or(&state.active);
        let mode = selected.mode;
        // An additive refusal also preserves denies published after resolution
        // began; mode and deny origins join the new rules under this one lock.
        if preserve_denies {
            for deny in &selected.ruleset.deny {
                for origin in deny.origins().iter() {
                    Ruleset::merge(&mut ruleset.deny, deny.rule().clone(), origin);
                }
            }
        }
        state
            .published_rules
            .retain(|snapshot| snapshot.target != published.target);
        state.published_rules.push(published);
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
        if self.rule_publication.is_poisoned() {
            return Err(rule_publication_poisoned());
        }
        let state = self
            .state
            .read()
            .map_err(|_| PolicyControlError::Synchronization {
                diagnostic: "the generation policy selection lock is poisoned".into(),
            })?;
        if let Some(diagnostic) = &state.invalidated {
            return Err(PolicyControlError::Resolution {
                diagnostic: diagnostic.clone(),
            });
        }
        Ok(state)
    }

    fn write(
        &self,
    ) -> Result<std::sync::RwLockWriteGuard<'_, GenerationPolicyState>, PolicyControlError> {
        if self.rule_publication.is_poisoned() {
            return Err(rule_publication_poisoned());
        }
        let state = self
            .state
            .write()
            .map_err(|_| PolicyControlError::Synchronization {
                diagnostic: "the generation policy selection lock is poisoned".into(),
            })?;
        if let Some(diagnostic) = &state.invalidated {
            return Err(PolicyControlError::Resolution {
                diagnostic: diagnostic.clone(),
            });
        }
        Ok(state)
    }

    fn invalidate(&self, error: &PolicyControlError) {
        // A poisoned lock already refuses all policy access.
        if let Ok(mut state) = self.state.write() {
            state.invalidated.get_or_insert_with(|| {
                format!(
                    "published permission rules could not be applied; restart this session: {error}"
                )
            });
        }
    }
}

fn rule_publication_poisoned() -> PolicyControlError {
    PolicyControlError::Synchronization {
        diagnostic: "the rule publication lock is poisoned; restart this session".into(),
    }
}

/// CLI adapter from the shared selection to core's generation-boundary policy port.
pub(crate) struct CliGenerationPolicySource {
    selection: GenerationPolicySelection,
    workspace: CanonPath,
    roots: Vec<ProfileRoot>,
    read_perimeter: ego_core::permit::ReadPerimeter,
    root_ceilings: ego_core::permit::RootCeilings,
    human: Option<Arc<dyn HumanApprover>>,
    session_approvals: ego_core::permit::SessionApprovals,
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
            read_perimeter: ego_core::permit::ReadPerimeter::default(),
            root_ceilings: ego_core::permit::RootCeilings::default(),
            human,
            session_approvals: ego_core::permit::SessionApprovals::default(),
        }
    }

    /// The same source, reaching the profile's admitted roots too.
    pub(crate) fn with_profile_roots(mut self, roots: Vec<ProfileRoot>) -> Self {
        self.roots = roots;
        self
    }

    pub(crate) fn with_read_perimeter(
        mut self,
        confinement: &ego_core::permit::RunConfinement,
    ) -> Self {
        self.read_perimeter = ego_core::permit::ReadPerimeter::from_confinement(confinement);
        self
    }

    pub(crate) fn with_root_ceilings(
        mut self,
        root_ceilings: ego_core::permit::RootCeilings,
    ) -> Self {
        self.root_ceilings = root_ceilings;
        self
    }

    /// Configured and session roots as admitted at start, plus the repository's
    /// checkouts as they are now: a worktree made mid-session joins here.
    fn live_roots(&self) -> Vec<ProfileRoot> {
        let fresh = crate::run::profile_roots::same_repository(&self.workspace);
        let mut roots: Vec<_> = self
            .roots
            .iter()
            .filter(|root| root.derived().is_none())
            .cloned()
            .collect();
        let added: Vec<_> = fresh
            .into_iter()
            .filter(|root| !roots.contains(root))
            .collect();
        roots.extend(added);
        roots
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
            .with_session_approvals(self.session_approvals.clone())
            .with_profile_roots(self.live_roots())
            .with_read_perimeter(self.read_perimeter.clone())
            .with_root_ceilings(self.root_ceilings.clone()),
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
            self.live_roots(),
            snapshot.mode,
            snapshot.ruleset,
            self.human.clone(),
        )
        .and_then(|capture| capture.with_session_approvals(self.session_approvals.clone()))
        .and_then(|capture| capture.with_read_perimeter(self.read_perimeter.clone()))
        .and_then(|capture| capture.with_root_ceilings(self.root_ceilings.clone()))
        .map_err(|error| error.to_string())
    }

    fn restore(
        &self,
        snapshot: Arc<CapturedGenerationPolicySnapshot>,
    ) -> Result<GenerationPolicyCapture, String> {
        self.selection
            .snapshot()
            .map_err(|error| error.to_string())?;
        GenerationPolicyCapture::restore(snapshot, self.human.clone())
            .and_then(|capture| capture.with_session_approvals(self.session_approvals.clone()))
            .and_then(|capture| capture.with_root_ceilings(self.root_ceilings.clone()))
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
    /// The maximum mode admitted by this session's frozen authority.
    fn max_mode(&self) -> Result<Option<Mode>, PolicyControlError>;

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
    max_mode: Result<Option<Mode>, PolicyControlError>,
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
            max_mode: resolver.max_mode(),
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
            PolicyControlRequest::CycleMode => self.apply_mode(self.selected_mode()?.next()),
            PolicyControlRequest::Rule(mutation) => self.apply_rule(mutation),
            PolicyControlRequest::Permission(input) => self.apply_permission(input),
        }
    }

    /// Last accepted mode, including pending changes at the generation boundary.
    pub fn selected_mode(&self) -> Result<Mode, PolicyControlError> {
        Ok(self.selection.next_snapshot()?.mode)
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
        if let Some(maximum) = self.max_mode.as_ref().map_err(Clone::clone)?
            && !mode.within(*maximum)
        {
            return Err(PolicyControlError::Resolution {
                diagnostic: format!("mode {mode} exceeds system max_mode {maximum}"),
            });
        }
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
        let _publication = self.selection.lock_rule_publication()?;
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
                let _publication = self.selection.lock_rule_publication()?;
                let change = self.rules.revoke(id).map_err(map_rule_error)?;
                let (receipt, staging) = self.finish_rule_change(change)?;
                Ok(PolicyControlOutcome::Rule { receipt, staging })
            }
            PermissionInput::Remember(request) => self
                .preview_remember(request)
                .map(PolicyControlOutcome::RememberPreview),
            PermissionInput::ConfirmRemember(preview) => {
                let _publication = self.selection.lock_rule_publication()?;
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
        finish_rule_change(&self.selection, self.resolver.as_ref(), change)
    }
}

/// Stage one successfully published transaction using the session's frozen authority.
pub(crate) fn finish_rule_change(
    selection: &GenerationPolicySelection,
    resolver: &dyn FrozenPolicyResolver,
    change: RuleChange,
) -> Result<(RuleMutationReceipt, PolicyStageOutcome), PolicyControlError> {
    let result = (|| {
        if !change.changed() {
            return change
                .into_unchanged_receipt()
                .map(|receipt| (receipt, PolicyStageOutcome { changed: false }))
                .map_err(map_rule_error);
        }
        let (resolution, only_adds_denies, published) = {
            let (before, after) = change.transaction();
            let only_adds_denies = !change.affected().is_empty()
                && change.affected().iter().all(|mutation| {
                    mutation.effect == ManagedRuleEffect::Deny
                        && after.deny.contains(&mutation.rule)
                })
                && before.deny.iter().all(|rule| after.deny.contains(rule));
            (
                resolver.resolve_rule_transaction(before, after, change.affected())?,
                only_adds_denies,
                after.clone(),
            )
        };
        let receipt = change
            .into_receipt(resolution.comparisons)
            .map_err(map_rule_error)?;
        let staging =
            selection.stage_rules(resolution.next.ruleset, only_adds_denies, published)?;
        Ok((receipt, staging))
    })();
    if let Err(error) = &result {
        selection.invalidate(error);
    }
    result
}

impl PolicyApplicationOwner {
    fn remember(
        &self,
        effect: ManagedRuleEffect,
        plan: RememberPlan,
    ) -> Result<String, PolicyControlError> {
        let rules = plan
            .candidates
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let preview = RememberPreview::presented(effect, plan);
        let PolicyControlOutcome::Rule { receipt, .. } =
            self.apply_permission(PermissionInput::ConfirmRemember(preview))?
        else {
            return Err(PolicyControlError::RuleManagement {
                diagnostic: "remembering a presented call returned a non-rule outcome".into(),
            });
        };
        let mut sentence = match effect {
            ManagedRuleEffect::Allow => {
                format!("Remembered: you will not be asked again for {rules}.")
            }
            ManagedRuleEffect::Deny => {
                format!("Kept blocked: {rules} is refused and will not be asked again.")
            }
        };
        if let Some(warning) = crate::rules::publication_warning_text(&receipt.publication()) {
            sentence.push(' ');
            sentence.push_str(&warning);
        }
        Ok(sentence)
    }
}

impl PermissionRuleApplication for PolicyApplicationOwner {
    fn remember_allow(&self, plan: RememberPlan) -> Result<String, PolicyControlError> {
        self.remember(ManagedRuleEffect::Allow, plan)
    }

    fn remember_deny(&self, plan: RememberPlan) -> Result<String, PolicyControlError> {
        self.remember(ManagedRuleEffect::Deny, plan)
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

    // Catches: a late resolver restores a revoked allow or loses a newer deny
    // when modal Remember and a native edit publish on different threads.
    #[test]
    fn rule_publications_stage_in_store_order_across_modal_and_native_writers() {
        use ego_core::permit::{SandboxAccess, SandboxGrant, rule::Rule};
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            mpsc,
        };
        use std::time::Duration;
        struct Preflight;
        impl PreflightPort for Preflight {
            fn validate_auto(&self) -> Result<(), PolicyControlError> {
                Ok(())
            }
        }
        struct OrderedResolver {
            inner: crate::run::FrozenRuleResolver,
            first: AtomicBool,
            entered: mpsc::SyncSender<()>,
            second_done: Mutex<mpsc::Receiver<()>>,
        }
        impl FrozenPolicyResolver for OrderedResolver {
            fn max_mode(&self) -> Result<Option<Mode>, PolicyControlError> {
                self.inner.max_mode()
            }
            fn resolve_rule_transaction(
                &self,
                before: &RuleSnapshot,
                after: &RuleSnapshot,
                affected: &[RuleMutation],
            ) -> Result<FrozenRuleResolution, PolicyControlError> {
                let resolved = self
                    .inner
                    .resolve_rule_transaction(before, after, affected)?;
                if self.first.swap(false, Ordering::SeqCst) {
                    self.entered.send(()).unwrap();
                    // Without serialization the second publication finishes
                    // first, then this stale result overwrites its rules.
                    let _ = self
                        .second_done
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(1));
                }
                Ok(resolved)
            }
        }
        for revoke in [true, false] {
            let home = tempfile::tempdir().unwrap();
            let path = home.path().join("config.toml");
            std::fs::write(&path, "[permissions]\nallow = [\"read(/workspace/old)\"]\n").unwrap();
            let store: Arc<dyn UserConfigStore> = Arc::new(
                ego_tools::config::OsUserConfigStore::new(Arc::new(ego_tools::store::OsIo), path),
            );
            let old = Rule::load("read(/workspace/old)").unwrap();
            let selection = GenerationPolicySelection::new(
                Mode::Default,
                Ruleset::from_layer(
                    ego_core::config::PermCfg {
                        judge: None,
                        allow: vec![old.clone()],
                        deny: vec![],
                    },
                    ego_core::config::Source::User,
                ),
            );
            let (entered_tx, entered_rx) = mpsc::sync_channel(1);
            let (done_tx, done_rx) = mpsc::sync_channel(1);
            let resolver = Arc::new(OrderedResolver {
                inner: crate::run::FrozenRuleResolver::new(
                    vec![],
                    ego_core::config::Trust::Untrusted,
                    None,
                    selection.clone(),
                ),
                first: AtomicBool::new(true),
                entered: entered_tx,
                second_done: Mutex::new(done_rx),
            });
            let owner = Arc::new(PolicyApplicationOwner::new(
                selection.clone(),
                store.clone(),
                Arc::new(Preflight),
                resolver,
            ));
            let preview = RememberPreview::presented(
                ManagedRuleEffect::Allow,
                RememberPlan::for_sandbox_path(&SandboxGrant::new(
                    CanonPath::new(crate::ai_test_support::native("/workspace/new")),
                    SandboxAccess::Read,
                ))
                .unwrap(),
            );
            std::thread::scope(|scope| {
                let first = owner.clone();
                let first = scope.spawn(move || {
                    first
                        .apply(PolicyControlRequest::Permission(
                            PermissionInput::ConfirmRemember(preview),
                        ))
                        .unwrap()
                });
                entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                let second = owner.clone();
                let second = scope.spawn(move || {
                    let request = if revoke {
                        PolicyControlRequest::Permission(PermissionInput::Revoke(old.id()))
                    } else {
                        PolicyControlRequest::Rule(RuleMutation {
                            effect: ManagedRuleEffect::Deny,
                            rule: Rule::load("read(/workspace/blocked)").unwrap(),
                        })
                    };
                    second.apply(request).unwrap();
                    done_tx.send(()).unwrap();
                });
                first.join().unwrap();
                second.join().unwrap();
            });
            let stored = store.rule_snapshot(UserConfigTarget::Base).unwrap();
            let captured = selection.capture().unwrap();
            let allow = captured
                .ruleset
                .allow
                .iter()
                .filter(|entry| {
                    entry
                        .origins()
                        .iter()
                        .any(|origin| origin == ego_core::config::Source::User)
                })
                .map(|entry| entry.rule().to_string())
                .collect::<Vec<_>>();
            let deny = captured
                .ruleset
                .deny
                .iter()
                .filter(|entry| {
                    entry
                        .origins()
                        .iter()
                        .any(|origin| origin == ego_core::config::Source::User)
                })
                .map(|entry| entry.rule().to_string())
                .collect::<Vec<_>>();
            assert!(
                allow.iter().any(|rule| rule == "read(/workspace/new)"),
                "Remember lost: {allow:?}"
            );
            assert_eq!(
                allow.iter().any(|rule| rule == "read(/workspace/old)"),
                !revoke,
                "revoked allow restored: {allow:?}"
            );
            assert_eq!(
                deny.iter().any(|rule| rule == "read(/workspace/blocked)"),
                !revoke,
                "deny lost: {deny:?}"
            );
            assert_eq!(
                allow,
                stored
                    .allow
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                deny,
                stored
                    .deny
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
            );
        }
    }

    // Catches: a publisher panic after saving leaves stale policy capturable.
    #[test]
    fn poisoned_rule_publication_refuses_policy_capture_and_restore() {
        let selection = GenerationPolicySelection::new(Mode::Yolo, Ruleset::default());
        let source = CliGenerationPolicySource::new(
            selection.clone(),
            CanonPath::new(crate::ai_test_support::native("/workspace")),
            None,
        );
        let before = source.capture().unwrap().snapshot();
        let writer = selection.clone();
        assert!(
            std::thread::spawn(move || {
                let _publication = writer.lock_rule_publication().unwrap();
                panic!("publisher failed after publication");
            })
            .join()
            .is_err()
        );
        assert!(selection.snapshot().is_err());
        assert!(selection.capture().is_err());
        assert!(selection.next_snapshot().is_err());
        assert!(source.current().is_err());
        assert!(source.capture().is_err());
        assert!(source.restore(before).is_err());
    }

    struct PublicationResolver {
        selection: GenerationPolicySelection,
        fail: bool,
    }

    // Catches: an earlier resolver result overwrites a second published deny
    // that reached the live selection while the first resolution was running.
    #[test]
    fn interleaved_deny_publications_preserve_both_denies_and_origins() {
        use ego_core::config::{PermCfg, Source};
        use ego_core::permit::{PolicyDenied, rule::Rule};
        struct InterleavingResolver {
            selection: GenerationPolicySelection,
            store: Arc<dyn UserConfigStore>,
            interleave: std::sync::atomic::AtomicBool,
        }
        impl FrozenPolicyResolver for InterleavingResolver {
            fn max_mode(&self) -> Result<Option<Mode>, PolicyControlError> {
                Ok(None)
            }
            fn resolve_rule_transaction(
                &self,
                before: &RuleSnapshot,
                after: &RuleSnapshot,
                affected: &[RuleMutation],
            ) -> Result<FrozenRuleResolution, PolicyControlError> {
                let selected = self.selection.next_snapshot()?;
                let rules = |snapshot: &RuleSnapshot| {
                    Ruleset::from_layer(
                        PermCfg {
                            judge: None,
                            allow: snapshot.allow.clone(),
                            deny: snapshot.deny.clone(),
                        },
                        Source::User,
                    )
                };
                let before_rules = rules(before);
                let after_rules = rules(after);
                if self
                    .interleave
                    .swap(false, std::sync::atomic::Ordering::SeqCst)
                {
                    let change = RuleApplication::new(self.store.clone(), UserConfigTarget::Base)
                        .deny(Rule::load("write(/outside/second)").unwrap())
                        .map_err(map_rule_error)?;
                    finish_rule_change(&self.selection, self, change)?;
                }
                Ok(FrozenRuleResolution {
                    next: GenerationPolicySnapshot {
                        mode: selected.mode,
                        ruleset: after_rules.clone(),
                    },
                    comparisons: affected
                        .iter()
                        .map(|mutation| {
                            ego_core::config::rule_management::RuleManagement::compare_resolved(
                                &before_rules,
                                &after_rules,
                                before != after,
                                &mutation.rule,
                                mutation.effect,
                            )
                        })
                        .collect(),
                })
            }
        }
        let home = tempfile::tempdir().unwrap();
        let store: Arc<dyn UserConfigStore> = Arc::new(ego_tools::config::OsUserConfigStore::new(
            Arc::new(ego_tools::store::OsIo),
            home.path().join("config.toml"),
        ));
        let selected = GenerationPolicySelection::new(
            Mode::Yolo,
            Ruleset::from_layer(
                PermCfg {
                    judge: None,
                    allow: vec![],
                    deny: vec![Rule::load("write(/outside/admin)").unwrap()],
                },
                Source::System,
            ),
        );
        let resolver = InterleavingResolver {
            selection: selected.clone(),
            store: store.clone(),
            interleave: std::sync::atomic::AtomicBool::new(true),
        };
        let first = RuleApplication::new(store.clone(), UserConfigTarget::Base)
            .deny(Rule::load("write(/outside/first)").unwrap())
            .unwrap();
        finish_rule_change(&selected, &resolver, first).unwrap();
        assert_eq!(
            store
                .rule_snapshot(UserConfigTarget::Base)
                .unwrap()
                .deny
                .len(),
            2
        );
        let confinement = ego_core::permit::RunConfinement::new(
            ego_core::permit::Confinement::Off,
            false,
            CanonPath::new(crate::ai_test_support::native("/workspace")),
        )
        .unwrap();
        let source = CliGenerationPolicySource::new(
            selected,
            CanonPath::new(crate::ai_test_support::native("/workspace")),
            None,
        )
        .with_read_perimeter(&confinement);
        let capture = source.capture().unwrap();
        for (path, expected_source) in [
            ("/outside/first", Source::User),
            ("/outside/second", Source::User),
            ("/outside/admin", Source::System),
        ] {
            assert!(
                matches!(capture.policy().classify(PreparedCall::new("write", Op::Write {
                path: CanonPath::new(path), change: Change::Whole(vec![]),
            })), Classification::Settled(Err(PolicyDenied::Denied { from, .. })) if from == expected_source)
            );
        }
    }

    impl FrozenPolicyResolver for PublicationResolver {
        fn max_mode(&self) -> Result<Option<Mode>, PolicyControlError> {
            Ok(None)
        }

        fn resolve_rule_transaction(
            &self,
            before: &RuleSnapshot,
            after: &RuleSnapshot,
            affected: &[RuleMutation],
        ) -> Result<FrozenRuleResolution, PolicyControlError> {
            if self.fail {
                return Err(PolicyControlError::Resolution {
                    diagnostic: "injected resolution failure after publication".into(),
                });
            }
            let earlier_mode = self.selection.next_snapshot()?.mode;
            // A mode choice lands after the resolver reads its mode, before
            // the caller publishes the resolved rules into the selection.
            self.selection.stage_mode(Mode::Yolo)?;
            let rules = |snapshot: &RuleSnapshot| {
                Ruleset::from_layer(
                    ego_core::config::PermCfg {
                        judge: None,
                        allow: snapshot.allow.clone(),
                        deny: snapshot.deny.clone(),
                    },
                    ego_core::config::Source::User,
                )
            };
            let before_rules = rules(before);
            let after_rules = rules(after);
            Ok(FrozenRuleResolution {
                comparisons: affected
                    .iter()
                    .map(|mutation| {
                        ego_core::config::rule_management::RuleManagement::compare_resolved(
                            &before_rules,
                            &after_rules,
                            before != after,
                            &mutation.rule,
                            mutation.effect,
                        )
                    })
                    .collect(),
                next: GenerationPolicySnapshot {
                    mode: earlier_mode,
                    ruleset: after_rules,
                },
            })
        }
    }

    // Catches: a persisted deny followed by resolution failure leaves a stale
    // yolo policy available to a later generation or restored continuation.
    #[tokio::test]
    async fn published_deny_resolution_failure_invalidates_future_policy_access() {
        use ego_acp::permission::PermissionRuleWriter;
        use ego_core::permit::{SandboxAccess, SandboxGrant};
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("config.toml");
        let selection = GenerationPolicySelection::new(Mode::Yolo, Ruleset::default());
        let source = CliGenerationPolicySource::new(
            selection.clone(),
            CanonPath::new(crate::ai_test_support::native("/workspace")),
            None,
        );
        let previous = source.capture().unwrap().snapshot();
        let writer =
            crate::acp_permission::CliAcpPermissionRules::new(path.clone(), UserConfigTarget::Base)
                .with_live_policy(
                    selection.clone(),
                    Arc::new(PublicationResolver {
                        selection: selection.clone(),
                        fail: true,
                    }),
                );
        let grant = SandboxGrant::new(CanonPath::new("/outside/data"), SandboxAccess::Write);
        assert!(
            writer
                .remember(
                    ManagedRuleEffect::Deny,
                    RememberPlan::for_sandbox_path(&grant).unwrap()
                )
                .await
                .is_err()
        );
        let store =
            ego_tools::config::OsUserConfigStore::new(Arc::new(ego_tools::store::OsIo), path);
        assert_eq!(
            store
                .rule_snapshot(UserConfigTarget::Base)
                .unwrap()
                .deny
                .len(),
            1,
            "publication really succeeded"
        );
        assert!(selection.snapshot().is_err());
        assert!(selection.capture().is_err());
        assert!(selection.next_snapshot().is_err());
        assert!(selection.stage_mode(Mode::Yolo).is_err());
        assert!(
            selection
                .stage_rules(
                    Ruleset::default(),
                    false,
                    store.rule_snapshot(UserConfigTarget::Base).unwrap()
                )
                .is_err()
        );
        assert!(source.current().is_err());
        assert!(source.capture().is_err());
        assert!(source.restore(previous).is_err());
    }

    // Catches: background rule publication stages its old mode over a newer
    // interactive mode selection made while resolution was running.
    #[tokio::test]
    async fn rule_publication_preserves_mode_selected_after_resolution_snapshot() {
        use ego_acp::permission::PermissionRuleWriter;
        use ego_core::permit::{PolicyDenied, SandboxAccess, SandboxGrant};
        let home = tempfile::tempdir().unwrap();
        let selection = GenerationPolicySelection::new(Mode::Default, Ruleset::default());
        let writer = crate::acp_permission::CliAcpPermissionRules::new(
            home.path().join("config.toml"),
            UserConfigTarget::Base,
        )
        .with_live_policy(
            selection.clone(),
            Arc::new(PublicationResolver {
                selection: selection.clone(),
                fail: false,
            }),
        );
        let grant = SandboxGrant::new(CanonPath::new("/outside/data"), SandboxAccess::Write);
        writer
            .remember(
                ManagedRuleEffect::Deny,
                RememberPlan::for_sandbox_path(&grant).unwrap(),
            )
            .await
            .unwrap();
        let confinement = ego_core::permit::RunConfinement::new(
            ego_core::permit::Confinement::Off,
            false,
            CanonPath::new(crate::ai_test_support::native("/workspace")),
        )
        .unwrap();
        let source = CliGenerationPolicySource::new(
            selection.clone(),
            CanonPath::new(crate::ai_test_support::native("/workspace")),
            None,
        )
        .with_read_perimeter(&confinement);
        let captured = source.capture().unwrap();
        assert_eq!(selection.snapshot().unwrap().mode, Mode::Yolo);
        assert!(matches!(
            captured.policy().classify(PreparedCall::new(
                "write",
                Op::Write {
                    path: grant.path,
                    change: Change::Whole(vec![]),
                }
            )),
            Classification::Settled(Err(PolicyDenied::Denied { .. }))
        ));
    }

    // Catches: restoring an old unbounded snapshot bypasses the currently
    // admitted administrator write ceiling even though new captures bind it.
    #[test]
    fn current_capture_and_restore_apply_current_root_ceilings() {
        use ego_core::permit::{
            Confinement, PolicyDenied, RootAccess, RootCeilings, RunConfinement,
        };
        let workspace = CanonPath::new(crate::ai_test_support::native("/workspace"));
        let confinement = RunConfinement::new(Confinement::Off, false, workspace.clone()).unwrap();
        let unbounded = CliGenerationPolicySource::new(
            GenerationPolicySelection::new(Mode::Yolo, Ruleset::default()),
            workspace.clone(),
            None,
        )
        .with_read_perimeter(&confinement);
        let old = unbounded.capture().unwrap();
        let bounded = CliGenerationPolicySource::new(
            GenerationPolicySelection::new(Mode::Yolo, Ruleset::default()),
            workspace,
            None,
        )
        .with_read_perimeter(&confinement)
        .with_root_ceilings(RootCeilings::new(vec![vec![ProfileRoot::new(
            CanonPath::new("/allowed"),
            RootAccess::ReadWrite,
        )]]));
        for policy in [
            bounded.current().unwrap(),
            bounded.capture().unwrap().policy(),
            bounded.restore(old.snapshot()).unwrap().policy(),
        ] {
            let denied = policy.classify(PreparedCall::new(
                "write",
                Op::Write {
                    path: CanonPath::new("/sibling/notes.md"),
                    change: Change::Whole(b"changed".to_vec()),
                },
            ));
            assert!(matches!(
                denied,
                Classification::Settled(Err(PolicyDenied::HardFloor {
                    class: "root ceiling"
                }))
            ));
        }
    }

    // Catches: same-repository roots fixed at start, so the built-in tools refuse
    // a worktree created mid-session, and a forged entry created mid-session joining.
    #[test]
    fn a_worktree_made_mid_session_is_reachable_and_a_forged_entry_is_not() {
        use ego_core::permit::PolicyDenied;
        let tree = tempfile::tempdir().unwrap();
        let base = tree.path().canonicalize().unwrap();
        let git = |dir: &std::path::Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .current_dir(dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success());
        };
        std::fs::create_dir(base.join("main")).unwrap();
        git(&base.join("main"), &["init", "-q"]);
        git(
            &base.join("main"),
            &["commit", "-q", "--allow-empty", "-m", "x"],
        );
        git(
            &base.join("main"),
            &["worktree", "add", "-q", "../wt", "-b", "wt"],
        );
        std::fs::create_dir(base.join("foreign")).unwrap();
        git(&base.join("foreign"), &["init", "-q"]);
        let ws = CanonPath::new(base.join("wt"));
        let source = CliGenerationPolicySource::new(
            GenerationPolicySelection::new(Mode::Yolo, Ruleset::default()),
            ws.clone(),
            None,
        )
        .with_profile_roots(crate::run::profile_roots::same_repository(&ws));
        let outside = |path: std::path::PathBuf| {
            let engine = source.current().unwrap();
            matches!(
                engine.classify(PreparedCall::new(
                    "write",
                    Op::Write { path: CanonPath::new(path), change: Change::Whole(vec![]) },
                )),
                Classification::Settled(Err(denied)) if matches!(denied, PolicyDenied::OutsideWorkspace { .. })
            )
        };
        git(
            &base.join("main"),
            &["worktree", "add", "-q", "../late", "-b", "late"],
        );
        assert!(!outside(base.join("late/x.md")));
        let forged = base.join("main/.git/worktrees/forged");
        std::fs::create_dir_all(&forged).unwrap();
        std::fs::write(
            forged.join("gitdir"),
            format!("{}\n", base.join("foreign/.git").display()),
        )
        .unwrap();
        assert!(outside(base.join("foreign/x.md")));
    }

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
        assert!(selection.stage_mode(Mode::Plan).unwrap().changed);
        assert!(!selection.stage_mode(Mode::Plan).unwrap().changed);
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
        // Owners: this test, the CLI source, the policy, and the capture.
        assert_eq!(Arc::strong_count(&configured_human), 4);
        assert_eq!(Arc::strong_count(&snapshot_human), 1);
        drop(restored_policy);
        drop(restored);
        assert_eq!(Arc::strong_count(&configured_human), 2);
    }
}
