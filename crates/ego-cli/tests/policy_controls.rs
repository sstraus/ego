//! contract for regression case interactive policy controls.
//!
//! The presenter owns modal state only. The application owner receives typed
//! requests and owns preflight, persistence, re-resolution, and staging.

use std::sync::{Arc, Mutex};

use ego_cli::repl::actions::{ActionId, ActionKind, ActionRegistry, Dispatch, InputOrigin};
use ego_cli::repl::policy_controls::{
    FrozenPolicyResolver, GenerationPolicySelection, ModalInput, PermissionInput,
    PolicyApplicationOwner, PolicyControlController, PolicyControlError, PolicyControlRequest,
    PreflightPort, RememberPreview, RememberRequest, RuleMutationReceipt,
};
use ego_core::config::{
    ManagedConfigKey, Mode, RuleMutation, RuleSnapshot, Ruleset, Source, UserConfigError,
    UserConfigMutation, UserConfigMutationFacts, UserConfigMutationResult, UserConfigPublication,
    UserConfigSnapshot, UserConfigStore, UserConfigTarget,
};
use ego_core::permit::rule::Rule;
use ego_core::permit::{CanonPath, Op, PreparedCall};

fn rule(raw: &str) -> Rule {
    Rule::load(raw).expect("valid rule fixture")
}

fn empty_rule_snapshot() -> RuleSnapshot {
    RuleSnapshot {
        target: UserConfigTarget::Base,
        permissions_present: true,
        allow: Vec::new(),
        deny: Vec::new(),
    }
}

#[derive(Clone)]
struct RecordingStore {
    mutations: Arc<Mutex<Vec<UserConfigMutation>>>,
    reads: Arc<Mutex<usize>>,
    publication: Result<UserConfigPublication, UserConfigError>,
    before: RuleSnapshot,
    after: Option<RuleSnapshot>,
}

impl RecordingStore {
    fn published(publication: UserConfigPublication) -> Self {
        Self {
            mutations: Arc::new(Mutex::new(Vec::new())),
            reads: Arc::new(Mutex::new(0)),
            publication: Ok(publication),
            before: empty_rule_snapshot(),
            after: None,
        }
    }

    fn failing(message: &str) -> Self {
        Self {
            mutations: Arc::new(Mutex::new(Vec::new())),
            reads: Arc::new(Mutex::new(0)),
            publication: Err(UserConfigError::new(message)),
            before: empty_rule_snapshot(),
            after: None,
        }
    }

    fn transaction(
        publication: UserConfigPublication,
        before: RuleSnapshot,
        after: RuleSnapshot,
    ) -> Self {
        let mut store = Self::published(publication);
        store.before = before;
        store.after = Some(after);
        store
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
        let before = self.before.clone();
        let after = self.after.clone().unwrap_or_else(|| {
            let mut after = before.clone();
            match &mutation {
                UserConfigMutation::Rules { changes, .. } => {
                    for change in changes {
                        let list = match change.effect {
                            ego_core::config::ManagedRuleEffect::Allow => &mut after.allow,
                            ego_core::config::ManagedRuleEffect::Deny => &mut after.deny,
                        };
                        if !list.iter().any(|entry| entry == &change.rule) {
                            list.push(change.rule.clone());
                            after.permissions_present = true;
                        }
                    }
                }
                UserConfigMutation::RevokeRule { id, .. } => {
                    after.allow.retain(|entry| entry.id() != *id);
                    after.deny.retain(|entry| entry.id() != *id);
                }
                _ => {}
            }
            after
        });
        let facts = match mutation.facts() {
            UserConfigMutationFacts::Batch { changes } => {
                UserConfigMutationFacts::RuleTransaction {
                    request: Box::new(UserConfigMutationFacts::Batch { changes }),
                    before,
                    after,
                }
            }
            UserConfigMutationFacts::Revoke { id } => UserConfigMutationFacts::RuleTransaction {
                request: Box::new(UserConfigMutationFacts::Revoke { id }),
                before,
                after,
            },
            other => other,
        };
        let changed = matches!(
            &facts,
            UserConfigMutationFacts::RuleTransaction { before, after, .. } if before != after
        );
        self.publication
            .clone()
            .map(|publication| UserConfigMutationResult {
                target: mutation.target(),
                facts,
                changed,
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

struct AllowingJudge;

impl PreflightPort for AllowingJudge {
    fn validate_auto(&self) -> Result<(), PolicyControlError> {
        Ok(())
    }
}

type RuleResolutionCalls = Vec<(RuleSnapshot, RuleSnapshot, Vec<RuleMutation>)>;

#[derive(Clone)]
struct RecordingResolver {
    calls: Arc<Mutex<RuleResolutionCalls>>,
    next: ego_cli::repl::policy_controls::GenerationPolicySnapshot,
}

impl FrozenPolicyResolver for RecordingResolver {
    fn resolve_rule_transaction(
        &self,
        before: &RuleSnapshot,
        after: &RuleSnapshot,
        affected: &[RuleMutation],
    ) -> Result<ego_cli::repl::policy_controls::FrozenRuleResolution, PolicyControlError> {
        self.calls
            .lock()
            .unwrap()
            .push((before.clone(), after.clone(), affected.to_vec()));
        Ok(ego_cli::repl::policy_controls::FrozenRuleResolution {
            next: self.next.clone(),
            comparisons: affected
                .iter()
                .map(
                    |_| ego_core::config::rule_management::RuleMutationComparison {
                        stored_changed: true,
                        effective_changed: true,
                        authorities: Vec::new(),
                    },
                )
                .collect(),
        })
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
        calls: Arc::new(Mutex::new(Vec::new())),
        next: ego_cli::repl::policy_controls::GenerationPolicySnapshot {
            mode: Mode::Default,
            ruleset: Ruleset::default(),
        },
    });
    let owner = owner(selection.clone(), store.clone(), resolver.clone());
    assert!(matches!(
        owner.apply(PolicyControlRequest::SetMode(Mode::Auto)),
        Err(PolicyControlError::JudgeUnavailable { .. })
    ));
    assert_eq!(selection.snapshot().unwrap(), before);
    assert!(resolver.calls.lock().unwrap().is_empty());
    assert!(store.mutations().is_empty());
}

#[test]
fn staged_modes_survive_a_rule_transaction_until_the_next_capture() {
    for mode in [Mode::Plan, Mode::Edits, Mode::Yolo] {
        let selection = GenerationPolicySelection::new(Mode::Default, Ruleset::default());
        let store = Arc::new(RecordingStore::published(
            UserConfigPublication::PublishedDurably,
        ));
        let resolver = Arc::new(RecordingResolver {
            calls: Arc::new(Mutex::new(Vec::new())),
            next: ego_cli::repl::policy_controls::GenerationPolicySnapshot {
                mode,
                ruleset: Ruleset::default(),
            },
        });
        let application = owner(selection.clone(), store, resolver);
        application
            .apply(PolicyControlRequest::SetMode(mode))
            .unwrap();
        application
            .apply(PolicyControlRequest::Rule(RuleMutation {
                effect: ego_core::config::ManagedRuleEffect::Allow,
                rule: rule("read(src/**)"),
            }))
            .unwrap();
        assert_eq!(selection.snapshot().unwrap().mode, Mode::Default);
        assert_eq!(selection.staged_snapshot().unwrap().unwrap().mode, mode);
    }
}

#[test]
fn owner_valid_auto_preflight_stages_without_changing_active_mode() {
    let selection = GenerationPolicySelection::new(Mode::Default, Ruleset::default());
    let store = Arc::new(RecordingStore::published(
        UserConfigPublication::NotPublished,
    ));
    let resolver = Arc::new(RecordingResolver {
        calls: Arc::new(Mutex::new(Vec::new())),
        next: ego_cli::repl::policy_controls::GenerationPolicySnapshot {
            mode: Mode::Auto,
            ruleset: Ruleset::default(),
        },
    });
    let application = PolicyApplicationOwner::new(
        selection.clone(),
        store.clone(),
        Arc::new(AllowingJudge),
        resolver.clone(),
    );
    assert!(
        application
            .apply(PolicyControlRequest::SetMode(Mode::Auto))
            .is_ok()
    );
    assert_eq!(selection.snapshot().unwrap().mode, Mode::Default);
    assert_eq!(
        selection.staged_snapshot().unwrap().unwrap().mode,
        Mode::Auto
    );
    assert!(store.mutations().is_empty());
    assert!(resolver.calls.lock().unwrap().is_empty());
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
            calls: Arc::new(Mutex::new(Vec::new())),
            next: ego_cli::repl::policy_controls::GenerationPolicySnapshot {
                mode: Mode::Edits,
                ruleset: Ruleset::default(),
            },
        });
        let owner = owner(selection.clone(), store.clone(), resolver.clone());
        let mutation = RuleMutation {
            effect: ego_core::config::ManagedRuleEffect::Allow,
            rule: ego_core::permit::rule::Rule::load("read(src/**)").unwrap(),
        };
        owner
            .apply(PolicyControlRequest::Rule(mutation.clone()))
            .unwrap();
        let calls = resolver.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0.target, UserConfigTarget::Base);
        assert_eq!(calls[0].1.target, UserConfigTarget::Base);
        assert_eq!(calls[0].2, vec![mutation.clone()]);
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
        calls: Arc::new(Mutex::new(Vec::new())),
        next: ego_cli::repl::policy_controls::GenerationPolicySnapshot {
            mode: Mode::Edits,
            ruleset: Ruleset::default(),
        },
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
    assert!(resolver.calls.lock().unwrap().is_empty());
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
        calls: Arc::new(Mutex::new(Vec::new())),
        next: ego_cli::repl::policy_controls::GenerationPolicySnapshot {
            mode: Mode::Default,
            ruleset: rules.clone(),
        },
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
fn remember_preview_is_pure_and_confirmation_emits_one_base_mutation() {
    let call = PreparedCall::new(
        "read",
        Op::Read {
            path: CanonPath::new("/workspace/src/main.rs"),
        },
    );
    let request = RememberRequest::new(
        call,
        ego_core::config::ManagedRuleEffect::Allow,
        CanonPath::new("/workspace"),
    );
    let store = Arc::new(RecordingStore::published(
        UserConfigPublication::PublishedDurably,
    ));
    let selection = GenerationPolicySelection::new(Mode::Default, Ruleset::default());
    let resolver = Arc::new(RecordingResolver {
        calls: Arc::new(Mutex::new(Vec::new())),
        next: ego_cli::repl::policy_controls::GenerationPolicySnapshot {
            mode: Mode::Edits,
            ruleset: Ruleset::default(),
        },
    });
    let owner = owner(selection.clone(), store.clone(), resolver.clone());
    let preview: RememberPreview = owner.preview_remember(request.clone()).unwrap();
    assert!(store.mutations().is_empty());
    assert_eq!(preview, owner.preview_remember(request.clone()).unwrap());
    let receipt: RuleMutationReceipt = match owner
        .apply(PolicyControlRequest::Permission(
            PermissionInput::ConfirmRemember(preview),
        ))
        .unwrap()
    {
        ego_cli::repl::policy_controls::PolicyControlOutcome::Rule { receipt, .. } => receipt,
        other => panic!("unexpected confirmed remember outcome: {other:?}"),
    };
    assert!(matches!(
        &store.mutations()[..],
        [UserConfigMutation::Rules {
            target: UserConfigTarget::Base,
            changes
        }] if changes.len() == 1
    ));
    assert_eq!(
        receipt.publication(),
        UserConfigPublication::PublishedDurably
    );
    assert_eq!(resolver.calls.lock().unwrap().len(), 1);
    let remember_calls = resolver.calls.lock().unwrap();
    let remember_call = &remember_calls[0];
    assert_eq!(remember_call.0.target, UserConfigTarget::Base);
    assert_eq!(remember_call.1.target, UserConfigTarget::Base);
    assert_eq!(remember_call.2.len(), 1);
    assert_eq!(
        remember_call.2[0].effect,
        ego_core::config::ManagedRuleEffect::Allow
    );
    assert_eq!(
        remember_call.2[0].rule,
        rule("read(/workspace/src/main.rs)")
    );
    assert_eq!(selection.snapshot().unwrap().mode, Mode::Default);
    assert_eq!(
        selection.staged_snapshot().unwrap().unwrap().mode,
        Mode::Edits
    );
}

#[test]
fn remember_confirmation_is_a_typed_policy_request_and_not_a_second_write_path() {
    let call = PreparedCall::new(
        "read",
        Op::Read {
            path: CanonPath::new("/workspace/src/main.rs"),
        },
    );
    let request = RememberRequest::new(
        call,
        ego_core::config::ManagedRuleEffect::Allow,
        CanonPath::new("/workspace"),
    );
    let store = Arc::new(RecordingStore::published(
        UserConfigPublication::PublishedDurably,
    ));
    let selection = GenerationPolicySelection::new(Mode::Default, Ruleset::default());
    let resolver = Arc::new(RecordingResolver {
        calls: Arc::new(Mutex::new(Vec::new())),
        next: ego_cli::repl::policy_controls::GenerationPolicySnapshot {
            mode: Mode::Plan,
            ruleset: Ruleset::default(),
        },
    });
    let owner = owner(selection.clone(), store.clone(), resolver.clone());
    let preview = match owner
        .apply(PolicyControlRequest::Permission(PermissionInput::Remember(
            request,
        )))
        .unwrap()
    {
        ego_cli::repl::policy_controls::PolicyControlOutcome::RememberPreview(preview) => preview,
        other => panic!("unexpected preview outcome: {other:?}"),
    };
    assert!(store.mutations().is_empty());
    let result = owner
        .apply(PolicyControlRequest::Permission(
            PermissionInput::ConfirmRemember(preview),
        ))
        .unwrap();
    assert!(matches!(
        result,
        ego_cli::repl::policy_controls::PolicyControlOutcome::Rule { .. }
    ));
    assert_eq!(store.mutations().len(), 1);
    assert_eq!(resolver.calls.lock().unwrap().len(), 1);
    assert_eq!(selection.snapshot().unwrap().mode, Mode::Default);
}

#[test]
fn admitted_permission_list_projects_rule_management_without_store_reads() {
    let store = Arc::new(RecordingStore::published(
        UserConfigPublication::NotPublished,
    ));
    let admitted_rule = rule("read(src/**)");
    let admitted = Ruleset::from_layer(
        ego_core::config::PermCfg {
            judge: None,
            allow: vec![admitted_rule],
            deny: vec![],
        },
        Source::Project,
    );
    let selection = GenerationPolicySelection::new(Mode::Default, admitted.clone());
    let owner = owner(
        selection,
        store.clone(),
        Arc::new(RecordingResolver {
            calls: Arc::new(Mutex::new(Vec::new())),
            next: ego_cli::repl::policy_controls::GenerationPolicySnapshot {
                mode: Mode::Default,
                ruleset: admitted.clone(),
            },
        }),
    );
    let result = owner.explain(PermissionInput::List).expect("admitted list");
    assert_eq!(store.reads(), 0);
    assert_eq!(
        result,
        ego_cli::repl::policy_controls::PermissionOutcome::List(
            ego_core::config::rule_management::RuleManagement::rows(&admitted)
        )
    );
}

#[test]
fn rule_mutations_revoke_and_remember_use_atomic_receipts_and_frozen_resolution() {
    let duplicate = rule("read(src/**)");
    let before = RuleSnapshot {
        target: UserConfigTarget::Base,
        permissions_present: true,
        allow: vec![duplicate.clone()],
        deny: vec![duplicate.clone()],
    };
    let after = RuleSnapshot {
        target: UserConfigTarget::Base,
        permissions_present: true,
        allow: vec![],
        deny: vec![],
    };
    let store = Arc::new(RecordingStore::transaction(
        UserConfigPublication::PublishedDurably,
        before.clone(),
        after.clone(),
    ));
    let resolver = Arc::new(RecordingResolver {
        calls: Arc::new(Mutex::new(Vec::new())),
        next: ego_cli::repl::policy_controls::GenerationPolicySnapshot {
            mode: Mode::Edits,
            ruleset: Ruleset::default(),
        },
    });
    let selection = GenerationPolicySelection::new(Mode::Default, Ruleset::default());
    let application = owner(selection.clone(), store.clone(), resolver.clone());
    let outcome = application
        .apply(PolicyControlRequest::Permission(PermissionInput::Revoke(
            duplicate.id(),
        )))
        .unwrap();
    assert!(matches!(
        outcome,
        ego_cli::repl::policy_controls::PolicyControlOutcome::Rule { .. }
    ));
    let calls = resolver.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0],
        (
            before,
            after,
            vec![
                RuleMutation {
                    effect: ego_core::config::ManagedRuleEffect::Allow,
                    rule: duplicate.clone()
                },
                RuleMutation {
                    effect: ego_core::config::ManagedRuleEffect::Deny,
                    rule: duplicate.clone()
                },
            ]
        )
    );
    assert!(selection.snapshot().unwrap().mode == Mode::Default);
    assert_eq!(
        selection.staged_snapshot().unwrap().unwrap().mode,
        Mode::Edits
    );

    let no_op_before = RuleSnapshot {
        target: UserConfigTarget::Base,
        permissions_present: true,
        allow: vec![],
        deny: vec![],
    };
    let no_op_store = Arc::new(RecordingStore::transaction(
        UserConfigPublication::NotPublished,
        no_op_before.clone(),
        no_op_before,
    ));
    let no_op_resolver = Arc::new(RecordingResolver {
        calls: Arc::new(Mutex::new(Vec::new())),
        next: ego_cli::repl::policy_controls::GenerationPolicySnapshot {
            mode: Mode::Edits,
            ruleset: Ruleset::default(),
        },
    });
    let no_op_selection = GenerationPolicySelection::new(Mode::Default, Ruleset::default());
    let no_op_owner = owner(no_op_selection.clone(), no_op_store, no_op_resolver.clone());
    no_op_owner
        .apply(PolicyControlRequest::Permission(PermissionInput::Revoke(
            duplicate.id(),
        )))
        .unwrap();
    assert!(no_op_resolver.calls.lock().unwrap().is_empty());
    assert!(no_op_selection.staged_snapshot().unwrap().is_none());

    let malformed_before = empty_rule_snapshot();
    let malformed_after = RuleSnapshot {
        target: UserConfigTarget::Base,
        permissions_present: true,
        allow: vec![rule("write(src/**)")],
        deny: vec![],
    };
    let malformed_store = Arc::new(RecordingStore::transaction(
        UserConfigPublication::PublishedDurably,
        malformed_before,
        malformed_after,
    ));
    let malformed_resolver = Arc::new(RecordingResolver {
        calls: Arc::new(Mutex::new(Vec::new())),
        next: ego_cli::repl::policy_controls::GenerationPolicySnapshot {
            mode: Mode::Edits,
            ruleset: Ruleset::default(),
        },
    });
    let malformed_selection = GenerationPolicySelection::new(Mode::Default, Ruleset::default());
    let malformed_owner = owner(
        malformed_selection.clone(),
        malformed_store,
        malformed_resolver.clone(),
    );
    assert!(
        malformed_owner
            .apply(PolicyControlRequest::Rule(RuleMutation {
                effect: ego_core::config::ManagedRuleEffect::Allow,
                rule: rule("read(src/**)"),
            }))
            .is_err()
    );
    assert!(malformed_resolver.calls.lock().unwrap().is_empty());
    assert!(malformed_selection.staged_snapshot().unwrap().is_none());

    let profile_before = RuleSnapshot {
        target: UserConfigTarget::Profile("ci".parse().unwrap()),
        permissions_present: true,
        allow: vec![],
        deny: vec![],
    };
    let profile_store = Arc::new(RecordingStore::transaction(
        UserConfigPublication::PublishedDurably,
        profile_before.clone(),
        RuleSnapshot {
            target: UserConfigTarget::Profile("ci".parse().unwrap()),
            permissions_present: true,
            allow: vec![rule("read(src/**)")],
            deny: vec![],
        },
    ));
    let profile_resolver = Arc::new(RecordingResolver {
        calls: Arc::new(Mutex::new(Vec::new())),
        next: ego_cli::repl::policy_controls::GenerationPolicySnapshot {
            mode: Mode::Edits,
            ruleset: Ruleset::default(),
        },
    });
    let profile_selection = GenerationPolicySelection::new(Mode::Default, Ruleset::default());
    let profile_owner = owner(
        profile_selection.clone(),
        profile_store,
        profile_resolver.clone(),
    );
    let profile_result = profile_owner.apply(PolicyControlRequest::Rule(RuleMutation {
        effect: ego_core::config::ManagedRuleEffect::Allow,
        rule: rule("read(src/**)"),
    }));
    assert!(matches!(
        profile_result,
        Err(PolicyControlError::PublicationContract { .. })
    ));
    assert!(profile_resolver.calls.lock().unwrap().is_empty());
    assert!(profile_selection.staged_snapshot().unwrap().is_none());
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
