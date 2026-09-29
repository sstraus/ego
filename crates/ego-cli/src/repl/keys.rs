//! The keys ego decides about, before the editor's own keymap sees them.
//!
//! Kept as a function over `(KeyCode, KeyModifiers)` — the two types reedline
//! re-exports — rather than over its `ReedlineRawEvent`. Constructing one of
//! those in a test needs `crossterm::event::Event`, which reedline does *not*
//! re-export, so a test would have to depend on crossterm pinned to whatever
//! version reedline resolved. That pin is a maintenance trap for no gain: the
//! part it would cover is one `Event::Key` destructure, and the part it would
//! not cover is every rule below.

use reedline::{KeyCode, KeyModifiers};

/// What ego does with a key before the editor does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Intercept {
    /// Not ours. The editor's own keymap decides, and ego never sees it.
    Editor,
    /// `Ctrl+V`: inspect the OS pasteboard and stage its image, or insert its text.
    ClipboardImage,
    /// Save or restore one exact composer draft.
    Draft,
    /// Submit the live composer through the durable coordinator.
    SubmitBusy,
    /// Expedite the durable pending-steer prefix.
    Interrupt,
}

/// Decide a key.
///
/// `Ctrl+V` is taken with or without `Shift`, because both spellings mean
/// "paste" to the person pressing them and neither is bound: plain `Ctrl+V`
/// is unbound in reedline's Emacs keymap, and `Ctrl+Shift+V` is bound only
/// under the `system_clipboard` feature, which this build does not enable.
///
/// `Alt` disqualifies the match. A person holding a third modifier is asking
/// for a different binding, and swallowing it here would make that binding
/// impossible to add later without this function silently eating it first.
pub(super) fn intercept(code: KeyCode, modifiers: KeyModifiers) -> Intercept {
    let pressed_v = matches!(code, KeyCode::Char(key) if key.eq_ignore_ascii_case(&'v'));
    // Shift is allowed through; every other modifier must be absent.
    let only_control = modifiers.difference(KeyModifiers::SHIFT) == KeyModifiers::CONTROL;

    let pressed_s = matches!(code, KeyCode::Char(key) if key.eq_ignore_ascii_case(&'s'));

    if pressed_v && only_control {
        Intercept::ClipboardImage
    } else if pressed_s && only_control {
        Intercept::Draft
    } else if code == KeyCode::Enter && modifiers.is_empty() {
        Intercept::SubmitBusy
    } else if code == KeyCode::Esc && modifiers.is_empty() {
        Intercept::Interrupt
    } else {
        Intercept::Editor
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_v_is_egos_key_and_not_the_editors() {
        assert_eq!(
            intercept(KeyCode::Char('v'), KeyModifiers::CONTROL),
            Intercept::ClipboardImage
        );
    }

    /// A terminal that reports the shifted spelling reports an upper-case
    /// char, and a person who pressed the shortcut they use for paste every
    /// day meant the same thing either way.
    #[test]
    fn the_shifted_spelling_of_paste_is_the_same_request() {
        assert_eq!(
            intercept(
                KeyCode::Char('V'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            ),
            Intercept::ClipboardImage
        );
    }

    #[test]
    fn a_v_that_is_just_a_v_is_typed_and_not_intercepted() {
        assert_eq!(
            intercept(KeyCode::Char('v'), KeyModifiers::NONE),
            Intercept::Editor
        );
    }

    #[test]
    fn durable_surface_actions_are_named_before_the_editor_sees_them() {
        assert_eq!(
            intercept(KeyCode::Char('s'), KeyModifiers::CONTROL),
            Intercept::Draft
        );
        assert_eq!(
            intercept(KeyCode::Enter, KeyModifiers::NONE),
            Intercept::SubmitBusy
        );
        assert_eq!(
            intercept(KeyCode::Esc, KeyModifiers::NONE),
            Intercept::Interrupt
        );
    }

    /// Swallowing a third modifier here would make `Ctrl+Alt+V` impossible to
    /// bind later without this function eating it first, and the failure would
    /// look like a key that does nothing rather than a key that is taken.
    #[test]
    fn a_third_modifier_asks_for_a_binding_this_is_not() {
        assert_eq!(
            intercept(
                KeyCode::Char('v'),
                KeyModifiers::CONTROL | KeyModifiers::ALT
            ),
            Intercept::Editor
        );
    }

    /// The editor's own control keys have to keep working. `Ctrl+C` and
    /// `Ctrl+D` are how a person abandons a line and ends a session, and this
    /// function is upstream of both.
    #[test]
    fn the_keys_that_end_a_line_or_a_session_are_left_alone() {
        for key in ['c', 'd', 'a', 'e', 'k', 'u', 'w'] {
            assert_eq!(
                intercept(KeyCode::Char(key), KeyModifiers::CONTROL),
                Intercept::Editor,
                "Ctrl+{key}"
            );
        }
    }

    #[test]
    fn nothing_that_is_not_a_character_is_taken() {
        for code in [KeyCode::Enter, KeyCode::Tab, KeyCode::Esc, KeyCode::Up] {
            assert_eq!(
                intercept(code, KeyModifiers::CONTROL),
                Intercept::Editor,
                "{code:?}"
            );
        }
    }
}
