//! Pure interactive policy-control state and application coordination.
//!
//! The presenter emits typed requests, while the application owner validates,
//! persists, resolves, and stages policy for the next generation boundary.

use std::sync::{Arc, RwLock};

use ego_core::config::rule_management::{EffectiveRule, RuleExplanation, RuleManagement};
use ego_core::config::{
    Mode, RuleMutation, RuleSnapshot, Ruleset, UserConfigMutation, UserConfigMutationResult,
    UserConfigPublication, UserConfigStore, UserConfigTarget,
};
use ego_core::permit::{CanonPath, HumanApprover, PolicyEngine, PreparedCall};
use ego_core::turn::{
    GenerationPolicyCapture, GenerationPolicySnapshot as CapturedGenerationPolicySnapshot,
    GenerationPolicySource,
};

const MODE_OPTIONS: [Mode; 5] = [
    Mode::Plan,
    Mode::Default,
    Mode::Auto,
    Mode::Edits,
    Mode::Yolo,
];

/// One input delivered while the presenter owns a confirmation modal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModalInput {
    Cancel,
    CtrlC,
    Eof,
    Text(String),
}

/// A read-only operation over the generation's admitted permission rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionInput {
    List,
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum ModalState {
    #[default]
    Idle,
    ConfirmingYolo,
}

/// Presenter-owned modal state for the interactive policy controls.
#[derive(Debug, Default)]
pub struct PolicyControlController {
    modal: ModalState,
}

impl PolicyControlController {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn mode_options(&self) -> [Mode; 5] {
        MODE_OPTIONS
    }

    /// Select a mode, opening confirmation instead of emitting `yolo`.
    pub fn select_mode(&mut self, mode: Mode) -> Option<PolicyControlRequest> {
        if mode == Mode::Yolo {
            self.begin_yolo();
            return None;
        }
        self.modal = ModalState::Idle;
        Some(PolicyControlRequest::SetMode(mode))
    }

    pub fn begin_yolo(&mut self) {
        self.modal = ModalState::ConfirmingYolo;
    }

    /// Consume one confirmation modal. Only the exact word `yolo` emits.
    pub fn confirm_yolo(&mut self, input: ModalInput) -> Option<PolicyControlRequest> {
        let modal = std::mem::take(&mut self.modal);
        match (modal, input) {
            (ModalState::ConfirmingYolo, ModalInput::Text(value)) if value == "yolo" => {
                Some(PolicyControlRequest::SetMode(Mode::Yolo))
            }
            _ => None,
        }
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
            human,
        }
    }

    fn engine(&self, snapshot: GenerationPolicySnapshot) -> Arc<PolicyEngine> {
        Arc::new(match self.human.as_ref() {
            Some(human) => PolicyEngine::with_human(
                self.workspace.clone(),
                snapshot.mode,
                snapshot.ruleset,
                Arc::clone(human),
            ),
            None => PolicyEngine::new(self.workspace.clone(), snapshot.mode, snapshot.ruleset),
        })
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
        GenerationPolicyCapture::new(
            self.workspace.clone(),
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
    fn resolve(&self) -> Result<(Mode, Ruleset), PolicyControlError>;
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
        mutation: UserConfigMutationResult,
        staging: PolicyStageOutcome,
    },
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
        };
        write!(formatter, "{context}: {diagnostic}")
    }
}

impl std::error::Error for PolicyControlError {}

/// Application owner for policy preflight, persistence, resolution, and staging.
pub struct PolicyApplicationOwner {
    selection: GenerationPolicySelection,
    store: Arc<dyn UserConfigStore>,
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
            store,
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
            PolicyControlRequest::Permission(input) => {
                self.explain(input).map(PolicyControlOutcome::Permission)
            }
        }
    }

    /// Inspect admitted rules without reading mutable config a second time.
    pub fn explain(&self, input: PermissionInput) -> Result<PermissionOutcome, PolicyControlError> {
        let ruleset = self.selection.snapshot()?.ruleset;
        Ok(match input {
            PermissionInput::List => PermissionOutcome::List(RuleManagement::effective_rows(
                &ruleset,
                &RuleSnapshot {
                    target: UserConfigTarget::Base,
                    permissions_present: false,
                    allow: Vec::new(),
                    deny: Vec::new(),
                },
            )),
            PermissionInput::Explain { call, workspace } => PermissionOutcome::Explain(
                RuleManagement::explain(&call, &ruleset, workspace.as_path()),
            ),
        })
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
        let result = self
            .store
            .mutate(UserConfigMutation::Rules {
                target: UserConfigTarget::Base,
                changes: vec![mutation],
            })
            .map_err(|error| PolicyControlError::Store {
                diagnostic: error.to_string(),
            })?;

        if !result.changed {
            return Ok(PolicyControlOutcome::Rule {
                mutation: result,
                staging: PolicyStageOutcome { changed: false },
            });
        }
        if result.publication == UserConfigPublication::NotPublished {
            return Err(PolicyControlError::PublicationContract {
                diagnostic: "a changed mutation was reported as not published".into(),
            });
        }

        let (mode, ruleset) = self.resolver.resolve()?;
        let staging = self.selection.stage(mode, ruleset)?;
        Ok(PolicyControlOutcome::Rule {
            mutation: result,
            staging,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use ego_core::permit::{
        Change, Classification, Op, PermissionDecision, PermissionRequest, Resolution,
    };
    use futures::future::BoxFuture;

    #[derive(Debug)]
    struct AllowingHumanApprover;

    impl HumanApprover for AllowingHumanApprover {
        fn decide<'a>(
            &'a self,
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
