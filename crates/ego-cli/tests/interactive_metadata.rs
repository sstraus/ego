//! contract for interactive session metadata commands.
use ego_cli::cli::{GoalOperation, NameOperation, SessionMetadataOperation};
use ego_cli::repl::actions::{
    ActionId, ActionKind, ActionRegistry, Availability, Dispatch, InputOrigin,
};

#[test]
fn metadata_actions_are_local_and_typed_commands_parse_once() {
    let registry = ActionRegistry::final_surface();
    assert_eq!(registry.kind(ActionId::Name), ActionKind::Local);
    assert_eq!(registry.kind(ActionId::Goal), ActionKind::Local);
    assert_eq!(registry.availability(ActionId::Name), Availability::Enabled);
    assert_eq!(registry.availability(ActionId::Goal), Availability::Enabled);
    for (text, expected) in [
        ("/name", SessionMetadataOperation::Name(NameOperation::Show)),
        (
            "/name show",
            SessionMetadataOperation::Name(NameOperation::Show),
        ),
        (
            "/name set project alpha",
            SessionMetadataOperation::Name(NameOperation::Set("project alpha".into())),
        ),
        (
            "/name clear",
            SessionMetadataOperation::Name(NameOperation::Clear),
        ),
        ("/goal", SessionMetadataOperation::Goal(GoalOperation::Show)),
        (
            "/goal extend 3",
            SessionMetadataOperation::Goal(GoalOperation::Extend(3)),
        ),
        (
            "/goal show",
            SessionMetadataOperation::Goal(GoalOperation::Show),
        ),
        (
            "/goal set objective",
            SessionMetadataOperation::Goal(GoalOperation::Set("objective".into())),
        ),
        (
            "/goal complete the mockup",
            SessionMetadataOperation::Goal(GoalOperation::Set("complete the mockup".into())),
        ),
        (
            "/goal achieve",
            SessionMetadataOperation::Goal(GoalOperation::Achieve),
        ),
        (
            "/goal block API unavailable",
            SessionMetadataOperation::Goal(GoalOperation::Block("API unavailable".into())),
        ),
    ] {
        assert_eq!(
            registry.classify(registry.submit(text, InputOrigin::Typed)),
            Dispatch::SessionMetadata(expected)
        );
    }
    for text in [
        "/name set",
        "/goal set",
        "/name nope",
        "/name clear extra",
        "/goal achieve extra",
        "/goal block",
        "/goal extend",
        "/goal extend 0",
        "/goal extend -1",
        "/goal extend 4294967296",
        "/goal extend 1 extra",
    ] {
        assert!(matches!(
            registry.classify(registry.submit(text, InputOrigin::Typed)),
            Dispatch::Refused { .. }
        ));
    }
    assert!(matches!(
        registry.classify(registry.submit("/name set title", InputOrigin::BracketedPaste)),
        Dispatch::Prompt
    ));
}
