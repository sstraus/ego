//! RED contract for the live Reedline action ingress.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ego_cli::repl::actions::{ActionId, ActionRegistry, Dispatch, InputOrigin};
use ego_cli::repl::selector::{Selector, SelectorKey, SelectorKind};

#[test]
fn meta_p_and_typed_model_slash_share_one_action_id_in_the_registry() {
    let registry = ActionRegistry::final_surface();
    let shortcut = registry
        .resolve_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::META))
        .expect("Meta-P is registered");
    let slash = registry
        .resolve_slash("/model")
        .expect("/model is registered");
    assert_eq!(shortcut, ActionId::Model);
    assert_eq!(shortcut, slash);
    assert_eq!(
        registry.classify(registry.submit("/model", InputOrigin::Typed)),
        Dispatch::Action(ActionId::Model)
    );
}

#[test]
fn pasted_slash_text_remains_prompt_text_for_the_local_model_entry() {
    let registry = ActionRegistry::final_surface();
    assert_eq!(
        registry.classify(registry.submit("/model", InputOrigin::BracketedPaste)),
        Dispatch::Prompt
    );
}

#[test]
fn modal_selector_consumes_enter_and_escape_without_touching_the_composer() {
    let mut selector = Selector::new(SelectorKind::Single, ["openai/gpt-5.6"]);
    selector.preserve_composer("draft /model", 6);
    let (before_text, before_cursor) = selector.composer();
    let before = (before_text.to_owned(), before_cursor);
    assert!(selector.key(SelectorKey::Enter).consumed());
    assert!(selector.key(SelectorKey::Escape).consumed());
    assert_eq!(selector.composer(), (before.0.as_str(), before.1));
    assert!(selector.did_not_submit_composer());
}
