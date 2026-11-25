//! contract for the live Reedline action ingress.

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

// Catches: mobile capitalization rejects builtin commands or argument-bearing aliases.
#[test]
fn mixed_case_builtin_slash_commands_keep_their_local_dispatch() {
    let registry = ActionRegistry::final_surface();
    for (input, expected) in [
        ("/Clear", Dispatch::Action(ActionId::Clear)),
        ("/CLEAR", Dispatch::Action(ActionId::Clear)),
        ("/CONFIG perimeter", Dispatch::Config("perimeter".into())),
        ("/ReSuMe", Dispatch::Resume(None)),
        ("/DeTaCh", Dispatch::Tasks("/detach".into())),
    ] {
        assert_eq!(
            registry.classify(registry.submit(input, InputOrigin::Typed)),
            expected,
            "{input:?}"
        );
    }
}

// Catches: lowercasing the whole submission corrupts model names and configuration values.
#[test]
fn mixed_case_builtin_slash_commands_preserve_argument_case() {
    let registry = ActionRegistry::final_surface();
    for (input, expected) in [
        (
            "/CoNfIg set model \"Vendor/MixedCase\"",
            Dispatch::Config("set model \"Vendor/MixedCase\"".into()),
        ),
        (
            "/MoDeL Vendor/MixedCase",
            Dispatch::ModelQuery("Vendor/MixedCase".into()),
        ),
        (
            "/SiDe Read /Path/WithCase",
            Dispatch::Side("Read /Path/WithCase".into()),
        ),
    ] {
        assert_eq!(
            registry.classify(registry.submit(input, InputOrigin::Typed)),
            expected,
            "{input:?}"
        );
    }
}

// Catches: case normalization executes pasted commands or breaks the literal-path escape.
#[test]
fn mixed_case_slash_prompt_text_keeps_its_input_boundary() {
    let registry = ActionRegistry::final_surface();
    for (input, origin) in [
        ("/Clear", InputOrigin::BracketedPaste),
        ("/CONFIG perimeter", InputOrigin::BracketedPaste),
        (" /Clear", InputOrigin::Typed),
    ] {
        let submission = registry.submit(input, origin);
        assert_eq!(submission.text(), input);
        assert_eq!(registry.classify(submission), Dispatch::Prompt, "{input:?}");
    }
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
