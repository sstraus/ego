//! RED contract for M4 S4: one strict hook catalogue and deterministic runner.
//!
//! Configuration is intentionally expressed at the existing TOML/YAML
//! boundaries.  The final implementation must project those inputs into one
//! source-bound `HookSpec` catalogue and execute it through one `Hook` port.

use ego_core::config::{ConfigLayer, Source, Sourced, Trust};
use ego_core::event::hook::{HookDecision, HookLifecycle, HookOutcome};
use ego_core::event::id::{ItemId, TurnId};
use ego_core::event::hook::HookSubject;
use ego_core::hook::{
    HookCatalogue, HookCatalogueError, HookEvent, HookHandler, HookPort, HookRunner, HookSpec,
    HookSource, HookTrigger,
};

fn layer(source: Source, toml: &str) -> Sourced<ConfigLayer> {
    ConfigLayer::parse(source, toml).expect("valid hook layer")
}

fn system_layer() -> Sourced<ConfigLayer> {
    layer(
        Source::System,
        r#"
[[hooks]]
event = "PreToolUse"
matcher = "exec"
[hooks.handler]
type = "command"
command = "/usr/bin/system-hook"
args = []
timeout = 30
"#,
    )
}

fn user_layer() -> Sourced<ConfigLayer> {
    layer(
        Source::User,
        r#"
[[hooks]]
event = "PostToolUse"
[hooks.handler]
type = "prompt"
timeout = 30
"#,
    )
}

fn project_layer() -> Sourced<ConfigLayer> {
    layer(
        Source::Project,
        r#"
[[hooks]]
event = "PermissionDenied"
[hooks.handler]
type = "mcp_tool"
server = "trusted"
tool = "audit"
"#,
    )
}

fn skill_yaml() -> &'static str {
    r#"
hooks:
  - event: SubagentStop
    matcher: ""
    once: true
    handler:
      type: command
      command: /usr/bin/skill-hook
      args: []
"#
}

#[test]
fn strict_sources_and_skill_hooks_project_into_one_source_bound_catalogue() {
    let catalogue = HookCatalogue::admit(
        &[system_layer(), user_layer(), project_layer()],
        &[HookSource::skill("skills/demo/SKILL.md", skill_yaml(), "invocation-1")],
        Trust::Trusted,
    )
    .expect("complete catalogue");
    assert_eq!(catalogue.sources(), ["system", "user", "project", "skill"]);
    assert_eq!(catalogue.specs().len(), 4);
    assert_eq!(catalogue.specs()[0].source(), HookSource::System);
    assert!(catalogue.specs()[3].source().is_skill());
    assert!(catalogue.specs().iter().all(|spec| spec.location().is_authority_relative()));
    assert!(catalogue.specs().iter().all(|spec| spec.content_digest().is_lower_sha256()));
    assert!(catalogue.specs().iter().all(|spec| spec.ordinal().is_some()));
    assert!(!catalogue.serialized_config().contains("command"));
    assert!(!catalogue.serialized_config().contains("server"));
}

#[test]
fn exactly_the_ratified_21_events_are_admitted_and_the_other_12_are_refused_by_name() {
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
fn unknown_deprecated_experimental_async_and_display_fields_fail_closed_by_exact_name() {
    for field in ["statusMessage", "async", "asyncRewake", "agent", "output", "regex"] {
        let error = HookSpec::from_fields("PreToolUse", [(field, "value")]).unwrap_err();
        assert_eq!(error.exact_name(), field);
    }
    let nested_claude_map = "PreToolUse:\n  - hooks:\n      - type: command\n";
    let error = HookCatalogue::from_claude_map(nested_claude_map).unwrap_err();
    assert_eq!(error.exact_name(), "PreToolUse");
}

#[test]
fn catalogue_order_and_matchers_are_deterministic_and_any_valid_block_wins() {
    let catalogue = HookCatalogue::admit(
        &[system_layer(), user_layer(), project_layer()],
        &[HookSource::skill("skills/demo/SKILL.md", skill_yaml(), "invocation-1")],
        Trust::Trusted,
    )
    .unwrap();
    let event = HookEvent::tool(
        HookLifecycle::PreToolUse,
        HookSubject::Item { item: ItemId::new() },
        "exec",
    );
    let first = catalogue.matching(&event);
    let second = catalogue.matching(&event);
    assert_eq!(first, second);
    assert!(first.windows(2).all(|pair| pair[0].sort_key() < pair[1].sort_key()));

    let runner = HookRunner::new(RecordingHookPort::default());
    let result = futures::executor::block_on(runner.run(&catalogue, event)).expect("hook run");
    assert_eq!(result.decision(), HookDecision::Block);
    assert!(result.feedback_bytes() <= 4096);
    assert_eq!(
        runner.call_order(),
        first.iter().map(|spec| spec.id()).collect::<Vec<_>>()
    );
}

#[test]
fn pre_effect_failure_blocks_and_observer_failure_is_durable_without_reverting_effect() {
    let catalogue = HookCatalogue::test_catalogue();
    let runner = HookRunner::new(RecordingHookPort::with_failures([
        HookFailureAt::PreEffect,
        HookFailureAt::Observer,
    ]));
    let pre = futures::executor::block_on(runner.run(
        &catalogue,
        HookEvent::tool(
            HookLifecycle::PreToolUse,
            HookSubject::Item { item: ItemId::new() },
            "exec",
        ),
    ))
    .unwrap();
    assert!(pre.is_blocked());
    assert!(!pre.effect_was_started());

    let post = futures::executor::block_on(runner.run(
        &catalogue,
        HookEvent::tool(
            HookLifecycle::PostToolUse,
            HookSubject::Item { item: ItemId::new() },
            "exec",
        ),
    ))
    .unwrap();
    assert!(post.is_observer_failure());
    assert!(post.effect_was_started());
    assert!(post.is_durable());
    assert!(!post.reverted_effect());
}

#[test]
fn hook_originated_work_does_not_recurse_into_the_same_runner() {
    let catalogue = HookCatalogue::test_catalogue();
    let runner = HookRunner::new(RecordingHookPort::emits_hook_event());
    let result = futures::executor::block_on(runner.run(
        &catalogue,
        HookEvent::session(HookLifecycle::SessionStart),
    ))
    .unwrap();
    assert_eq!(result.recursive_invocation_count(), 0);
    assert_eq!(runner.max_depth(), 1);
}

#[test]
fn once_is_consumed_at_durable_start_only_for_skill_scope_and_survives_crash() {
    let catalogue = HookCatalogue::test_catalogue_with_once();
    let runner = HookRunner::new(RecordingHookPort::default());
    let subject = HookSubject::Turn { turn: TurnId::new() };
    let event = HookEvent::turn(HookLifecycle::SubagentStop, subject);
    let first = futures::executor::block_on(runner.run(&catalogue, event.clone())).unwrap();
    assert!(first.started_invocation());
    runner.crash_after_start();
    let second = futures::executor::block_on(runner.run(&catalogue, event)).unwrap();
    assert!(second.was_skipped_once());
    assert_eq!(runner.started_count(), 1);
    assert!(catalogue.persistent_once().is_err());
}

#[test]
fn atomic_reload_publishes_only_complete_valid_catalogues_and_retains_last_good() {
    let mut catalogue = HookCatalogue::admit(
        &[system_layer(), user_layer(), project_layer()],
        &[],
        Trust::Trusted,
    )
    .unwrap();
    let before = catalogue.generation();
    let before_ids = catalogue.specs().iter().map(HookSpec::id).collect::<Vec<_>>();
    let invalid = [
        "unknown event",
        "duplicate identity",
        "missing handler owner",
        "untrusted project",
    ];
    for candidate in invalid {
        let error = catalogue.reload(candidate).unwrap_err();
        assert_eq!(catalogue.generation(), before);
        assert_eq!(catalogue.specs().iter().map(HookSpec::id).collect::<Vec<_>>(), before_ids);
        assert_eq!(error.candidate_published(), false);
    }
    let _ = catalogue.reload_valid(system_layer(), user_layer(), project_layer());
    assert_ne!(catalogue.generation(), before);
}

#[test]
fn matcher_uses_exact_event_native_selectors_and_bounded_source_tagged_feedback() {
    let spec = HookSpec::new(
        HookLifecycle::PreToolUse,
        Some("exec(git commit *)"),
        HookHandler::command("/usr/bin/guard", []),
    )
    .with_source(HookSource::User, "hooks/user.toml", 0);
    assert!(spec.matches(&HookEvent::tool(
        HookLifecycle::PreToolUse,
        HookSubject::Item { item: ItemId::new() },
        "exec(git commit main)",
    )));
    assert!(!spec.matches(&HookEvent::tool(
        HookLifecycle::PreToolUse,
        HookSubject::Item { item: ItemId::new() },
        "exec(git push)",
    )));
    assert!(HookTrigger::from_javascript_regex("/commit/").is_err());
    let feedback = spec.feedback("blocked");
    assert_eq!(feedback.source(), HookSource::User);
    assert!(feedback.bytes() <= 4096);
}

#[test]
fn model_switch_hooks_match_the_canonical_target_and_do_not_fire_recursively() {
    let target = ego_core::config::ModelRef("provider/target".to_owned());
    let before = HookEvent::model_switch(HookLifecycle::PreModelSwitch, target.clone());
    let after = HookEvent::model_switch(HookLifecycle::PostModelSwitch, target);
    assert_eq!(before.model_target(), after.model_target());
    assert_eq!(before.lifecycle(), HookLifecycle::PreModelSwitch);
    assert_eq!(after.lifecycle(), HookLifecycle::PostModelSwitch);
    assert!(!HookRunner::hook_originated_event(&before));
    assert!(!HookRunner::hook_originated_event(&after));
}

#[derive(Default)]
struct RecordingHookPort;

impl RecordingHookPort {
    fn with_failures<const N: usize>(_failures: [HookFailureAt; N]) -> Self {
        Self
    }

    fn emits_hook_event() -> Self {
        Self
    }
}

impl HookPort for RecordingHookPort {
    fn invoke<'a>(
        &'a self,
        _spec: &'a HookSpec,
        event: HookEvent,
    ) -> ego_core::hook::HookFuture<'a> {
        let decision = if event.lifecycle() == HookLifecycle::PreToolUse {
            HookDecision::Block
        } else {
            HookDecision::Continue
        };
        Box::pin(async move { Ok(HookOutcome::new(decision, None)) })
    }
}

#[derive(Debug, Clone, Copy)]
enum HookFailureAt {
    PreEffect,
    Observer,
}
