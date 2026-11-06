//! contract for typed absence in the session generation controls.
//!
//! An unconfigured model and an unresolved reasoning default are different
//! from a concrete value. `chosen` is a separate operator-intent field: a
//! configured default may be current without being a human choice, and an
//! absent current must not be represented by an empty model id or `Off`.

use ego_core::config::{Mode, ModelRef};
use ego_core::control::{ControlSelect, SessionControlState};
use ego_core::generator::types::ThinkingLevel;
use ego_core::permit::Confinement;

#[test]
fn model_and_effort_absence_are_typed_and_independent_from_chosen() {
    let state = SessionControlState {
        model: ControlSelect {
            current: None,
            chosen: None,
            available: vec![ModelRef("ollama/qwen3".to_owned())],
        },
        effort: ControlSelect {
            current: None,
            chosen: None,
            available: vec![ThinkingLevel::Medium, ThinkingLevel::High],
        },
        mode: ControlSelect {
            current: Some(Mode::Default),
            chosen: None,
            available: vec![Mode::Default],
        },
        sandbox: ControlSelect {
            current: Some(Confinement::Workspace),
            chosen: None,
            available: vec![Confinement::Workspace],
        },
    };

    assert_eq!(state.model.current, None);
    assert_eq!(state.model.chosen, None);
    assert_eq!(state.effort.current, None);
    assert_eq!(state.effort.chosen, None);
    assert_eq!(state.mode.current, Some(Mode::Default));
    assert_eq!(state.sandbox.current, Some(Confinement::Workspace));
}

#[test]
fn a_configured_default_is_current_without_becoming_an_explicit_choice() {
    let state = SessionControlState {
        model: ControlSelect {
            current: Some(ModelRef("ollama/qwen3".to_owned())),
            chosen: None,
            available: vec![ModelRef("ollama/qwen3".to_owned())],
        },
        effort: ControlSelect {
            current: Some(ThinkingLevel::Medium),
            chosen: None,
            available: vec![ThinkingLevel::Medium, ThinkingLevel::High],
        },
        mode: ControlSelect {
            current: Some(Mode::Default),
            chosen: None,
            available: vec![Mode::Default],
        },
        sandbox: ControlSelect {
            current: Some(Confinement::Workspace),
            chosen: None,
            available: vec![Confinement::Workspace],
        },
    };

    assert_eq!(
        state.model.current,
        Some(ModelRef("ollama/qwen3".to_owned()))
    );
    assert_eq!(state.model.chosen, None);
    assert_eq!(state.effort.current, Some(ThinkingLevel::Medium));
    assert_eq!(state.effort.chosen, None);
}

#[test]
fn an_explicit_effort_remains_distinct_from_a_resolved_current_value() {
    let state = SessionControlState {
        model: ControlSelect {
            current: Some(ModelRef("ollama/qwen3".to_owned())),
            chosen: Some(ModelRef("ollama/qwen3".to_owned())),
            available: vec![ModelRef("ollama/qwen3".to_owned())],
        },
        effort: ControlSelect {
            current: Some(ThinkingLevel::High),
            chosen: Some(ThinkingLevel::High),
            available: vec![ThinkingLevel::Medium, ThinkingLevel::High],
        },
        mode: ControlSelect {
            current: Some(Mode::Default),
            chosen: None,
            available: vec![Mode::Default],
        },
        sandbox: ControlSelect {
            current: Some(Confinement::Workspace),
            chosen: None,
            available: vec![Confinement::Workspace],
        },
    };

    assert_eq!(state.effort.current, state.effort.chosen);
    assert_eq!(state.model.current, state.model.chosen);
}
