//! RED contract for M4 S4: one typed hook catalogue and durable runner.
//!
//! Core receives already projected, source-bound hook values. YAML envelope
//! extraction belongs to the CLI boundary and is covered by its crate-private
//! projection test; this suite never feeds raw skill YAML to core.

use std::sync::{Arc, Mutex};

use ego_core::config::{ConfigLayer, Source, Sourced, Trust};
use ego_core::event::hook::{
    HookDecision, HookFailure, HookFailureKind, HookInvocationId, HookLifecycle, HookOutcome,
    HookSubject,
};
use ego_core::event::id::{ItemId, TurnId};
use ego_core::event::skill::SkillInvocationId;
use ego_core::hook::{
    HookCatalogue, HookCatalogueError, HookConfigEntry, HookEvent,
    HookHandlerConfig, HookMatcher, HookPort, HookSpec, HookSource, HookFuture,
    HookRecordFuture, HookRunner,
};
use ego_core::permit::rule::Rule;

fn layer(source: Source, toml: &str) -> Sourced<ConfigLayer> {
    ConfigLayer::parse(source, toml).expect("valid hook layer")
}

fn persistent_layers() -> [Sourced<ConfigLayer>; 3] {
    [
        layer(Source::System, "[[hooks]]\nevent = \"PreToolUse\"\n"),
        layer(Source::User, "[[hooks]]\nevent = \"PostToolUse\"\n"),
        layer(Source::Project, "[[hooks]]\nevent = \"PermissionDenied\"\n"),
    ]
}

fn typed_skill_hook() -> HookConfigEntry {
    HookConfigEntry::new(
        HookLifecycle::SubagentStop,
        Some(HookMatcher::Permission(
            Rule::load("read(src/**)").expect("canonical permission rule"),
        )),
        HookHandlerConfig::Command {
            command: "/usr/bin/skill-hook".to_owned(),
            args: Vec::new(),
            timeout_seconds: None,
        },
    )
}

#[test]
fn typed_sources_and_skill_projection_make_one_strict_source_bound_catalogue() {
    let layers = persistent_layers();
    let skill = HookSource::skill_entries(
        "skills/demo/SKILL.md",
        SkillInvocationId::new(),
        [typed_skill_hook()],
    );
    let catalogue = HookCatalogue::admit_typed(&layers, &[skill], Trust::Trusted)
        .expect("complete typed catalogue");
    assert_eq!(catalogue.sources(), ["system", "user", "project", "skill"]);
    assert_eq!(catalogue.specs().len(), 4);
    assert!(catalogue.specs().iter().all(|spec| spec.location().is_authority_relative()));
    assert!(catalogue.specs().iter().all(|spec| spec.content_digest().is_lower_sha256()));
    assert!(catalogue.specs().windows(2).all(|pair| pair[0].sort_key() < pair[1].sort_key()));
    assert!(!catalogue.serialized_config().contains("/usr/bin"));
}

#[test]
fn exactly_21_events_are_admitted_and_exact_other_12_names_refuse() {
    let admitted = [
        "SessionStart", "InstructionsLoaded", "UserPromptSubmit", "UserPromptExpansion",
        "PreToolUse", "PermissionRequest", "PermissionDenied", "PostToolUse",
        "PostToolUseFailure", "PostToolBatch", "PreModelSwitch", "PostModelSwitch",
        "SubagentStart", "SubagentStop", "Stop", "StopFailure", "PreCompact", "PostCompact",
        "Elicitation", "ElicitationResult", "SessionEnd",
    ];
    let refused = [
        "Setup", "MessageDisplay", "Notification", "TaskCreated", "TaskCompleted",
        "TeammateIdle", "ConfigChange", "CwdChanged", "DirectoryAdded", "FileChanged",
        "WorktreeCreate", "WorktreeRemove",
    ];
    assert_eq!(admitted.len(), 21);
    assert_eq!(refused.len(), 12);
    for event in admitted {
        let spec = HookSpec::from_wire(event, "command", "/usr/bin/hook").expect("admitted event");
        assert_eq!(spec.event_name(), event);
    }
    for event in refused {
        let error = HookSpec::from_wire(event, "command", "/usr/bin/hook").unwrap_err();
        assert_eq!(error.exact_name(), event);
        assert!(matches!(error, HookCatalogueError::UnsupportedEvent { .. }));
    }
}

#[test]
fn closed_handlers_reject_exact_unknown_fields_and_admit_http_without_adapter() {
    for field in ["statusMessage", "async", "asyncRewake", "agent", "output", "regex"] {
        let error = HookSpec::from_fields("PreToolUse", [(field, "value")]).unwrap_err();
        assert_eq!(error.exact_name(), field);
    }
    let http = HookHandlerConfig::Http {
        url: "https://hooks.example.test/notify".to_owned(),
        headers: Vec::new(),
        allowed_env_vars: Vec::new(),
        timeout_seconds: None,
    };
    let spec = HookSpec::from_typed(
        HookConfigEntry::new(HookLifecycle::PostToolUse, None, http),
        HookSource::User,
        "hooks/user.toml",
        0,
    )
    .expect("catalogue admits closed HTTP handler before S5 adapter");
    assert_eq!(spec.event_name(), "PostToolUse");
}

#[derive(Clone, Default)]
struct DurableHookPort {
    started: Arc<Mutex<Vec<HookInvocationId>>>,
    settled: Arc<Mutex<Vec<HookInvocationId>>>,
    emitted: Arc<Mutex<Vec<HookEvent>>>,
    fail: Arc<Mutex<Option<HookFailureKind>>>,
}

impl DurableHookPort {
    fn fail_with(kind: HookFailureKind) -> Self {
        Self { fail: Arc::new(Mutex::new(Some(kind))), ..Self::default() }
    }
}

impl HookPort for DurableHookPort {
    fn started<'a>(
        &'a self,
        _spec: &'a HookSpec,
        _event: &'a HookEvent,
        invocation: HookInvocationId,
    ) -> HookRecordFuture<'a> {
        self.started.lock().expect("started journal").push(invocation);
        Box::pin(async { Ok(()) })
    }

    fn invoke<'a>(&'a self, _spec: &'a HookSpec, event: HookEvent) -> HookFuture<'a> {
        self.emitted.lock().expect("hook emissions").push(event.clone().originated_by_hook());
        if let Some(kind) = self.fail.lock().expect("failure").take() {
            return Box::pin(async move { Err(HookFailure::new(kind, None)) });
        }
        Box::pin(async move { Ok(HookOutcome::new(HookDecision::Continue, None)) })
    }

    fn settled<'a>(
        &'a self,
        _spec: &'a HookSpec,
        invocation: HookInvocationId,
        _outcome: Result<&'a HookOutcome, &'a HookFailure>,
    ) -> HookRecordFuture<'a> {
        self.settled.lock().expect("settled journal").push(invocation);
        Box::pin(async { Ok(()) })
    }
}

fn typed_catalogue() -> HookCatalogue {
    let layers = persistent_layers();
    HookCatalogue::admit_typed(
        &layers,
        &[HookSource::skill_entries(
            "skills/demo/SKILL.md",
            SkillInvocationId::new(),
            [typed_skill_hook()],
        )],
        Trust::Trusted,
    )
    .expect("typed catalogue")
}

#[test]
fn deterministic_matching_uses_permission_ast_and_valid_block_wins_with_durable_feedback() {
    let catalogue = typed_catalogue();
    let event = HookEvent::tool(
        HookLifecycle::PreToolUse,
        ego_core::event::hook::HookSubject::Item { item: ItemId::new() },
        "read(src/main.rs)",
    );
    let first = catalogue.matching(&event);
    let second = catalogue.matching(&event);
    assert_eq!(first, second);
    assert!(first.windows(2).all(|pair| pair[0].sort_key() < pair[1].sort_key()));
    let port = DurableHookPort::default();
    let runner = HookRunner::new(port.clone());
    let result = futures::executor::block_on(runner.run(&catalogue, event)).expect("hook run");
    assert_eq!(result.decision(), HookDecision::Block);
    assert!(result.feedback_bytes() <= 4096);
    assert!(!port.started.lock().expect("starts").is_empty());
    assert_eq!(port.started.lock().expect("starts").len(), port.settled.lock().expect("settles").len());
}

#[test]
fn pre_effect_failure_blocks_observer_failure_stays_durable_and_effect_is_not_reverted() {
    let catalogue = typed_catalogue();
    let pre = futures::executor::block_on(HookRunner::new(DurableHookPort::fail_with(
        HookFailureKind::HandlerFailed,
    )).run(&catalogue, HookEvent::session(HookLifecycle::PreCompact))).expect("pre result");
    assert!(pre.is_blocked());
    let post = futures::executor::block_on(HookRunner::new(DurableHookPort::fail_with(
        HookFailureKind::HandlerFailed,
    )).run(&catalogue, HookEvent::session(HookLifecycle::PostCompact))).expect("post result");
    assert!(post.is_observer_failure());
    assert!(post.is_durable());
    assert!(!post.reverted_effect());
}

#[test]
fn durable_once_rebuilds_in_a_new_runner_and_hook_originated_events_are_suppressed() {
    let catalogue = typed_catalogue();
    let port = DurableHookPort::default();
    let event = HookEvent::turn(HookLifecycle::SubagentStop, HookSubject::Turn { turn: TurnId::new() });
    let first = futures::executor::block_on(HookRunner::new_with_durable_started(
        port.clone(), port.started.clone(),
    ).run(&catalogue, event.clone())).expect("first once invocation");
    assert!(first.started_invocation());
    let rebuilt = HookRunner::new_with_durable_started(port.clone(), port.started.clone());
    let second = futures::executor::block_on(rebuilt.run(&catalogue, event)).expect("replayed once");
    assert!(second.was_skipped_once());
    let emitted = port.emitted.lock().expect("emitted events").clone();
    assert!(emitted.iter().all(HookRunner::<DurableHookPort>::hook_originated_event));
    let suppressed = futures::executor::block_on(rebuilt.run(
        &catalogue,
        HookEvent::session(HookLifecycle::SessionStart).originated_by_hook(),
    )).expect("suppressed recursive event");
    assert!(!suppressed.started_invocation());
}

#[test]
fn catalogue_reload_builds_a_real_candidate_then_atomically_keeps_last_good() {
    let mut catalogue = typed_catalogue();
    let before = catalogue.generation().to_owned();
    let invalid = vec![HookConfigEntry::new(
            HookLifecycle::PostToolUse,
            None,
            HookHandlerConfig::Command { command: String::new(), args: Vec::new(), timeout_seconds: None },
        )];
    assert!(catalogue.reload_typed(invalid, Trust::Trusted).is_err());
    assert_eq!(catalogue.generation(), before);
    catalogue
        .reload_typed(vec![typed_skill_hook()], Trust::Trusted)
        .expect("candidate validates before publication");
    assert_ne!(catalogue.generation(), before);
}

#[test]
fn persistent_once_is_refused_and_candidate_duplicates_are_not_published() {
    assert!(matches!(
        HookCatalogue::admit_typed(
            &[layer(Source::User, "[[hooks]]\nevent = \"PostToolUse\"\nonce = true\n")],
            &[],
            Trust::Trusted,
        ),
        Err(HookCatalogueError::Invalid { exact_name, .. }) if exact_name == "once"
    ));
    let duplicate = HookCatalogue::admit_typed(
        &[],
        &[HookSource::skill_entries(
            "skills/demo/SKILL.md",
            SkillInvocationId::new(),
            [typed_skill_hook(), typed_skill_hook()],
        )],
        Trust::Trusted,
    );
    assert!(duplicate.is_err());
}
