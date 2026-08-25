//! RED contract for terminal question control grammar.

use ego_cli::repl::selector::{Selector, SelectorKey, SelectorKind};

#[test]
fn single_multiple_free_and_numbered_fallback_are_terminal_only_controls() {
    let mut single = Selector::new(SelectorKind::Single, ["a", "b"]);
    assert_eq!(single.key(SelectorKey::Number(2)).selected(), Some("b"));

    let mut multiple = Selector::new(SelectorKind::Multiple, ["a", "b"]);
    assert_eq!(multiple.key(SelectorKey::Number(1)).selected(), Some("a"));
    assert_eq!(multiple.key(SelectorKey::Space).selected(), None);

    let mut free = Selector::free_text();
    free.insert("typed answer");
    assert_eq!(free.key(SelectorKey::Enter).selected(), Some("typed answer"));
}

#[test]
fn escape_restores_the_original_composer_without_submitting_or_mutating_drafts() {
    let mut selector = Selector::new(SelectorKind::Single, ["a", "b"]);
    selector.preserve_composer("draft bytes", 5);
    let (composer_text, composer_cursor) = selector.composer();
    let composer = (composer_text.to_owned(), composer_cursor);
    let result = selector.key(SelectorKey::Escape);
    assert!(result.consumed());
    assert_eq!(selector.composer(), (composer.0.as_str(), composer.1));
    assert!(selector.did_not_submit_composer());
}
