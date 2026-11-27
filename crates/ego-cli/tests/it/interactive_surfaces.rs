//! Public process-target hook capture assertion for M4 S8.
//!
//! Registry-owned skill routing, rescan transactions, and private catalogue
//! view tests live beside their owners in `src/repl` and `src/skill`.

use ego_core::config::Trust;
use ego_core::event::hook::HookLifecycle;
use ego_core::event::skill::{SkillInvocationId, SkillSourceAuthority};
use ego_core::hook::{HookCatalogue, HookConfigEntry, HookHandlerConfig, HookSource};

#[test]
fn hook_catalogue_capture_keeps_active_skill_scope_and_redacts_handler_payload() {
    let invocation = SkillInvocationId::new();
    let source = HookSource::skill_entries_with_authority(
        "skills/explain/SKILL.md",
        invocation,
        SkillSourceAuthority::User,
        [HookConfigEntry::new(
            HookLifecycle::PreToolUse,
            None,
            HookHandlerConfig::Prompt {
                prompt: "do not disclose this handler payload".to_owned(),
                model: None,
                timeout_seconds: None,
            },
        )
        .with_once(true)],
    );
    let catalogue = HookCatalogue::admit_typed(&[], &[source], Trust::Trusted)
        .expect("active skill hook catalogue");
    let hook = catalogue.specs().first().expect("one active skill hook");

    assert_eq!(hook.skill_invocation(), Some(invocation));
    assert_eq!(hook.event_name(), "PreToolUse");
    assert!(matches!(hook.source(), HookSource::Skill { .. }));
    assert!(!catalogue.serialized_config().contains("handler payload"));
}
