//! Two files, on two axes.
//!
//! `docs/decisions/2026-08-22-two-axes-and-a-terminal-that-answers.md` §2 is
//! the whole of this module's shape, and it was read off a capture rather than
//! invented: the diff writes **background** and only background; a foreground
//! pass writes foreground. In the captured renderer that is what lets
//! `(CancelCause)` stay Monokai white on a lighter green while a doc comment
//! on the next line stays comment-grey on a darker one. A design where the
//! diff sets the text colour has to choose, every line, between saying *this
//! changed* and saying *this is a comment*.
//!
//! There is no syntax pass in this build — [`Highlighter`] has one
//! implementation and it returns the base colour. That is deliberate and it is
//! the reason the seam exists now: adding one later must not touch a line of
//! this file, and the only way to be sure of that is to write this file
//! against the seam.

use similar::{ChangeTag, InlineChange, TextDiff};

use super::line::{Line, Span, cells};
use super::style::{Palette, Rgb, Style};

/// Foreground for a line of source, as a sequence of runs.
///
/// The unit is a run and not a token because a run is what a renderer can use
/// without knowing what a token is, and because the null implementation is
/// then one run rather than a lexer that returns everything as `Other`.
pub trait Highlighter {
    /// Runs covering `text` exactly, in order. The concatenation of the run
    /// texts must equal `text`; a highlighter that drops a byte drops it from
    /// the screen.
    fn runs(&self, text: &str) -> Vec<(String, Rgb)>;
}

/// Every line in one colour. What ships today.
pub struct Unhighlighted;

impl Highlighter for Unhighlighted {
    fn runs(&self, text: &str) -> Vec<(String, Rgb)> {
        vec![(text.to_string(), Palette::CODE_FG)]
    }
}

/// How much unchanged text to show around a change.
pub const CONTEXT: usize = 3;

/// Render a change to one file.
///
/// `width` is the column the background should reach; on a pipe
/// [`super::block`] trims the padding back off, which is why the padding can
/// be unconditional here.
pub fn render(old: &str, new: &str, width: usize, highlighter: &dyn Highlighter) -> Vec<Line> {
    let diff = TextDiff::from_lines(old, new);
    // The gutter is as wide as the largest number either side will show, so a
    // 4-digit file does not shift the whole body two columns in at the hunk
    // where it crosses 1000.
    let digits = cells(&old.lines().count().max(new.lines().count()).to_string()).max(2);
    let gutter = digits + 2;
    let content_width = width.saturating_sub(gutter).max(8);

    let mut out = Vec::new();
    let groups = diff.grouped_ops(CONTEXT);
    for (position, group) in groups.iter().enumerate() {
        if position > 0 {
            // A gap between hunks, named rather than blank: a blank line in a
            // diff reads as an unchanged empty line in the file.
            out.push(Line::styled("⋯".repeat(3), Style::fg(Palette::GUTTER_FG)));
        }
        for op in group {
            for change in diff.iter_inline_changes(op) {
                out.push(row(&change, digits, content_width, highlighter));
            }
        }
    }
    out
}

fn row(
    change: &InlineChange<'_, str>,
    digits: usize,
    content_width: usize,
    highlighter: &dyn Highlighter,
) -> Line {
    let (sign, number, gutter_fg, background, span_background) = match change.tag() {
        ChangeTag::Delete => (
            '-',
            change.old_index(),
            Palette::REMOVED_FG,
            Some(Palette::REMOVED_BG),
            Palette::REMOVED_SPAN_BG,
        ),
        ChangeTag::Insert => (
            '+',
            change.new_index(),
            Palette::ADDED_FG,
            Some(Palette::ADDED_BG),
            Palette::ADDED_SPAN_BG,
        ),
        // Context carries no background at all — not a "neutral" one. A
        // background on every row turns the diff into a solid block and the
        // two that mean something stop standing out.
        ChangeTag::Equal => (
            ' ',
            change.new_index(),
            Palette::GUTTER_FG,
            None,
            Palette::GUTTER_FG,
        ),
    };

    let mut line = Line::empty();
    let gutter_style = match background {
        Some(colour) => Style::fg(gutter_fg).with_bg(colour),
        // Dim, matching the captured renderer: the number of an unchanged line
        // is orientation, not information.
        None => Style::fg(gutter_fg).dim(),
    };
    let label = match number {
        Some(index) => format!("{:>digits$}", index + 1, digits = digits),
        None => " ".repeat(digits),
    };
    line.push(Span::new(format!("{label} {sign}"), gutter_style));

    let base = |emphasised: bool| -> Option<Rgb> {
        match (background, emphasised) {
            (Some(_), true) => Some(span_background),
            (colour, _) => colour,
        }
    };

    for (emphasised, value) in change.iter_strings_lossy() {
        let text = value.trim_end_matches(['\n', '\r']);
        // The foreground comes from the highlighter and the background from
        // the diff. Neither knows about the other; this loop is the only place
        // they meet, and it is three lines long by design.
        for (run, colour) in highlighter.runs(text) {
            let mut style = Style::fg(colour);
            if let Some(colour) = base(emphasised) {
                style = style.with_bg(colour);
            }
            line.push(Span::new(run, style));
        }
    }

    // Reach the right edge, so the background is a band rather than a ragged
    // stripe. `super::block` removes this again at `Depth::None`.
    if let Some(colour) = background {
        line.pad_styled(content_width + digits + 2, Style::bg(colour));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::caps::Depth;

    /// A highlighter that colours one word, so a test can prove the two axes
    /// stay independent without a real lexer.
    struct OneWord;

    const MARKED: Rgb = Rgb(166, 226, 46);

    impl Highlighter for OneWord {
        fn runs(&self, text: &str) -> Vec<(String, Rgb)> {
            match text.split_once("fn") {
                Some((before, after)) => vec![
                    (before.to_string(), Palette::CODE_FG),
                    ("fn".to_string(), MARKED),
                    (after.to_string(), Palette::CODE_FG),
                ],
                None => vec![(text.to_string(), Palette::CODE_FG)],
            }
        }
    }

    fn rendered(old: &str, new: &str) -> Vec<String> {
        render(old, new, 40, &Unhighlighted)
            .iter()
            .map(Line::text)
            .collect()
    }

    #[test]
    fn the_sign_and_the_number_say_which_side_a_line_came_from() {
        let out = rendered("a\nb\nc\n", "a\nB\nc\n");
        let trimmed: Vec<String> = out.iter().map(|line| line.trim_end().to_string()).collect();
        assert_eq!(
            trimmed,
            vec![" 1  a", " 2 -b", " 2 +B", " 3  c"],
            "{out:#?}"
        );
    }

    /// The claim §2 is built on. The changed span gets a *lighter background*
    /// and keeps whatever foreground the highlighter chose — here, a colour
    /// for `fn` that no diff code knows about.
    #[test]
    fn a_changed_span_keeps_its_syntax_colour() {
        let lines = render("fn old() {}\n", "fn new() {}\n", 40, &OneWord);
        let inserted = lines
            .iter()
            .find(|line| line.text().contains('+'))
            .expect("there is an inserted line");
        let bytes = inserted.render(Depth::TrueColor);

        // Foreground: the highlighter's, untouched by the diff.
        assert!(
            bytes.contains("38;2;166;226;46"),
            "syntax foreground survived: {bytes:?}"
        );
        // Background: the diff's, in both shades — the line and the span.
        assert!(bytes.contains("48;2;2;40;0"), "line background: {bytes:?}");
        assert!(
            bytes.contains("48;2;4;71;0"),
            "changed-span background: {bytes:?}"
        );
    }

    /// The inverse, and the reason the two shades exist: a line where
    /// everything changed and a line where one word did must not look the
    /// same.
    #[test]
    fn a_word_level_change_is_narrower_than_the_line_it_is_in() {
        let lines = render(
            "the quick brown fox\n",
            "the quick red fox\n",
            60,
            &Unhighlighted,
        );
        let inserted = lines
            .iter()
            .find(|line| line.text().contains('+'))
            .expect("there is an inserted line");
        let emphasised: usize = inserted
            .spans
            .iter()
            .filter(|span| span.style.bg == Some(Palette::ADDED_SPAN_BG))
            .map(|span| cells(&span.text))
            .sum();
        assert!(
            (1..=4).contains(&emphasised),
            "only the changed word is emphasised, not {emphasised} cells"
        );
    }

    /// Context rows carry no background. A diff where every row is filled is a
    /// block of colour in which the two rows that mean something disappear.
    #[test]
    fn context_is_left_alone() {
        let lines = render("a\nb\nc\n", "a\nB\nc\n", 40, &Unhighlighted);
        let context = lines
            .iter()
            .find(|line| line.text().trim_end().ends_with(" a"))
            .expect("the first line is context");
        assert!(context.spans.iter().all(|span| span.style.bg.is_none()));
    }

    /// The padding is what makes the background a band. It is also what would
    /// fill a redirected file with blanks, which is why `block` takes it off.
    #[test]
    fn changed_rows_reach_the_edge_on_a_screen_and_not_in_a_file() {
        let lines = render("a\n", "b\n", 40, &Unhighlighted);
        let changed = &lines[0];
        assert_eq!(changed.width(), 40);
        assert_eq!(
            crate::render::block(&lines, Depth::None),
            " 1 -a\n 1 +b\n",
            "a pipe gets no trailing blanks"
        );
    }

    #[test]
    fn distant_hunks_are_separated_by_a_mark_and_not_by_a_blank_line() {
        let old: String = (1..=40).map(|n| format!("line {n}\n")).collect();
        let new = old
            .replace("line 2\n", "LINE 2\n")
            .replace("line 39\n", "LINE 39\n");
        let out = rendered(&old, &new);
        assert!(out.iter().any(|line| line.contains('⋯')), "{out:#?}");
        assert!(
            !out.iter().any(|line| line.trim().is_empty()),
            "a blank row would read as an unchanged empty line"
        );
    }

    #[test]
    fn a_file_with_no_change_renders_nothing() {
        assert!(rendered("same\n", "same\n").is_empty());
    }

    /// The gutter is sized once for the whole diff. Sizing it per hunk shifts
    /// the body two columns at the hunk that crosses 1000.
    #[test]
    fn the_gutter_does_not_move_when_the_line_count_crosses_a_power_of_ten() {
        let old: String = (1..=1200).map(|n| format!("line {n}\n")).collect();
        let new = old
            .replace("line 5\n", "LINE 5\n")
            .replace("line 1100\n", "LINE 1100\n");
        let out = render(&old, &new, 60, &Unhighlighted);
        let gutters: Vec<usize> = out
            .iter()
            .filter(|line| !line.text().contains('⋯'))
            .filter_map(|line| line.spans.first().map(|span| cells(&span.text)))
            .collect();
        assert!(
            gutters.windows(2).all(|pair| pair[0] == pair[1]),
            "{gutters:?}"
        );
    }
}
