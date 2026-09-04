//! RED contract for regression case interactive policy controls.
//!
//! The presenter owns modal state only. The application owner receives typed
//! requests and owns preflight, persistence, re-resolution, and staging.

use std::sync::{Arc, Mutex};

use ego_cli::repl::actions::{ActionId, ActionKind, ActionRegistry, Dispatch, InputOrigin};
use ego_cli::repl::policy_controls::{
    FrozenPolicyResolver, GenerationPolicySelection, ModalInput, PermissionInput,
    PolicyApplicationOwner, PolicyControlController, PolicyControlError, PolicyControlRequest,
    PreflightPort,
};
use ego_core::config::{
    ManagedConfigKey, Mode, RuleMutation, RuleSnapshot, Ruleset, Source, UserConfigError,
    UserConfigMutation, UserConfigMutationResult, UserConfigPublication, UserConfigSnapshot,
    UserConfigStore, UserConfigTarget,
};
use ego_core::permit::{CanonPath, Op, PreparedCall};

#[derive(Clone)]
struct RecordingStore {
    mutations: Arc<Mutex<Vec<UserConfigMutation>>>,
    reads: Arc<Mutex<usize>>,
    publication: Result<UserConfigPublication, UserConfigError>,
}

impl RecordingStore {
    fn published(publication: UserConfigPublication) -> Self {
        Self {
            mutations: Arc::new(Mutex::new(Vec::new())),
            reads: Arc::new(Mutex::new(0)),
            publication: Ok(publication),
        }
    }

    fn failing(message: &str) -> Self {
        Self {
            mutations: Arc::new(Mutex::new(Vec::new())),
            reads: Arc::new(Mutex::new(0)),
            publication: Err(UserConfigError::new(message)),
        }
    }

    fn mutations(&self) -> Vec<UserConfigMutation> {
        self.mutations.lock().unwrap().clone()
    }

    fn reads(&self) -> usize {
        *self.reads.lock().unwrap()
    }
}

impl UserConfigStore for RecordingStore {
    fn snapshot(&self, target: UserConfigTarget) -> Result<UserConfigSnapshot, UserConfigError> {
        *self.reads.lock().unwrap() += 1;
        Ok(UserConfigSnapshot {
            values: ManagedConfigKey::for_target(target.clone())
                .iter()
                .copied()
                .map(|key| (key, None))
                .collect(),
            target,
        })
    }

    fn rule_snapshot(&self, target: UserConfigTarget) -> Result<RuleSnapshot, UserConfigError> {
        *self.reads.lock().unwrap() += 1;
        Ok(RuleSnapshot {
            target,
            permissions_present: false,
            allow: Vec::new(),
            deny: Vec::new(),
        })
    }

    fn mutate(
        &self,
        mutation: UserConfigMutation,
    ) -> Result<UserConfigMutationResult, UserConfigError> {
        self.mutations.lock().unwrap().push(mutation.clone());
        self.publication
            .clone()
            .map(|publication| UserConfigMutationResult {
                target: mutation.target(),
                facts: mutation.facts(),
                changed: true,
                publication,
            })
    }
}

struct RefusingJudge;

impl PreflightPort for RefusingJudge {
    fn validate_auto(&self) -> Result<(), PolicyControlError> {
        Err(PolicyControlError::JudgeUnavailable {
            diagnostic: "no supplied judge".into(),
        })
    }
}

#[derive(Clone)]
struct RecordingResolver {
    calls: Arc<Mutex<usize>>,
    snapshot: (Mode, Ruleset),
}

impl FrozenPolicyResolver for RecordingResolver {
    fn resolve(&self) -> Result<(Mode, Ruleset), PolicyControlError> {
        *self.calls.lock().unwrap() += 1;
        Ok(self.snapshot.clone())
    }
}

fn owner(
    selection: GenerationPolicySelection,
    store: Arc<dyn UserConfigStore>,
    resolver: Arc<RecordingResolver>,
) -> PolicyApplicationOwner {
    PolicyApplicationOwner::new(selection, store, Arc::new(RefusingJudge), resolver)
}

#[test]
fn presenter_mode_selector_has_exact_values_and_emits_plan_each_time() {
    let mut presenter = PolicyControlController::new();
    assert_eq!(
        presenter.mode_options(),
        [
            Mode::Plan,
            Mode::Default,
            Mode::Auto,
            Mode::Edits,
            Mode::Yolo
        ]
    );
    assert_eq!(
        presenter.select_mode(Mode::Plan),
        Some(PolicyControlRequest::SetMode(Mode::Plan))
    );
    assert_eq!(
        presenter.select_mode(Mode::Plan),
        Some(PolicyControlRequest::SetMode(Mode::Plan))
    );
}

#[test]
fn presenter_yolo_requires_exact_lowercase_and_cancel_paths_emit_no_request() {
    for input in [
        ModalInput::Cancel,
        ModalInput::CtrlC,
        ModalInput::Eof,
        ModalInput::Text("YOLO".into()),
        ModalInput::Text("yolo ".into()),
    ] {
        let mut presenter = PolicyControlController::new();
        presenter.begin_yolo();
        assert_eq!(presenter.confirm_yolo(input), None);
    }

    let mut presenter = PolicyControlController::new();
    presenter.begin_yolo();
    assert_eq!(
        presenter.confirm_yolo(ModalInput::Text("yolo".into())),
        Some(PolicyControlRequest::SetMode(Mode::Yolo))
    );
}

#[test]
fn owner_auto_uses_injected_preflight_and_failure_preserves_selection() {
    let selection = GenerationPolicySelection::new(Mode::Default, Ruleset::default());
    let before = selection.snapshot().unwrap();
    let store = Arc::new(RecordingStore::published(
        UserConfigPublication::NotPublished,
    ));
    let resolver = Arc::new(RecordingResolver {
        calls: Arc::new(Mutex::new(0)),
        snapshot: (Mode::Default, Ruleset::default()),
    });
    let owner = owner(selection.clone(), store.clone(), resolver.clone());
    assert!(matches!(
        owner.apply(PolicyControlRequest::SetMode(Mode::Auto)),
        Err(PolicyControlError::JudgeUnavailable { .. })
    ));
    assert_eq!(selection.snapshot().unwrap(), before);
    assert_eq!(*resolver.calls.lock().unwrap(), 0);
    assert!(store.mutations().is_empty());
}

#[test]
fn owner_mutation_constructs_base_and_resolves_once_for_each_success_publication() {
    for publication in [
        UserConfigPublication::PublishedDurably,
        UserConfigPublication::PublishedDurabilityUncertain {
            diagnostic: "directory sync failed".into(),
        },
    ] {
        let selection = GenerationPolicySelection::new(Mode::Default, Ruleset::default());
        let store = Arc::new(RecordingStore::published(publication));
        let resolver = Arc::new(RecordingResolver {
            calls: Arc::new(Mutex::new(0)),
            snapshot: (Mode::Edits, Ruleset::default()),
        });
        let owner = owner(selection.clone(), store.clone(), resolver.clone());
        let mutation = RuleMutation {
            effect: ego_core::config::ManagedRuleEffect::Allow,
            rule: ego_core::permit::rule::Rule::load("read(src/**)").unwrap(),
        };
        owner
            .apply(PolicyControlRequest::Rule(mutation.clone()))
            .unwrap();
        assert_eq!(*resolver.calls.lock().unwrap(), 1);
        assert_eq!(selection.snapshot().unwrap().mode, Mode::Default);
        assert_eq!(
            selection.staged_snapshot().unwrap().unwrap().mode,
            Mode::Edits
        );
        assert!(matches!(
            &store.mutations()[..],
            [UserConfigMutation::Rules { target: UserConfigTarget::Base, changes }] if changes == &[mutation]
        ));
    }

    let selection = GenerationPolicySelection::new(Mode::Default, Ruleset::default());
    let store = Arc::new(RecordingStore::failing("publication refused"));
    let resolver = Arc::new(RecordingResolver {
        calls: Arc::new(Mutex::new(0)),
        snapshot: (Mode::Edits, Ruleset::default()),
    });
    let owner = owner(selection.clone(), store, resolver.clone());
    assert!(
        owner
            .apply(PolicyControlRequest::Rule(RuleMutation {
                effect: ego_core::config::ManagedRuleEffect::Allow,
                rule: ego_core::permit::rule::Rule::load("read(src/**)").unwrap(),
            }))
            .is_err()
    );
    assert_eq!(*resolver.calls.lock().unwrap(), 0);
    assert_eq!(selection.snapshot().unwrap().mode, Mode::Default);
    assert!(selection.staged_snapshot().unwrap().is_none());
}

#[test]
fn permissions_explain_uses_captured_admitted_rules_and_real_call() {
    let call = PreparedCall::new(
        "read",
        Op::Read {
            path: CanonPath::new("/workspace/src/main.rs"),
        },
    );
    let workspace = CanonPath::new("/workspace");
    let rules = Ruleset::from_layer(
        ego_core::config::PermCfg {
            judge: None,
            allow: vec![],
            deny: vec![],
        },
        Source::Project,
    );
    let selection = GenerationPolicySelection::new(Mode::Default, rules.clone());
    let store = Arc::new(RecordingStore::published(
        UserConfigPublication::NotPublished,
    ));
    let resolver = Arc::new(RecordingResolver {
        calls: Arc::new(Mutex::new(0)),
        snapshot: (Mode::Default, rules.clone()),
    });
    let owner = owner(selection, store.clone(), resolver);
    let reads_before = store.reads();
    let first = owner
        .explain(PermissionInput::Explain {
            call: call.clone(),
            workspace: workspace.clone(),
        })
        .unwrap();
    store
        .mutate(UserConfigMutation::Rules {
            target: UserConfigTarget::Profile("ci".parse().unwrap()),
            changes: vec![],
        })
        .unwrap();
    assert_eq!(store.reads(), reads_before);
    assert_eq!(
        owner
            .explain(PermissionInput::Explain { call, workspace })
            .unwrap(),
        first
    );
}

#[test]
fn registry_routes_four_controls_and_canonical_names_refuse_direct_arguments() {
    let registry = ActionRegistry::final_surface();
    for (name, id) in [
        ("mode", ActionId::Mode),
        ("plan", ActionId::Plan),
        ("yolo", ActionId::Yolo),
        ("permissions", ActionId::Permissions),
    ] {
        assert_eq!(registry.resolve_slash(name), Some(id));
        assert_eq!(registry.kind(id), ActionKind::Local);
    }
    for text in [
        "/mode edits",
        "/permissions allow read(src/**)",
        "/plan now",
        "/yolo yes",
    ] {
        assert!(matches!(
            registry.classify(registry.submit(text, InputOrigin::Typed)),
            Dispatch::Refused { .. }
        ));
    }
}
