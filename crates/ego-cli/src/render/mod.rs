//! The renderer: the only place in this workspace that knows a terminal has a
//! colour, a width, or an opinion about escape sequences.
//!
//! `ARCHITECTURE.md` §6 ratifies a line-oriented renderer with an ownership
//! contract (R12): *exactly one component writes to the terminal*. This module
//! is that component's vocabulary; `output.rs` is the component. Nothing here
//! writes to a stream — every function returns a [`line::Line`] or a `String`
//! — because a renderer that could write would be a second door to stdout, and
//! `output.rs` owns the only one.
//!
//! `ego-core`, `ego-ai` and `ego-tools` depend on none of this and must not:
//! a `PublicEvent` is a fact about a run, and how a fact looks is the CLI's
//! business. That is the same boundary `sink.rs` already draws, restated here
//! because it is the boundary a rendering layer is most likely to cross.
//!
//! Layout of the module, and why it is in this order:
//!
//! - [`caps`] decides once what the terminal can be told.
//! - [`style`] names colours in 24 bits and spells them at the depth
//!   [`caps`] allowed. Nothing else formats an SGR.
//! - [`line`] measures and wraps in *cells*. Everything below produces
//!   `Line`s, never strings with escapes already in them.
//! - [`table`], [`diff`], [`markdown`] are the three shapes.
//! - [`tool_row`] is a fourth: one tool call as a compact row and, behind the
//!   expand key, its detail.
//! - [`stream`] applies [`markdown`] to text that is still arriving.
//! - [`osc`] is the out-of-band half: what ego tells the host that is not
//!   text on the screen.

pub mod caps;
pub mod diff;
pub mod line;
pub mod markdown;
pub mod osc;
pub mod permission_diff;
pub mod stream;
pub mod style;
pub mod table;
pub mod tool_row;

pub use caps::{Capabilities, Depth, OscProfile};
pub use line::{Line, Span};
pub use stream::Answer;
pub use style::{Palette, Rgb, Style};

/// Render a block of lines to bytes, one `\n` per line.
///
/// The one place trailing whitespace is removed. It matters more than it
/// looks: a diff pads every row to the full width so its background reaches
/// the right edge, and the ADR §2 rules that the piped form is a *different*
/// rendering rather than the coloured one with escapes stripped — precisely
/// because that padding would survive stripping and produce a file of ragged
/// blanks. At [`Depth::None`] there is no background to carry, so the padding
/// has nothing left to do and goes.
pub fn block(lines: &[Line], depth: Depth) -> String {
    let mut out = String::new();
    for line in lines {
        if depth == Depth::None {
            out.push_str(line.text().trim_end());
        } else {
            out.push_str(&line.render(depth));
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pipe_gets_no_escapes_and_no_ragged_blanks() {
        let mut padded = Line::plain("text");
        padded.pad_styled(20, Style::bg(Palette::ADDED_BG));
        let rendered = block(&[padded], Depth::None);
        assert_eq!(rendered, "text\n");
        assert!(!rendered.contains('\x1b'));
    }

    /// The same line on a screen keeps the padding, because the padding *is*
    /// the background reaching the edge.
    #[test]
    fn a_screen_keeps_the_padding_that_carries_the_background() {
        let mut padded = Line::plain("text");
        padded.pad_styled(20, Style::bg(Palette::ADDED_BG));
        let rendered = block(&[padded], Depth::TrueColor);
        assert!(rendered.contains("\x1b[48;2;2;40;0m"));
        assert!(rendered.ends_with("\x1b[0m\n"));
    }
}
