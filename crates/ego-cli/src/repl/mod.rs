//! The reading half of the terminal surface.
//!
//! The writing half is [`crate::render`]. They are never live at the same
//! time, and that is the point rather than an implementation detail:
//! `ARCHITECTURE.md` R12 says exactly one component writes to the terminal,
//! and a line editor is a writer — it paints a prompt, redraws on every
//! keystroke and clears what it drew. So the editor is asked for one line and
//! then dropped, the turn renders with no editor in existence, and the next
//! line gets a fresh one. See
//! `docs/decisions/2026-08-22-the-seat-reads-before-the-screen-writes.md` §2.

mod keys;
mod seat;

pub use seat::Seat;

use reedline::Signal;

/// Why `Ctrl+V` cannot do what `CLI.md` §6.1 says it will.
///
/// §6.1 says `Ctrl+V` asks the platform pasteboard for an image first and text
/// second. The image half has nowhere to go: it is the same gap that refuses
/// `--attach` in [`crate::cli`], so both close on the same day and this text
/// says so rather than making the reader find that out.
///
/// It refuses instead of quietly pasting text, because a key that silently
/// does something other than what the document promises cannot be discovered,
/// and one that refuses by name can.
pub const CLIPBOARD_IMAGE_WAITING_ON: &str = "a non-text content block — the \
     same gap that refuses `--attach`. The conversation model carries text, \
     tool calls and tool results, so an image read from the pasteboard has no \
     representation to be sent in. Name the file instead, once `--attach` \
     lands";

/// What one read of the input line produced.
///
/// The editor's own vocabulary is turned into this before anything else looks
/// at it, so the loop's decisions are made over a type this crate owns and can
/// test without a terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    /// A prompt to run. Bracketed paste is on, so this may hold newlines and
    /// still be one prompt.
    Submitted(String),
    /// Enter on a line with nothing on it. Not a turn: a turn costs money, and
    /// this says nothing to spend it on.
    Blank,
    /// `Ctrl+C`. Abandon the line, keep the session.
    Interrupted,
    /// `Ctrl+D` on an empty line, or the terminal went away.
    Ended,
    /// A key bound to something this build does not have, named.
    Refused(&'static str),
}

impl Line {
    /// Read the editor's answer as one of ours.
    ///
    /// [`Line::Refused`] is not produced here, and deliberately cannot be.
    /// reedline 0.44's `ExecuteHostCommand` exits as `Signal::Success(text)`
    /// — the same arm a person typing that exact text produces — so a
    /// sentinel string would be a key that a prompt could impersonate. The
    /// refusal travels on its own channel instead ([`keys::Intercept`]), and
    /// the reader consults that before this.
    pub fn of(signal: Signal) -> Self {
        match signal {
            Signal::Success(text) if text.trim().is_empty() => Self::Blank,
            Signal::Success(text) => Self::Submitted(text),
            Signal::CtrlC => Self::Interrupted,
            Signal::CtrlD => Self::Ended,
        }
    }
}

/// Turn one read into a [`Line`], key first and buffer second.
///
/// The ordering is the refusal: `Ctrl+V` exits the read carrying whatever had
/// already been typed, so consulting the buffer first would submit a
/// half-written prompt as though Enter had been pressed.
fn answer(asked_for_image: bool, signal: std::io::Result<Signal>) -> Line {
    if asked_for_image {
        return Line::Refused(CLIPBOARD_IMAGE_WAITING_ON);
    }
    match signal {
        Ok(signal) => Line::of(signal),
        // A read that fails is a terminal that went away: there is no line to
        // retype and nobody left to retype it.
        Err(_) => Line::Ended,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pasted_stack_trace_is_one_prompt_and_not_its_first_line() {
        let pasted = "thread 'main' panicked at src/main.rs:4:5:\nindex out of bounds\nnote: run with RUST_BACKTRACE=1";

        let line = Line::of(Signal::Success(pasted.to_string()));

        assert_eq!(line, Line::Submitted(pasted.to_string()));
    }

    /// The refusal is a factual claim about this build, so it names the thing
    /// that is actually missing rather than saying "unsupported". `cli.rs`'s
    /// `INTERACTIVE_WAITING_ON` records why that matters: a reason nothing
    /// checks decays into a confident falsehood the day the gap is closed.
    #[test]
    fn the_refusal_names_the_missing_piece_and_the_flag_that_shares_it() {
        assert!(
            CLIPBOARD_IMAGE_WAITING_ON.contains("content block"),
            "{CLIPBOARD_IMAGE_WAITING_ON}"
        );
        assert!(
            CLIPBOARD_IMAGE_WAITING_ON.contains("--attach"),
            "the same gap refuses `--attach`, and saying so is how a reader \
             finds the one story that closes both: {CLIPBOARD_IMAGE_WAITING_ON}"
        );
    }

    #[test]
    fn ctrl_c_abandons_the_line_and_keeps_the_session() {
        assert_eq!(Line::of(Signal::CtrlC), Line::Interrupted);
    }

    #[test]
    fn ctrl_d_ends_the_session() {
        assert_eq!(Line::of(Signal::CtrlD), Line::Ended);
    }

    /// Nothing is spent on a prompt that says nothing. A blank submission is a
    /// real event — the person pressed Enter — but it is not a turn, and a
    /// turn is what costs money.
    #[test]
    fn a_line_with_nothing_on_it_is_not_a_turn() {
        for blank in ["", "   ", "\t", "\n", "  \n\t "] {
            assert_eq!(
                Line::of(Signal::Success(blank.to_string())),
                Line::Blank,
                "{blank:?}"
            );
        }
    }

    /// The key is answered before the buffer is, and that ordering is the
    /// whole refusal. `Ctrl+V` exits the read with whatever had already been
    /// typed, so reading the buffer first would submit a half-written prompt
    /// as if the person had pressed Enter — spending money on a turn they
    /// never asked for.
    #[test]
    fn the_key_is_answered_before_whatever_was_already_typed() {
        let half_written = Ok(Signal::Success("explain this err".to_string()));

        match answer(true, half_written) {
            Line::Refused(why) => assert_eq!(why, CLIPBOARD_IMAGE_WAITING_ON),
            other => panic!("Ctrl+V must refuse and submit nothing, not {other:?}"),
        }
    }

    #[test]
    fn without_the_key_the_buffer_is_just_a_prompt() {
        assert_eq!(
            answer(false, Ok(Signal::Success("hello".to_string()))),
            Line::Submitted("hello".to_string())
        );
    }

    /// A read that fails is a terminal that went away. There is no line to
    /// retype and no one to retype it, so it ends the session rather than
    /// looping on an error forever.
    #[test]
    fn a_terminal_that_went_away_ends_the_session() {
        let broken = Err(std::io::Error::other("the terminal went away"));

        assert_eq!(answer(false, broken), Line::Ended);
    }

    /// The one thing that must never be confusable with a refusal: a person
    /// typing text that *looks* like an internal signal is still just a
    /// prompt. reedline 0.44 returns a host command as `Signal::Success`, so
    /// if the refusal ever moved onto a sentinel string this test is where it
    /// would be caught.
    #[test]
    fn a_prompt_cannot_impersonate_a_key() {
        for typed in ["ego:paste-image", "Ctrl+V", "\\x16"] {
            assert_eq!(
                Line::of(Signal::Success(typed.to_string())),
                Line::Submitted(typed.to_string()),
                "{typed:?}"
            );
        }
    }
}
