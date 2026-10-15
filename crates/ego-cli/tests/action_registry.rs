//! contract for the single, final interactive action registry.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ego_cli::repl::actions::{
    ActionDispatch, ActionId, ActionKind, ActionRegistration, ActionRegistry, ActionSink,
    Availability, CompositeActionSink, Dispatch, InputOrigin, LocalAction, LocalActionOutcome,
};
use ego_cli::repl::selector::{Selector, SelectorKey, SelectorKind};

const NAMES: [&str; 29] = [
    "help",
    "model",
    "thinking",
    "mode",
    "plan",
    "yolo",
    "permissions",
    "mcp",
    "sessions",
    "fork",
    "compact",
    "clear",
    "usage",
    "statusline",
    "export",
    "config",
    "setup",
    "doctor",
    "quit",
    "refresh",
    "new",
    "name",
    "goal",
    "tasks",
    "tools",
    "expand",
    "skills",
    "hooks",
    "version",
];

#[test]
fn final_surface_has_exact_names_in_contract_order_and_stable_ids() {
    // Catches: missing public commands, reordered completion, or changed slash identities.
    let registry = ActionRegistry::final_surface();
    assert_eq!(registry.names(), NAMES);
    assert_eq!(registry.id("help"), Some(ActionId::Help));
    assert_eq!(registry.id("model"), Some(ActionId::Model));
    assert_eq!(registry.id("thinking"), Some(ActionId::Thinking));
    assert_eq!(registry.id("statusline"), Some(ActionId::Statusline));
    assert_eq!(registry.id("config"), Some(ActionId::Config));
    assert_eq!(registry.id("hooks"), Some(ActionId::Hooks));
    assert_eq!(registry.id("version"), Some(ActionId::Version));
    assert_eq!(registry.len(), NAMES.len());
}

#[test]
fn bindings_help_completion_and_dispatch_project_the_same_registration_id() {
    let registry = ActionRegistry::final_surface();
    for registration in registry.iter() {
        let id = registration.id();
        assert_eq!(registry.resolve_slash(registration.name()), Some(id));
        for alias in registration.aliases() {
            assert_eq!(registry.resolve_alias(alias), Some(id));
        }
        for shortcut in registration.shortcuts() {
            assert_eq!(registry.resolve_shortcut(*shortcut), Some(id));
        }
        assert_eq!(registry.help(id).id(), id);
        assert_eq!(registry.completion(id).id(), id);
        assert_eq!(registry.dispatch_projection(id).id(), id);
    }
}

#[test]
fn construction_rejects_duplicate_ids_names_aliases_and_shortcuts() {
    let duplicate_id = [
        ActionRegistration::new(ActionId::Help, "help", ActionKind::Local),
        ActionRegistration::new(ActionId::Help, "other", ActionKind::Local),
    ];
    assert!(ActionRegistry::try_from(duplicate_id).is_err());

    let duplicate_name = [
        ActionRegistration::new(ActionId::Help, "same", ActionKind::Local),
        ActionRegistration::new(ActionId::Model, "same", ActionKind::Local),
    ];
    assert!(ActionRegistry::try_from(duplicate_name).is_err());

    let duplicate_alias = [
        ActionRegistration::new(ActionId::Help, "help", ActionKind::Local).alias("h"),
        ActionRegistration::new(ActionId::Model, "model", ActionKind::Local).alias("h"),
    ];
    assert!(ActionRegistry::try_from(duplicate_alias).is_err());

    let duplicate_shortcut = [
        ActionRegistration::new(ActionId::Help, "help", ActionKind::Local).shortcut('h'),
        ActionRegistration::new(ActionId::Model, "model", ActionKind::Local).shortcut('h'),
    ];
    assert!(ActionRegistry::try_from(duplicate_shortcut).is_err());
}

#[test]
fn mcp_tools_and_doctor_are_enabled_when_the_shared_handlers_are_installed() {
    let registry = ActionRegistry::final_surface();
    for id in [ActionId::Mcp, ActionId::Tools, ActionId::Doctor] {
        assert_eq!(registry.availability(id), Availability::Enabled);
        assert!(registry.help(id).disabled_reason().is_none());
    }
}

#[test]
fn menu_display_is_not_authorization_and_dispatch_rechecks_current_snapshot() {
    let registry = ActionRegistry::final_surface();
    let menu = registry.menu("m");
    assert_eq!(menu.first().map(|entry| entry.id()), Some(ActionId::Model));

    let stale = registry.snapshot();
    let current = registry.with_availability(ActionId::Model, Availability::Busy);
    assert_eq!(
        current.dispatch(ActionId::Model, &stale),
        Dispatch::Refused {
            reason: "action is no longer available",
        }
    );
}

#[test]
fn busy_routing_keeps_local_controls_immediate_and_prompt_actions_on_ingress() {
    let registry = ActionRegistry::final_surface();
    assert_eq!(registry.kind(ActionId::Model), ActionKind::Local);
    assert_eq!(registry.kind(ActionId::Statusline), ActionKind::Local);
}

struct RecordingSink {
    calls: Vec<LocalAction>,
}

impl ActionSink for RecordingSink {
    fn invoke(&mut self, action: LocalAction) -> LocalActionOutcome {
        self.calls.push(action);
        LocalActionOutcome::Completed
    }
}

#[test]
fn dispatch_reaches_the_injected_local_sink_and_prompt_ingress_without_handler_presence() {
    let registry = ActionRegistry::final_surface();
    let displayed = registry.snapshot();
    let selection = registry
        .select(ActionId::Model, &displayed)
        .expect("displayed model is selectable");
    let mut sink = RecordingSink { calls: Vec::new() };
    assert_eq!(
        registry.dispatch_to(selection, &displayed, true, &mut sink),
        ActionDispatch::Local(LocalActionOutcome::Completed)
    );
    assert_eq!(
        sink.calls,
        vec![LocalAction {
            id: ActionId::Model,
            busy: true
        }]
    );

    let changed = registry.with_availability(ActionId::Model, Availability::Busy);
    assert_eq!(
        changed.dispatch_to(selection, &changed.snapshot(), false, &mut sink),
        ActionDispatch::Refused {
            reason: "action is no longer available",
        }
    );

    let prompt_registry = ActionRegistry::try_from([ActionRegistration::new(
        ActionId::Version,
        "prompt-producing-test",
        ActionKind::PromptProducing,
    )])
    .unwrap();
    let prompt_snapshot = prompt_registry.snapshot();
    let prompt_selection = prompt_registry
        .select(ActionId::Version, &prompt_snapshot)
        .expect("displayed prompt action is selectable");
    assert_eq!(
        prompt_registry.dispatch_to(prompt_selection, &prompt_snapshot, true, &mut sink),
        ActionDispatch::Prompt(ego_cli::repl::actions::PromptPayload {
            id: ActionId::Version
        })
    );
}

#[test]
fn tools_slash_dispatch_uses_the_shared_handler_and_preserves_catalogue_identity() {
    let registry = ActionRegistry::final_surface();
    assert_eq!(registry.resolve_slash("/tools"), Some(ActionId::Tools));
    assert_eq!(
        registry.availability(ActionId::Tools),
        Availability::Enabled
    );

    let displayed = registry.snapshot();
    let selection = registry
        .select(ActionId::Tools, &displayed)
        .expect("/tools is selectable when its shared handler is installed");
    let mut sink = CompositeActionSink::new();
    let recording = RecordingSink { calls: Vec::new() };
    sink.register(ActionId::Tools, Box::new(recording))
        .expect("the /tools handler is registered once");
    assert_eq!(
        registry.dispatch_to(selection, &displayed, false, &mut sink),
        ActionDispatch::Local(LocalActionOutcome::Completed)
    );
}

#[test]
fn typed_and_bracketed_paste_origins_are_distinct_even_with_identical_text() {
    let registry = ActionRegistry::final_surface();
    let typed = registry.submit("/model", InputOrigin::Typed);
    let pasted = registry.submit("/model", InputOrigin::BracketedPaste);
    assert_eq!(typed.text(), pasted.text());
    assert_eq!(typed.origin(), InputOrigin::Typed);
    assert_eq!(pasted.origin(), InputOrigin::BracketedPaste);
    assert_eq!(registry.classify(typed), Dispatch::Action(ActionId::Model));
    assert_eq!(registry.classify(pasted), Dispatch::Prompt);
}

#[test]
fn mcp_arguments_reach_the_host_instead_of_the_provider() {
    let registry = ActionRegistry::final_surface();

    assert_eq!(
        registry.classify(registry.submit("/mcp add", InputOrigin::Typed)),
        Dispatch::Mcp("add".into())
    );
}

#[test]
fn one_selector_supports_single_multi_free_and_numbered_fallback() {
    for kind in [SelectorKind::Single, SelectorKind::Multiple] {
        let mut selector = Selector::new(kind, ["alpha", "beta"]);
        assert_eq!(
            selector.key(SelectorKey::Number(2)).selected(),
            Some("beta")
        );
        assert!(selector.key(SelectorKey::Enter).consumed());
        assert!(selector.key(SelectorKey::Escape).consumed());
    }
    let mut free_text = Selector::free_text();
    free_text.insert("answer");
    assert!(free_text.key(SelectorKey::Enter).consumed());
    assert_eq!(free_text.value(), "answer");
}

#[test]
fn modal_enter_and_escape_are_exclusive_and_restore_exact_buffer_and_cursor() {
    let mut selector = Selector::new(SelectorKind::Single, ["alpha", "beta"]);
    selector.preserve_composer("draft\nwith paste", 7);
    assert!(selector.key(SelectorKey::Enter).consumed());
    assert!(selector.key(SelectorKey::Escape).consumed());
    assert_eq!(selector.composer(), ("draft\nwith paste", 7));
    assert!(selector.did_not_submit_composer());
}

#[test]
fn model_shortcuts_and_slash_are_the_same_action() {
    let registry = ActionRegistry::final_surface();
    let meta_p = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::META);
    let alt_p = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::ALT);
    assert_eq!(registry.resolve_key(meta_p), Some(ActionId::Model));
    assert_eq!(registry.resolve_key(alt_p), Some(ActionId::Model));
    assert_eq!(
        registry.resolve_key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE)),
        None
    );
    assert_eq!(
        registry.resolve_key(KeyEvent::new(KeyCode::Char('π'), KeyModifiers::NONE)),
        Some(ActionId::Model)
    );
    assert_eq!(registry.resolve_slash("model"), Some(ActionId::Model));
    assert_ne!(
        registry.resolve_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE)),
        Some(ActionId::Model)
    );
    assert_ne!(
        registry.resolve_key(KeyEvent::new(
            KeyCode::Char('p'),
            KeyModifiers::ALT | KeyModifiers::CONTROL,
        )),
        Some(ActionId::Model)
    );
}

#[test]
fn resume_accepts_repeatable_answer_flags_without_splitting_json_values() {
    let invocation = ego_cli::cli::parse(vec![
        "ego".into(),
        "resume".into(),
        "0192f3a1-0000-7000-8000-000000000001".into(),
        "--answer".into(),
        r#"0192f3a1-0000-7000-8000-000000000002="yes""#.into(),
        "--answer".into(),
        r#"0192f3a1-0000-7000-8000-000000000003=["a","b"]"#.into(),
    ])
    .unwrap();
    let ego_cli::cli::Invocation::Resume(args) = invocation else {
        panic!("repeatable --answer belongs to resume");
    };
    assert_eq!(args.answers.len(), 2);
    assert_eq!(
        args.answers[0].id.to_string(),
        "0192f3a1-0000-7000-8000-000000000002"
    );
    assert_eq!(args.answers[0].json, r#""yes""#);
    assert_eq!(args.answers[1].json, r#"["a","b"]"#);

    assert!(
        ego_cli::cli::parse(vec![
            "ego".into(),
            "resume".into(),
            "0192f3a1-0000-7000-8000-000000000001".into(),
            "--answer".into(),
            "not-an-item=\"yes\"".into(),
        ])
        .is_err()
    );
    assert!(
        ego_cli::cli::parse(vec![
            "ego".into(),
            "resume".into(),
            "0192f3a1-0000-7000-8000-000000000001".into(),
            "--answer".into(),
            "0192f3a1-0000-7000-8000-000000000002=not-json".into(),
        ])
        .is_err()
    );
}
