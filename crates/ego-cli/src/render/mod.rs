//! The renderer: the only place in this workspace that knows a terminal has a
//! colour, a width, or an opinion about escape sequences.
//!
//! Only `output.rs` writes terminal streams. Renderers return [`line::Line`] or `String`
//! values and provide no second door to stdout.
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
pub(crate) mod replay;
mod shell;
pub(crate) mod startup_logo;
pub mod stream;
pub mod style;
pub mod table;
pub mod tool_row;

pub use caps::{Capabilities, Depth, OscProfile};
pub use line::{Line, Span};
pub use stream::Answer;
pub use style::{Palette, Rgb, Style};

/// A turn duration a person reads, with whole seconds below one hour.
pub(crate) fn duration(elapsed: std::time::Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {:02}m", seconds / 3600, seconds % 3600 / 60)
    }
}

/// Render a block of lines to bytes, one `\n` per line.
/// Strip trailing padding for [`Depth::None`]. Diff padding carries a
/// background in terminals but has no meaning in plain redirected text.
pub fn block(lines: &[Line], depth: Depth) -> String {
    block_with(
        lines,
        Capabilities {
            depth,
            ..Capabilities::plain()
        },
    )
}

/// Render an answer block with the negotiated hyperlink capability.
pub fn block_with(lines: &[Line], capabilities: Capabilities) -> String {
    let depth = capabilities.depth;
    let osc = osc::Osc::new(capabilities);
    let mut out = String::new();
    for line in lines {
        if depth == Depth::None {
            out.push_str(line.render_with(depth, &osc).trim_end());
        } else {
            out.push_str(&line.render_with(depth, &osc));
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
