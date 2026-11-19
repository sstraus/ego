//! Markdown, rendered by the harness — because nobody else can.
//!
//! Markdown rendering separates style from literal text. Two rules:
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

use super::caps::{Capabilities, Depth, OscProfile};
use super::line::{Line, Span};
use super::style::{Palette, Style};
use super::table::{Align, Table};

/// Turn a markdown document into rendered rows.
///
/// `width` lays out containers and tables. Top-level prose stays on logical
/// lines so each terminal viewer can wrap it at its own edge. Only the TUIC
/// OSC profile preserves host protocol marker boundaries.
pub fn render(source: &str, capabilities: Capabilities, base: Option<&url::Url>) -> Vec<Line> {
    let tuic = capabilities.osc == OscProfile::Tuic;
    let mut renderer = Renderer::new(capabilities.width);
    renderer.hyperlinks = capabilities.hyperlinks;
    renderer.link_base = base.cloned();
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
                renderer.host_line = tuic && host_marker_line(&source[range.start..]);
            }
            Event::SoftBreak | Event::HardBreak if paragraph => {
                let next_host = tuic && host_marker_line(&source[range.end..]);
                if renderer.host_line
                    || next_host
                    || (tuic && source[range.end..].starts_with("💬 "))
                {
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
    link_destination: Option<String>,
    link_label: String,
    hyperlinks: bool,
    link_base: Option<url::Url>,
    /// The list marker owed to the first row of the item being built. Held
    /// out of `current` so it is not measured twice — see [`Renderer::flush`].
    marker: Option<Line>,
    /// Host markers must reach the terminal as one logical line so it owns wrapping.
    host_line: bool,
    /// Top-level paragraphs leave wrapping to the terminal viewer.
    paragraph: bool,
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
            link_destination: None,
            link_label: String::new(),
            hyperlinks: false,
            link_base: None,
            marker: None,
            host_line: false,
            paragraph: false,
        }
    }

    fn resolve_link(&self, destination: &str) -> Option<String> {
        if cfg!(windows) && std::path::Path::new(destination).is_absolute() {
            return url::Url::from_file_path(destination).ok().map(Into::into);
        }
        url::Url::parse(destination)
            .ok()
            .or_else(|| self.link_base.as_ref()?.join(destination).ok())
            .or_else(|| url::Url::from_file_path(destination).ok())
            .map(Into::into)
    }

    fn start_link(&mut self, destination: &str) {
        self.link_destination = Some(destination.to_owned());
        self.link_label.clear();
        self.link = self
            .hyperlinks
            .then(|| self.resolve_link(destination))
            .flatten();
    }

    fn style(&self) -> Style {
        self.styles.last().copied().unwrap_or(Style::PLAIN)
    }

    fn push_style(&mut self, change: impl FnOnce(Style) -> Style) {
        let style = change(self.style());
        self.styles.push(style);
    }

    fn text(&mut self, text: &str) {
        if self.link_destination.is_some() {
            self.link_label.push_str(text);
        }
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
        if std::mem::take(&mut self.host_line)
            || (self.paragraph && self.lists.is_empty() && self.quote == 0)
        {
            self.out.push(taken);
            return;
        }
        let marker = self.marker.take();
        let rows = taken.wrap(body);
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
                if self.link_destination.is_some() {
                    self.link_label.push_str(&code);
                }
                self.current.push(match &self.link {
                    Some(uri) => Span::linked(code.to_string(), style, uri),
                    None => Span::new(code.to_string(), style),
                });
            }
            // A soft break is a newline the author typed inside a paragraph.
            // It becomes a space: the viewer owns the prose wrap column, and
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
                self.paragraph = true;
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
                self.start_link(dest_url.as_ref());
                let actionable = self.link.is_some();
                self.push_style(|style| {
                    if actionable {
                        style.with_fg(Palette::LINK).underline()
                    } else {
                        style
                    }
                });
            }
            Tag::Image { dest_url, .. } => {
                // Without image protocol support, name the image instead of dropping it.
                self.text("🖼 ");
                self.start_link(dest_url.as_ref());
                let actionable = self.link.is_some();
                self.push_style(|style| {
                    if actionable {
                        style.with_fg(Palette::LINK).underline()
                    } else {
                        style
                    }
                });
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
            TagEnd::Paragraph => {
                self.flush();
                self.paragraph = false;
            }
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
                if let Some(destination) = self.link_destination.take()
                    && (!self.hyperlinks || self.resolve_link(&destination).is_none())
                    && self.link_label != destination
                {
                    self.text(&format!(" ({destination})"));
                }
                self.link_label.clear();
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

    fn at(width: usize, osc: OscProfile) -> Capabilities {
        Capabilities {
            width,
            osc,
            hyperlinks: true,
            ..Capabilities::plain()
        }
    }

    fn text(source: &str, width: usize) -> Vec<String> {
        render(source, at(width, OscProfile::Tuic), None)
            .iter()
            .map(Line::text)
            .collect()
    }

    fn bytes(source: &str, width: usize) -> String {
        crate::render::block(
            &render(source, at(width, OscProfile::Tuic), None),
            Depth::TrueColor,
        )
    }

    // Catches: non-TUIC profiles accidentally retain marker-specific Markdown boundaries.
    #[test]
    fn non_tuic_profiles_render_markers_as_ordinary_markdown() {
        for host in [OscProfile::None, OscProfile::Standard] {
            let source =
                "intent: task\nsuggest: **try this**\nTUICommander v2.0.0 is connected.\n💬 answer";
            assert_eq!(
                render(source, at(80, host), None)
                    .iter()
                    .map(Line::text)
                    .collect::<Vec<_>>(),
                ["intent: task suggest: try this TUICommander v2.0.0 is connected. 💬 answer"]
            );
        }
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

    // Catches: adjacent answer markers merge or styled tails acquire hard wraps.
    #[test]
    fn marked_answers_keep_boundaries_and_unwrapped_styled_tails() {
        let source = format!(
            "💬 **{}**\n💬 second answer",
            "accented perché l’eseguibile ".repeat(10).trim_end()
        );
        let rows = text(&source, 80);
        let second = rows
            .iter()
            .position(|row| row == "💬 second answer")
            .unwrap();
        assert_eq!(second, 1);
        assert_eq!(
            rows[0],
            format!(
                "💬 {}",
                "accented perché l’eseguibile ".repeat(10).trim_end()
            )
        );
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
    fn a_soft_break_is_a_space_and_the_viewer_owns_the_wrap() {
        let out = text("one two\nthree four\nfive six seven\n", 20);
        assert_eq!(out, vec!["one two three four five six seven"]);
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
        let lines = render(source, at(60, OscProfile::Standard), None);
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

    /// At `Depth::None`, every construct renders as plain text.
    #[test]
    fn nothing_escapes_at_depth_none() {
        let source = "# h\n\n**b** *i* `c` [l](http://x) ~~s~~\n\n> q\n\n- i\n\n| a |\n|---|\n| 1 |\n\n---\n";
        let out = crate::render::block(
            &render(source, at(60, OscProfile::Standard), None),
            Depth::None,
        );
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
        assert!(render("", at(80, OscProfile::Standard), None).is_empty());
        assert!(render("   \n\n  \n", at(80, OscProfile::Standard), None).is_empty());
    }
}
