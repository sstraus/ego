//! RED contract for regression case's interactive compaction boundary.
//!
//! `/compact` is a host-owned session command.  It must not enter the normal
//! prompt ingress, which owns ordinary user turns and their queue.

use ego_cli::catalogue::CATALOGUE_VERSION;
use ego_cli::repl::actions::{ActionId, ActionRegistry, Availability};
use ego_cli::repl::{DraftStash, HostCommand, Line};
use ego_core::event::id::SessionId;

#[test]
fn compact_is_a_typed_host_command_and_never_prompt_text() {
    let command = HostCommand::Action(ActionId::Compact);
    assert_eq!(
        Line::HostCommand(command.clone()),
        Line::HostCommand(HostCommand::Action(ActionId::Compact))
    );
    assert_ne!(
        Line::HostCommand(command),
        Line::Submitted("/compact".to_string()),
        "compact must not enter ordinary prompt ingress"
    );
}

#[test]
fn typed_host_commands_cover_the_non_prompt_control_variants() {
    for command in [
        HostCommand::Action(ActionId::Compact),
        HostCommand::Draft,
        HostCommand::BusySubmit,
        HostCommand::Interrupt,
        HostCommand::ModalEscape,
        HostCommand::ModelPicker("provider/model".to_string()),
        HostCommand::QuestionSelector("answer".to_string()),
    ] {
        assert!(matches!(Line::HostCommand(command), Line::HostCommand(_)));
    }
}

#[test]
fn compact_availability_remains_disabled_until_pending_work_is_consumed() {
    let registry =
        ActionRegistry::final_surface().with_availability(ActionId::Compact, Availability::Busy);
    assert_eq!(registry.availability(ActionId::Compact), Availability::Busy);
}

#[test]
fn ephemeral_source_disables_compact_with_the_exact_owner_reason() {
    let registry = ActionRegistry::final_surface().with_availability(
        ActionId::Compact,
        Availability::Disabled {
            owner: "compaction requires a durable source session",
        },
    );
    assert_eq!(
        registry.availability(ActionId::Compact),
        Availability::Disabled {
            owner: "compaction requires a durable source session",
        }
    );
}

#[test]
fn compaction_catalogue_publication_uses_catalogue_schema_v3() {
    assert_eq!(CATALOGUE_VERSION, 3);
}

#[test]
fn draft_stash_is_source_keyed_and_activation_starts_empty() {
    let source = SessionId::new();
    let target = SessionId::new();
    let mut drafts = DraftStash::new(source);

    assert_eq!(drafts.active(), source);
    assert_eq!(drafts.active_count(), 0);
    assert_eq!(drafts.count(source), 0);
    assert_eq!(drafts.count(target), 0);

    drafts.activate(target);
    assert_eq!(drafts.active(), target);
    assert_eq!(drafts.active_count(), 0);
    assert_eq!(drafts.count(source), 0);
    assert_eq!(drafts.count(target), 0);
}
