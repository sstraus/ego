//! A line of styled text, measured in cells rather than in bytes.
//!
//! Everything downstream — markdown, tables, diffs — produces [`Line`]s and
//! never strings with escapes already in them. That is the whole reason the
//! type exists: a renderer that concatenates pre-escaped strings can no longer
//! measure its own output, and every width it computes afterwards is a byte
//! count wearing a column's name. `len()` says 15 for `ESC[1mciao ESC[0m` and
//! 4 for `naïve` when the accent is decomposed; a table laid out on either
//! number is crooked in a way no test that reads the string back can see.
//!
//! Wrapping is here too, for the same reason and one more: a wrapped row has
//! to re-open the style it was in the middle of, because [`super::style::Style`]
//! closes with `ESC[0m` and a host that reflows or copies rows separately
//! must never receive half a styled run.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::caps::{Capabilities, Depth};
use super::osc::Osc;
use super::style::Style;

/// A run of text with one appearance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub style: Style,
    /// The target of an OSC 8 hyperlink around this run, if it is one.
    ///
    /// Beside the text rather than inside it, and this is the whole reason
    /// [`Span`] is not a `String`: OSC 8 occupies **no cells**. A renderer
    /// that wrapped the text in the escape before layout would measure the URL
    /// as content, and every column after it would be wrong by the length of a
    /// link the reader cannot see.
    pub link: Option<String>,
}

impl Span {
    pub fn new(text: impl Into<String>, style: Style) -> Self {
        Span {
            text: text.into(),
            style,
            link: None,
        }
    }

    pub fn plain(text: impl Into<String>) -> Self {
        Span::new(text, Style::PLAIN)
    }

    pub fn linked(text: impl Into<String>, style: Style, uri: impl Into<String>) -> Self {
        Span {
            text: text.into(),
            style,
            link: Some(uri.into()),
        }
    }
}

/// One row. Never contains a newline: a `Line` is what occupies one terminal
/// row after wrapping, and the writer supplies the `\n`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Line {
    pub spans: Vec<Span>,
}

impl Line {
    pub fn empty() -> Self {
        Line { spans: Vec::new() }
    }

    pub fn of(spans: Vec<Span>) -> Self {
        Line { spans }
    }

    pub fn plain(text: impl Into<String>) -> Self {
        Line {
            spans: vec![Span::plain(text)],
        }
    }

    pub fn styled(text: impl Into<String>, style: Style) -> Self {
        Line {
            spans: vec![Span::new(text, style)],
        }
    }

    pub fn push(&mut self, span: Span) {
        if span.text.is_empty() {
            return;
        }
        // Coalesce on the way in, so `width` and `wrap` never walk a thousand
        // one-character spans produced by an inline parser.
        if let Some(last) = self.spans.last_mut()
            && last.style == span.style
            && last.link == span.link
        {
            last.text.push_str(&span.text);
            return;
        }
        self.spans.push(span);
    }

    pub fn is_empty(&self) -> bool {
        self.spans.iter().all(|span| span.text.is_empty())
    }

    /// Display width in terminal cells.
    pub fn width(&self) -> usize {
        self.spans.iter().map(|span| cells(&span.text)).sum()
    }

    /// The text with every style dropped. What a pipe gets, and what a test
    /// asserts against when it is checking layout rather than colour.
    pub fn text(&self) -> String {
        self.spans.iter().map(|span| span.text.as_str()).collect()
    }

    /// Extend to `width` cells with unstyled spaces.
    ///
    /// Unstyled on purpose: padding a cell with the cell's own background is
    /// what makes a diff line reach the right edge, and it is also what leaves
    /// a file full of trailing whitespace when the same output is redirected.
    /// The diff asks for its padding explicitly, through [`Line::pad_styled`];
    /// nothing else gets it by default.
    pub fn pad_to(&mut self, width: usize) {
        self.pad_styled(width, Style::PLAIN);
    }

    pub fn pad_styled(&mut self, width: usize, style: Style) {
        let have = self.width();
        if have < width {
            self.push(Span::new(" ".repeat(width - have), style));
        }
    }

    /// Cut to `width` cells, never inside a grapheme cluster.
    ///
    /// Used only where the content is known to be unbreakable — a URL in a
    /// narrow cell. Prose reaches [`Line::wrap`] instead, because truncating
    /// prose deletes what the model said.
    pub fn truncate_to(&self, width: usize) -> Line {
        let mut out = Line::empty();
        let mut used = 0;
        for span in &self.spans {
            for grapheme in span.text.graphemes(true) {
                let w = cells(grapheme);
                if used + w > width {
                    return out;
                }
                used += w;
                out.push(Span {
                    text: grapheme.to_string(),
                    style: span.style,
                    link: span.link.clone(),
                });
            }
        }
        out
    }

    /// Greedy word wrap at `width` cells.
    ///
    /// Breaks at whitespace; a single word longer than the line is broken at
    /// the last cell that fits, because the alternative is a row wider than
    /// the terminal, and a row wider than the terminal is reflowed by the host
    /// — which TUIC has explicitly disabled (`resize_reflow`), so it would
    /// simply be lost.
    ///
    /// Trailing whitespace at a break is dropped; whitespace inside a line is
    /// kept. Styles survive the break because each grapheme carries its own.
    pub fn wrap(&self, width: usize) -> Vec<Line> {
        self.wrap_with(width, true, 0)
    }

    /// Word wrap with a hanging indent included in each continuation row.
    pub fn wrap_hanging(&self, width: usize, indent: usize) -> Vec<Line> {
        self.wrap_with(width, true, indent)
    }

    /// Word wrap that never breaks a word, even one wider than the line.
    ///
    /// For values whose characters belong together more than they belong to
    /// the layout: a URL, a path, an identifier. Chopping one at column 18
    /// costs the reader a copy-paste and costs OSC 8 its target, and the
    /// emulator's own soft wrap — which is not reflow, and survives TUIC
    /// disabling reflow — handles the overflow without either loss.
    ///
    /// Not the default, because a table cell that overflows breaks the grid
    /// for every row below it.
    pub fn wrap_soft(&self, width: usize) -> Vec<Line> {
        self.wrap_with(width, false, 0)
    }

    /// The whole of wrapping, in one place.
    ///
    /// It fits **words**, not graphemes. A grapheme-at-a-time loop that breaks
    /// whenever the next cell does not fit is a *character* wrap wearing a
    /// word wrap's name: it happens to agree with one whenever the words are
    /// short enough that the overflow lands on a space, which is most prose,
    /// and then splits `https://example.com/…` down the middle the first time
    /// it does not. The decision has to be taken once per word, before any of
    /// the word is placed.
    fn wrap_with(&self, width: usize, break_words: bool, indent: usize) -> Vec<Line> {
        let indent = indent.min(width.saturating_sub(1));
        let continuation_width = width.saturating_sub(indent).max(1);
        let mut width = width.max(1);
        if self.width() <= width {
            return vec![self.clone()];
        }
        let atoms = self.atoms();
        let mut lines = Vec::new();
        let mut current: Vec<Atom<'_>> = Vec::new();
        let mut used = 0;
        let mut index = 0;

        while index < atoms.len() {
            // The run of spaces before the next word, held back so a break can
            // discard it and a continuation can keep it.
            let gap_start = index;
            while index < atoms.len() && is_space(atoms[index].grapheme) {
                index += 1;
            }
            let gap = &atoms[gap_start..index];
            let gap_width: usize = gap.iter().map(|atom| cells(atom.grapheme)).sum();

            let word_start = index;
            while index < atoms.len() && !is_space(atoms[index].grapheme) {
                index += 1;
            }
            let word = &atoms[word_start..index];
            if word.is_empty() {
                // Trailing whitespace at the very end of the line: it belongs
                // to no word and starts no row.
                if !gap.is_empty() && used > 0 && used + gap_width <= width {
                    current.extend_from_slice(gap);
                }
                break;
            }
            let word_width: usize = word.iter().map(|atom| cells(atom.grapheme)).sum();

            if used > 0 && used + gap_width + word_width > width {
                lines.push(collect(&current));
                width = continuation_width;
                current.clear();
                used = 0;
            } else if used > 0 {
                current.extend_from_slice(gap);
                used += gap_width;
            }

            if word_width <= width || !break_words {
                current.extend_from_slice(word);
                used += word_width;
                continue;
            }
            // A word wider than the whole line, and the caller wants columns
            // kept. Fill each row to the cell.
            for atom in word {
                let w = cells(atom.grapheme);
                if used + w > width && used > 0 {
                    lines.push(collect(&current));
                    width = continuation_width;
                    current.clear();
                    used = 0;
                }
                current.push(*atom);
                used += w;
            }
        }
        if !current.is_empty() {
            lines.push(collect(&current));
        }
        if lines.is_empty() {
            lines.push(Line::empty());
        }
        if indent > 0 {
            for line in lines.iter_mut().skip(1) {
                let mut indented = Line::plain(" ".repeat(indent));
                for span in std::mem::take(&mut line.spans) {
                    indented.push(span);
                }
                *line = indented;
            }
        }
        lines
    }

    /// The bytes. `depth` decides whether any of them are escapes.
    ///
    /// Each span opens and closes on its own rather than the line opening once
    /// and closing at the end: a host that damages and redraws a single cell
    /// range needs the style to be recoverable from the cell, and a run that
    /// inherits an open sequence from three spans ago is not.
    pub fn render(&self, depth: Depth) -> String {
        self.render_with(depth, &Osc::new(Capabilities::plain()))
    }

    /// The bytes, with hyperlinks if this host has them.
    ///
    /// `osc` and not a `bool` so the sequence is built in one place — the same
    /// rule as [`super::style`]: nothing outside those two modules formats an
    /// escape.
    pub fn render_with(&self, depth: Depth, osc: &Osc) -> String {
        let mut out = String::new();
        for span in &self.spans {
            let painted = span.style.paint(depth, &span.text);
            match &span.link {
                Some(uri) => out.push_str(&osc.hyperlink(uri, &painted)),
                None => out.push_str(&painted),
            }
        }
        out
    }

    fn atoms(&self) -> Vec<Atom<'_>> {
        self.spans
            .iter()
            .flat_map(|span| {
                span.text.graphemes(true).map(move |grapheme| Atom {
                    grapheme,
                    style: span.style,
                    link: span.link.as_deref(),
                })
            })
            .collect()
    }
}

/// One grapheme with everything that must survive a line break.
#[derive(Debug, Clone, Copy)]
struct Atom<'a> {
    grapheme: &'a str,
    style: Style,
    link: Option<&'a str>,
}

fn collect(atoms: &[Atom<'_>]) -> Line {
    let mut line = Line::empty();
    for atom in atoms {
        line.push(Span {
            text: atom.grapheme.to_string(),
            style: atom.style,
            link: atom.link.map(str::to_string),
        });
    }
    line
}

fn is_space(grapheme: &str) -> bool {
    grapheme == " " || grapheme == "\t"
}

/// Display width of a string in terminal cells.
///
/// Per grapheme cluster and not per `char`: `unicode-width` answers for a
/// scalar, so a base plus two combining marks measures 1 + 0 + 0 correctly by
/// luck, while a regional-indicator pair measures 2 + 2 and is wrong by a
/// factor of two. Clustering first makes the common emoji cases right and the
/// exotic ones no worse.
pub fn cells(text: &str) -> usize {
    text.graphemes(true)
        .map(|grapheme| {
            let width = UnicodeWidthStr::width(grapheme);
            // A cluster that measures zero still occupies a cell if it is
            // printable — a lone combining mark, a ZWJ left dangling by a
            // truncation. Counting it as zero lets a table row overflow by
            // however many of them the model emitted.
            if width == 0 && !grapheme.chars().all(|c| c.is_control()) {
                1
            } else {
                width
            }
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::caps::Terminal;
    use crate::render::style::Rgb;

    #[test]
    fn width_is_cells_and_not_bytes() {
        assert_eq!(cells("ciao"), 4);
        // Four bytes, one cell.
        assert_eq!(cells("è"), 1);
        // Decomposed: two scalars, one cluster, one cell.
        assert_eq!(cells("e\u{301}"), 1);
        // Wide.
        assert_eq!(cells("日本語"), 6);
    }

    /// The measurement a renderer that concatenated escapes would get wrong:
    /// the styled line is 15 bytes and 4 cells.
    #[test]
    fn style_costs_no_cells() {
        let line = Line::styled("ciao", Style::PLAIN.bold());
        assert_eq!(line.width(), 4);
        // Twelve bytes: the text, plus an opener and a closer.
        assert_eq!(line.render(Depth::TrueColor), "\x1b[1mciao\x1b[0m");
        assert_eq!(line.render(Depth::TrueColor).len(), 12);
    }

    /// The other reason `Span` is not a `String`: an OSC 8 hyperlink is
    /// arbitrarily long and occupies no cells. Measuring it as content would
    /// put every column after it out by the length of a URL nobody can see.
    #[test]
    fn a_hyperlink_costs_no_cells_either() {
        let line = Line::of(vec![Span::linked(
            "run.rs",
            Style::PLAIN,
            "file:///a/very/long/path/to/run.rs#42",
        )]);
        assert_eq!(line.width(), 6);
        let osc = Osc::new(Capabilities::of(&Terminal {
            stdout_is_terminal: true,
            term: Some("xterm-256color"),
            ..Terminal::default()
        }));
        let bytes = line.render_with(Depth::None, &osc);
        assert!(bytes.contains("file:///a/very/long/path/to/run.rs#42"));
        assert!(bytes.len() > 40);
    }

    #[test]
    fn spans_of_one_style_coalesce_on_the_way_in() {
        let mut line = Line::empty();
        for character in "hello".chars() {
            line.push(Span::plain(character.to_string()));
        }
        assert_eq!(line.spans.len(), 1);
        assert_eq!(line.text(), "hello");
    }

    #[test]
    fn an_empty_span_is_not_stored() {
        let mut line = Line::empty();
        line.push(Span::plain(""));
        assert!(line.spans.is_empty());
    }

    #[test]
    fn wrapping_breaks_at_words_and_drops_the_break_space() {
        let line = Line::plain("the quick brown fox jumps");
        let wrapped: Vec<String> = line.wrap(10).iter().map(Line::text).collect();
        assert_eq!(wrapped, vec!["the quick", "brown fox", "jumps"]);
    }

    /// The property that makes wrapping safe to do after styling: a break in
    /// the middle of a styled run leaves both halves styled.
    #[test]
    fn a_style_survives_a_break_in_the_middle_of_it() {
        let emphasis = Style::fg(Rgb(1, 2, 3));
        let line = Line::of(vec![
            Span::plain("say "),
            Span::new("something rather long", emphasis),
        ]);
        let wrapped = line.wrap(12);
        assert!(wrapped.len() > 1, "the point is that it broke");
        for row in &wrapped {
            if !row.text().trim().is_empty() && row.text() != "say" {
                let rendered = row.render(Depth::TrueColor);
                assert!(
                    rendered.contains("\x1b[38;2;1;2;3m"),
                    "each row re-opens the style it is inside: {rendered:?}"
                );
                assert!(rendered.ends_with("\x1b[0m"), "and closes it: {rendered:?}");
            }
        }
    }

    // Catches: continuation indentation is added after wrapping and overflows,
    // or a style/link boundary incorrectly makes a word breakable.
    #[test]
    fn hanging_wrap_reserves_cells_and_preserves_styled_words() {
        let line = Line::of(vec![
            Span::plain("one two "),
            Span::linked("three", Style::PLAIN.bold(), "https://example.test"),
            Span::plain("four five six"),
        ]);
        let rows = line.wrap_hanging(12, 3);
        assert_eq!(
            rows.iter().map(Line::text).collect::<Vec<_>>(),
            ["one two", "   threefour", "   five six"]
        );
        assert!(rows.iter().all(|row| row.width() <= 12));
        assert_eq!(
            rows[1].spans[1].link.as_deref(),
            Some("https://example.test")
        );
    }

    // Catches: an oversized word loops or overflows the reduced continuation width.
    #[test]
    fn hanging_wrap_breaks_only_words_larger_than_the_available_row() {
        let rows = Line::plain("💬 abcdefghijklmnop").wrap_hanging(10, 3);
        assert_eq!(
            rows.iter().map(Line::text).collect::<Vec<_>>(),
            ["💬", "   abcdefg", "   hijklmn", "   op"]
        );
        assert!(rows.iter().all(|row| row.width() <= 10));
    }

    /// A path or a URL belongs together more than it belongs to the column.
    #[test]
    fn soft_wrapping_lets_an_unbreakable_word_overflow() {
        let line = Line::plain("see https://example.com/a/very/long/path now");
        let wrapped: Vec<String> = line.wrap_soft(12).iter().map(Line::text).collect();
        assert!(
            wrapped
                .iter()
                .any(|row| row == "https://example.com/a/very/long/path"),
            "{wrapped:?}"
        );
    }

    /// A word wider than the line has no good answer; the bad answers are a
    /// row wider than the terminal (which TUIC will not reflow, so it is lost)
    /// and an infinite loop.
    #[test]
    fn a_word_longer_than_the_line_is_broken_rather_than_overflowing() {
        let line = Line::plain("supercalifragilistic");
        let wrapped: Vec<String> = line.wrap(7).iter().map(Line::text).collect();
        assert_eq!(wrapped, vec!["superca", "lifragi", "listic"]);
        assert!(wrapped.iter().all(|row| cells(row) <= 7));
    }

    #[test]
    fn wrapping_at_a_width_that_already_fits_is_the_identity() {
        let line = Line::plain("short");
        assert_eq!(line.wrap(80), vec![line.clone()]);
    }

    #[test]
    fn a_zero_width_request_does_not_hang() {
        let wrapped = Line::plain("abc").wrap(0);
        assert_eq!(wrapped.len(), 3);
    }

    #[test]
    fn truncation_never_splits_a_cluster() {
        let line = Line::plain("日本語");
        // Three cells fits one wide cluster and half of another; the half is
        // not emitted.
        assert_eq!(line.truncate_to(3).text(), "日");
        assert_eq!(line.truncate_to(4).text(), "日本");
    }

    #[test]
    fn padding_reaches_the_asked_width_in_cells() {
        let mut line = Line::plain("日本");
        line.pad_to(6);
        assert_eq!(line.width(), 6);
        assert_eq!(line.text(), "日本  ");
    }
}
