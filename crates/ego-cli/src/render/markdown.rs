//! Markdown, rendered by the harness — because nobody else can.
//!
//! The captures behind
//! `ARCHITECTURE.md` settle
//! what would otherwise be a matter of taste. A model emits
//! `**Step 13.5 spedito** (` a9b84ad `)`; what reaches the terminal is
//! `ESC[1mStep 13.5 spedito ESC[22m ( ESC[38;2;174;185;213ma9b84ad ESC[39m )`.
//! Two rules, and they are rules rather than preferences:
//!
//! - `**strong**` is **weight and no colour**.
//! - `` `code` `` is **colour and no weight**.
//!
//! A sentence in technical prose contains several of each. Giving both a
//! colour, or both a weight, makes the paragraph a ransom note and destroys
//! the distinction the author was drawing.
//!
//! Everything here returns [`Line`]s. The model's text never becomes a string
//! with escapes in it before layout, because layout has to be able to measure
//! it — see [`super::line`].

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use super::caps::Depth;
use super::line::{Line, Span};
use super::style::{Palette, Style};
use super::table::{Align, Table};

/// Turn a markdown document into rendered rows.
///
/// `width` is the wrap column: the terminal's on a screen, a fixed 80 on a
/// pipe, so a redirected run produces the same bytes from any window.
pub fn render(source: &str, width: usize) -> Vec<Line> {
    let mut renderer = Renderer::new(width);
    let mut paragraph = false;
    for (event, range) in Parser::new_ext(source, options()).into_offset_iter() {
        let top_level = renderer.lists.is_empty()
            && renderer.quote == 0
            && renderer.code.is_none()
            && renderer.table.is_none();
        match event {
            Event::Start(Tag::Paragraph) if top_level => {
                paragraph = true;
                renderer.event(event);
                renderer.host_line = host_marker_line(&source[range.start..]);
            }
            Event::SoftBreak | Event::HardBreak if paragraph => {
                let next_host = host_marker_line(&source[range.end..]);
                if renderer.host_line || next_host || source[range.end..].starts_with("💬 ") {
                    renderer.flush();
                } else {
                    renderer.event(event);
                }
                renderer.host_line = next_host;
            }
            Event::End(TagEnd::Paragraph) => {
                renderer.event(event);
                paragraph = false;
            }
            _ => renderer.event(event),
        }
    }
    renderer.finish()
}

/// Host protocol markers are column-zero source lines, not Markdown containers.
fn host_marker_line(source: &str) -> bool {
    let line = source.lines().next().unwrap_or_default();
    line.starts_with("intent: ")
        || line.starts_with("suggest: ")
        || (line.starts_with("TUICommander v") && line.ends_with(" is connected."))
}

/// The dialect ego reads, in one place.
///
/// [`super::stream`] parses the same text to find its block boundaries, and two
/// parsers configured differently would disagree about where a block ends — the
/// streaming renderer would checkpoint inside something the batch renderer
/// treats as one construct. There is one answer because there is one function.
pub(super) fn options() -> Options {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    // Not `ENABLE_SMART_PUNCTUATION`: a model that wrote `"` meant `"`, and a
    // renderer that turns it into `"` has edited the answer.
    options
}

/// The list the renderer is inside, if any.
#[derive(Debug, Clone, Copy)]
struct ListLevel {
    /// `Some(n)` for an ordered list, counting up. `None` for a bullet.
    next: Option<u64>,
}

struct Renderer {
    width: usize,
    out: Vec<Line>,
    current: Line,
    /// The style stack. Pushed by `Strong`/`Emphasis`/`Code`/… and popped by
    /// their ends, so nesting composes rather than overwrites: `**bold with
    /// `code` inside**` is bold *and* coloured, which is what the author
    /// wrote.
    styles: Vec<Style>,
    lists: Vec<ListLevel>,
    /// Cells of prose indent that every wrapped row of the current block
    /// carries — list markers and block quotes.
    indent: usize,
    quote: usize,
    /// Set while inside a fenced block, so text events are taken verbatim.
    code: Option<Vec<String>>,
    table: Option<TableBuilder>,
    /// The link the current spans belong to, if any.
    link: Option<String>,
    /// The list marker owed to the first row of the item being built. Held
    /// out of `current` so it is not measured twice — see [`Renderer::flush`].
    marker: Option<Line>,
    /// Host markers must reach the terminal as one logical line so it owns wrapping.
    host_line: bool,
}

#[derive(Default)]
struct TableBuilder {
    align: Vec<Align>,
    header: Vec<Line>,
    rows: Vec<Vec<Line>>,
    row: Vec<Line>,
    in_header: bool,
}

impl Renderer {
    fn new(width: usize) -> Self {
        Renderer {
            width: width.max(20),
            out: Vec::new(),
            current: Line::empty(),
            styles: Vec::new(),
            lists: Vec::new(),
            indent: 0,
            quote: 0,
            code: None,
            table: None,
            link: None,
            marker: None,
            host_line: false,
        }
    }

    fn style(&self) -> Style {
        self.styles.last().copied().unwrap_or(Style::PLAIN)
    }

    fn push_style(&mut self, change: impl FnOnce(Style) -> Style) {
        let style = change(self.style());
        self.styles.push(style);
    }

    fn text(&mut self, text: &str) {
        let style = self.style();
        let link = self.link.clone();
        self.current.push(match link {
            Some(uri) => Span::linked(text, style, uri),
            None => Span::new(text, style),
        });
    }

    /// End the paragraph-shaped thing being built and wrap it into `out`.
    ///
    /// The marker of a list item is deliberately **not** part of `current`.
    /// Putting it there is the obvious implementation and it double-counts:
    /// the marker occupies cells in the wrapped line *and* is subtracted from
    /// the wrap column, so a `• ` item wraps two cells early and every nested
    /// level compounds the error.
    fn flush(&mut self) {
        if self.current.is_empty() {
            self.current = Line::empty();
            // The marker is **kept**. It is owed to the next row this item
            // produces, not to this call: in a *loose* list every item opens
            // with a paragraph, `Tag::Paragraph` flushes before it, and
            // clearing here threw away the bullet of every item in every loose
            // list — the list rendered as indented prose with no markers at
            // all. Nothing else can put it back, because by the time the text
            // arrives the item has already started.
            return;
        }
        let prefix = self.quote_prefix();
        let body = self
            .width
            .saturating_sub(self.indent + prefix.width())
            .max(8);
        let taken = std::mem::take(&mut self.current);
        if std::mem::take(&mut self.host_line) {
            self.out.push(taken);
            return;
        }
        let marker = self.marker.take();
        // TUIC keeps hard-wrapped answer rows in the answer region when they
        // carry a hanging indent. The overlay itself consumes no cells.
        let rows = if taken.text().starts_with("💬 ") {
            taken.wrap_hanging(body, super::line::cells("💬 "))
        } else {
            taken.wrap(body)
        };
        for (position, mut row) in rows.into_iter().enumerate() {
            let mut line = prefix.clone();
            match (&marker, position) {
                // The marker goes on the first row; continuation rows carry
                // the same number of blank cells, so the text stays in one
                // column and does not read as a new item.
                (Some(marker), 0) => {
                    for span in &marker.spans {
                        line.push(span.clone());
                    }
                }
                _ if self.indent > 0 => line.push(Span::plain(" ".repeat(self.indent))),
                _ => {}
            }
            for span in std::mem::take(&mut row.spans) {
                line.push(span);
            }
            self.out.push(line);
        }
    }

    fn quote_prefix(&self) -> Line {
        let mut prefix = Line::empty();
        for _ in 0..self.quote {
            // A bar, not a `>`. The bar is what a reader's eye follows down
            // the left margin; a repeated `>` reads as part of the sentence.
            prefix.push(Span::new("▌ ", Style::fg(Palette::QUOTE)));
        }
        prefix
    }

    fn blank(&mut self) {
        // Never two in a row, and never one at the top: markdown's block
        // separation is generous and a terminal's vertical space is not.
        if self.out.is_empty() {
            return;
        }
        // Inside a quote the separator carries the bar. A bare blank line
        // between two paragraphs of one quote breaks the margin the bar draws,
        // and a broken margin reads as two quotes.
        let separator = self.quote_prefix();
        if self
            .out
            .last()
            .is_some_and(|last| last.text().trim_end().is_empty())
        {
            return;
        }
        self.out.push(separator);
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => {
                if let Some(block) = self.code.as_mut() {
                    for (index, piece) in text.split('\n').enumerate() {
                        if index > 0 {
                            block.push(String::new());
                        }
                        match block.last_mut() {
                            Some(last) => last.push_str(piece),
                            None => block.push(piece.to_string()),
                        }
                    }
                } else {
                    self.text(&text);
                }
            }
            Event::Code(code) => {
                let style = self.style().with_fg(Palette::CODE);
                self.current.push(Span::new(code.to_string(), style));
            }
            // A soft break is a newline the author typed inside a paragraph.
            // It becomes a space: the renderer owns the wrap column, and
            // honouring the author's would produce a ragged column of the
            // width *their* editor happened to be.
            Event::SoftBreak => self.text(" "),
            Event::HardBreak => {
                self.flush();
            }
            Event::Rule => {
                self.flush();
                self.blank();
                // Capped rather than drawn to the edge. A thematic break is a
                // pause, and a pause 200 cells wide reads as a wall — the one
                // element in this renderer that gains nothing from the space it
                // is given.
                const WIDEST_RULE: usize = 60;
                self.out.push(Line::styled(
                    "─".repeat(self.width.min(WIDEST_RULE)),
                    Style::fg(Palette::RULE),
                ));
                self.blank();
            }
            Event::TaskListMarker(done) => {
                let mark = if done { "[x] " } else { "[ ] " };
                let style = Style::fg(Palette::BULLET);
                self.current.push(Span::new(mark, style));
            }
            // HTML in a model's answer is text it wrote, and dropping it hides
            // content. It is not interpreted.
            Event::Html(html) | Event::InlineHtml(html) => self.text(&html),
            Event::FootnoteReference(name) => self.text(&format!("[^{name}]")),
            Event::InlineMath(text) | Event::DisplayMath(text) => self.text(&text),
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                self.flush();
                // Not before the first paragraph of a list item. A pending
                // marker means this paragraph *is* the item, and separating a
                // bullet from its own text with a blank line reads as an empty
                // item followed by a stray paragraph. A second paragraph inside
                // the same item still gets one, because by then the marker has
                // been spent.
                if self.marker.is_none() {
                    self.blank();
                }
            }
            Tag::Heading { level, .. } => {
                self.flush();
                self.blank();
                self.push_style(|style| style.with_fg(Palette::HEADING).bold());
                // `#` is markdown's syntax for depth, not the author's word.
                // The level survives as weight and as a prefix that shows
                // nesting without spending a colour on it.
                if level > HeadingLevel::H2 {
                    let depth = heading_depth(level);
                    self.current
                        .push(Span::new("─".repeat(depth), Style::fg(Palette::RULE)));
                    self.current.push(Span::plain(" "));
                }
            }
            Tag::BlockQuote(_) => {
                self.flush();
                self.blank();
                self.quote += 1;
            }
            Tag::CodeBlock(kind) => {
                self.flush();
                self.blank();
                if let CodeBlockKind::Fenced(language) = &kind
                    && !language.is_empty()
                {
                    self.out.push(Line::styled(
                        language.to_string(),
                        Style::fg(Palette::QUOTE).dim(),
                    ));
                }
                self.code = Some(vec![String::new()]);
            }
            Tag::List(start) => {
                self.flush();
                if self.lists.is_empty() {
                    self.blank();
                }
                self.lists.push(ListLevel { next: start });
            }
            Tag::Item => {
                self.flush();
                let level = self.lists.len().saturating_sub(1);
                let marker = match self.lists.last_mut() {
                    Some(ListLevel { next: Some(n) }) => {
                        let marker = format!("{n}. ");
                        *n += 1;
                        marker
                    }
                    // Depth is shown by the glyph rather than by colour: three
                    // shades of bullet is a legend nobody has.
                    _ => match level % 3 {
                        0 => "• ".to_string(),
                        1 => "◦ ".to_string(),
                        _ => "▪ ".to_string(),
                    },
                };
                let pad = level * 2;
                self.indent = pad + super::line::cells(&marker);
                let mut line = Line::empty();
                line.push(Span::plain(" ".repeat(pad)));
                line.push(Span::new(marker, Style::fg(Palette::BULLET)));
                self.marker = Some(line);
            }
            Tag::Emphasis => self.push_style(Style::italic),
            Tag::Strong => self.push_style(Style::bold),
            Tag::Strikethrough => self.push_style(Style::strike),
            Tag::Link { dest_url, .. } => {
                self.link = Some(dest_url.to_string());
                self.push_style(|style| style.with_fg(Palette::LINK).underline());
            }
            Tag::Image { dest_url, .. } => {
                // No image path exists: TUIC decodes `alacritty_terminal`,
                // which implements neither the kitty graphics protocol nor
                // sixel (ADR §4). The alternative to naming the image is
                // silently dropping it.
                self.text("🖼 ");
                self.link = Some(dest_url.to_string());
                self.push_style(|style| style.with_fg(Palette::LINK).underline());
            }
            Tag::Table(alignments) => {
                self.flush();
                self.blank();
                self.table = Some(TableBuilder {
                    align: alignments.iter().map(convert_alignment).collect(),
                    ..TableBuilder::default()
                });
            }
            Tag::TableHead => {
                if let Some(table) = self.table.as_mut() {
                    table.in_header = true;
                }
            }
            Tag::TableRow | Tag::TableCell => {}
            Tag::FootnoteDefinition(_) | Tag::HtmlBlock | Tag::MetadataBlock(_) => {
                self.flush();
            }
            Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::Superscript
            | Tag::Subscript => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush(),
            TagEnd::Heading(_) => {
                self.flush();
                self.styles.pop();
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.quote = self.quote.saturating_sub(1);
            }
            TagEnd::CodeBlock => {
                let Some(mut block) = self.code.take() else {
                    return;
                };
                // A fence's last line is the newline before the closing
                // backticks, not an empty line the author wrote.
                if block.last().is_some_and(String::is_empty) {
                    block.pop();
                }
                for row in block {
                    let mut line = Line::plain("  ");
                    line.push(Span::new(row, Style::fg(Palette::CODE_FG)));
                    self.out.push(line);
                }
                self.blank();
            }
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
                self.indent = 0;
                if self.lists.is_empty() {
                    self.blank();
                }
            }
            TagEnd::Item => {
                self.flush();
                self.indent = 0;
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.styles.pop();
            }
            TagEnd::Link | TagEnd::Image => {
                self.styles.pop();
                self.link = None;
            }
            TagEnd::Table => {
                let Some(builder) = self.table.take() else {
                    return;
                };
                let table = Table {
                    header: builder.header,
                    rows: builder.rows,
                    align: builder.align,
                };
                self.out.extend(table.render(self.width, Depth::None));
                self.blank();
            }
            TagEnd::TableHead => {
                if let Some(table) = self.table.as_mut() {
                    table.in_header = false;
                }
            }
            TagEnd::TableRow => {
                if let Some(table) = self.table.as_mut() {
                    let row = std::mem::take(&mut table.row);
                    table.rows.push(row);
                }
            }
            TagEnd::TableCell => {
                let cell = std::mem::take(&mut self.current);
                if let Some(table) = self.table.as_mut() {
                    if table.in_header {
                        table.header.push(cell);
                    } else {
                        table.row.push(cell);
                    }
                }
            }
            _ => {}
        }
    }

    fn finish(mut self) -> Vec<Line> {
        self.flush();
        while self.out.last().is_some_and(Line::is_empty) {
            self.out.pop();
        }
        self.out
    }
}

fn heading_depth(level: HeadingLevel) -> usize {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn convert_alignment(alignment: &Alignment) -> Align {
    match alignment {
        Alignment::Right => Align::Right,
        Alignment::Center => Align::Center,
        Alignment::Left | Alignment::None => Align::Left,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(source: &str, width: usize) -> Vec<String> {
        render(source, width).iter().map(Line::text).collect()
    }

    fn bytes(source: &str, width: usize) -> String {
        crate::render::block(&render(source, width), Depth::TrueColor)
    }

    // Catches: CommonMark merges adjacent host protocol lines into one paragraph row.
    #[test]
    fn rendered_host_markers_keep_line_boundaries() {
        for width in [80, 120, 200] {
            assert_eq!(
                text(
                    "TUICommander v1.8.0 is connected.\nintent: Checking output (Output check)\nordinary prose\ncontinues here\nsuggest: [ A | B | C ]",
                    width
                ),
                [
                    "TUICommander v1.8.0 is connected.",
                    "intent: Checking output (Output check)",
                    "ordinary prose continues here",
                    "suggest: [ A | B | C ]"
                ]
            );
        }
    }

    // Catches: explicit renderer newlines prevent the host joining a marker's soft-wrapped tail.
    #[test]
    fn rendered_host_marker_lines_soft_wrap() {
        for width in [80, 120, 200] {
            for prefix in ["intent: ", "suggest: [ "] {
                let source = format!("{prefix}{}", "long marker text ".repeat(30));
                assert_eq!(text(&source, width), [source.trim_end()]);
            }
        }
    }

    // Catches: marker recognition changes Markdown inside containers or treats marker-like prose as protocol.
    #[test]
    fn host_markers_do_not_change_commonmark_containers_or_breaks() {
        assert_eq!(
            text("one\ntwo  \nthree\n\nfour", 80),
            ["one two", "three", "", "four"]
        );
        assert_eq!(
            text("- intent: one\n  continuation\n- two", 80),
            ["• intent: one continuation", "• two"]
        );
        assert_eq!(
            text("> intent: one\n> continuation", 80),
            ["▌ intent: one continuation"]
        );
        assert_eq!(
            text("```\nintent: one\ncontinuation\n```", 80),
            ["  intent: one", "  continuation"]
        );
        assert_eq!(
            text("not intent: one\ncontinuation", 80),
            ["not intent: one continuation"]
        );
    }

    // Catches: adjacent answer markers merge or their styled tails overflow.
    #[test]
    fn marked_answers_keep_boundaries_and_word_wrapped_styled_tails() {
        let source = format!(
            "💬 **{}**\n💬 second answer",
            "accented perché l’eseguibile ".repeat(10).trim_end()
        );
        let rows = text(&source, 80);
        let second = rows
            .iter()
            .position(|row| row == "💬 second answer")
            .unwrap();
        assert!(second > 1);
        assert!(rows[1..second].iter().all(|row| row.starts_with("   ")));
        assert!(rows.iter().all(|row| super::super::line::cells(row) <= 80));
        assert!(bytes(&source, 80).contains("\x1b[1m"));
    }

    /// The two rules the capture settles. Not preferences: a paragraph where
    /// both are coloured, or both bold, loses the distinction the author drew.
    #[test]
    fn strong_is_weight_and_code_is_colour() {
        let out = bytes("**shipped** in `run.rs`", 80);
        assert!(out.contains("\x1b[1mshipped\x1b[0m"), "{out:?}");
        assert!(
            out.contains("\x1b[38;2;174;185;213mrun.rs\x1b[0m"),
            "{out:?}"
        );
        // And neither borrows the other's axis.
        assert!(!out.contains("\x1b[1;38;2;174;185;213mrun.rs"));
    }

    /// Nesting composes rather than overwrites. `**bold with `code`**` is what
    /// the author wrote, and it is both.
    #[test]
    fn nested_emphasis_composes() {
        let out = bytes("**bold with `code` inside**", 80);
        assert!(out.contains("\x1b[1;38;2;174;185;213mcode"), "{out:?}");
    }

    #[test]
    fn markdown_syntax_does_not_survive_into_the_output() {
        let out = text("## A heading\n\n- one\n- two\n", 80).join("\n");
        assert!(!out.contains('#'), "{out}");
        assert!(!out.contains("- one"), "{out}");
        assert!(out.contains("A heading"));
        assert!(out.contains("• one"));
    }

    #[test]
    fn an_ordered_list_counts_and_a_nested_one_indents() {
        let out = text("1. first\n2. second\n   - inner\n", 80);
        assert_eq!(out[0], "1. first");
        assert_eq!(out[1], "2. second");
        assert_eq!(out[2], "  ◦ inner");
    }

    /// A **loose** list — items separated by blank lines — keeps its markers.
    ///
    /// The regression this exists for: pulldown-cmark wraps every item of a
    /// loose list in a paragraph, `Tag::Paragraph` flushes before it, and the
    /// flush used to clear the pending marker. Every bullet of every loose
    /// list was silently dropped and the list rendered as indented prose. A
    /// tight list does not go through a paragraph, so every list test written
    /// before this one passed while the case a model actually produces was
    /// broken.
    #[test]
    fn a_loose_list_keeps_the_markers_a_tight_one_never_lost() {
        let out = text("- first\n\n- second\n\n  continued\n", 80);
        assert_eq!(out[0], "• first");
        assert_eq!(out[1], "• second");
        // The item's second paragraph is separated, and indented to the column
        // the bullet set rather than to column zero.
        assert_eq!(out[2], "");
        assert_eq!(out[3], "  continued");

        let ordered = text("1. first\n\n2. second\n", 80);
        assert_eq!(ordered[0], "1. first");
        assert_eq!(ordered[1], "2. second");
    }

    /// A quote in two paragraphs is one quote. The separator carries the bar,
    /// because a break in the left margin reads as two quotes.
    #[test]
    fn a_blank_line_inside_a_quote_still_carries_its_bar() {
        let out = text("> first\n>\n> second\n", 80);
        assert_eq!(out[0], "▌ first");
        assert_eq!(out[1].trim_end(), "▌");
        assert_eq!(out[2], "▌ second");
    }

    /// A wrapped list item stays in its own column. The alternative — a
    /// continuation starting at column zero — reads as a new item.
    #[test]
    fn a_wrapped_list_item_keeps_its_column() {
        let out = text("- one two three four five six seven eight\n", 20);
        assert_eq!(out[0], "• one two three four");
        assert!(
            out[1].starts_with("  "),
            "continuation is indented under the text: {:?}",
            out[1]
        );
        assert!(out[1].trim_start().starts_with("five"));
    }

    /// The author's newline inside a paragraph is not a layout instruction:
    /// honouring it produces a column as wide as *their* editor was.
    #[test]
    fn a_soft_break_is_a_space_and_the_renderer_owns_the_wrap() {
        let out = text("one two\nthree four\nfive six seven\n", 20);
        assert_eq!(out, vec!["one two three four", "five six seven"]);
    }

    #[test]
    fn a_fenced_block_is_verbatim_and_never_wrapped() {
        let source = "```rust\nfn main() { println!(\"a very long line indeed, wider than the terminal\"); }\n```\n";
        let out = text(source, 30);
        assert!(out.iter().any(|line| line.contains("fn main()")));
        assert!(
            out.iter().any(|line| line.len() > 30),
            "code is not re-flowed: {out:#?}"
        );
        assert_eq!(out[0], "rust", "the language is named");
    }

    #[test]
    fn a_table_becomes_a_grid_with_its_alignment() {
        let source = "| a | b |\n|:--|--:|\n| 1 | 2 |\n";
        let out = text(source, 40);
        assert!(out.iter().any(|line| line.starts_with('┌')));
        assert!(out.iter().any(|line| line.contains("│ 1 │ 2 │")));
    }

    /// A table whose cells contain inline code keeps the colour through
    /// layout. This is the case a string-based renderer cannot do: the escape
    /// is inside the cell before the column width is known.
    #[test]
    fn a_styled_cell_survives_layout_and_does_not_break_the_alignment() {
        let source = "| flag | note |\n|---|---|\n| `--no-color` | off |\n";
        let lines = render(source, 60);
        let rendered = crate::render::block(&lines, Depth::TrueColor);
        assert!(rendered.contains("38;2;174;185;213m--no-color"));
        let widths: Vec<usize> = lines
            .iter()
            .filter(|line| line.text().starts_with(['┌', '│', '├', '└']))
            .map(Line::width)
            .collect();
        assert!(
            widths.windows(2).all(|pair| pair[0] == pair[1]),
            "{widths:?}"
        );
    }

    #[test]
    fn a_block_quote_gets_a_bar_and_not_a_chevron() {
        let out = text("> quoted\n", 40);
        assert_eq!(out[0], "▌ quoted");
    }

    #[test]
    fn strikethrough_is_an_attribute_not_a_pair_of_tildes() {
        let out = bytes("~~gone~~", 40);
        assert!(out.contains("\x1b[9mgone"), "{out:?}");
        assert!(!out.contains('~'));
    }

    /// Every construct at `Depth::None` is the text and nothing else — the
    /// property `CLI.md` §4.5 turns on.
    #[test]
    fn nothing_escapes_at_depth_none() {
        let source = "# h\n\n**b** *i* `c` [l](http://x) ~~s~~\n\n> q\n\n- i\n\n| a |\n|---|\n| 1 |\n\n---\n";
        let out = crate::render::block(&render(source, 60), Depth::None);
        assert!(!out.contains('\x1b'), "{out:?}");
    }

    #[test]
    fn there_is_never_a_leading_or_a_double_blank_line() {
        let out = text("# h\n\n\n\npara\n\n\n\npara\n", 40);
        assert!(!out.first().is_some_and(String::is_empty));
        assert!(!out.last().is_some_and(String::is_empty));
        assert!(
            !out.windows(2)
                .any(|pair| pair[0].is_empty() && pair[1].is_empty()),
            "{out:#?}"
        );
    }

    #[test]
    fn an_empty_document_renders_nothing() {
        assert!(render("", 80).is_empty());
        assert!(render("   \n\n  \n", 80).is_empty());
    }
}
