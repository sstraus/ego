//! Tables laid out in cells, and a table that will not fit turned into
//! something that will.
//!
//! `docs/decisions/2026-08-22-two-axes-and-a-terminal-that-answers.md` §3. The
//! two decisions worth restating where the code is:
//!
//! **Width is measured, never assumed.** A model emits a markdown table with
//! no idea how wide the window is; alignment is entirely the renderer's job,
//! and it is arithmetic over [`super::line::cells`] rather than over `len()`.
//!
//! **A table that does not fit is re-shaped, not cut.** Truncating a column
//! deletes what the model said, which is the same class of defect as a
//! swallowed event; horizontal scrolling does not exist in a line-oriented
//! renderer that has to survive being piped into a log. So below a floor the
//! table becomes a labelled list, which is what the content wanted at that
//! width anyway.

use super::caps::Depth;
use super::line::{Line, Span};
use super::style::{Palette, Style};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Align {
    #[default]
    Left,
    Center,
    Right,
}

/// A parsed table, before it knows how wide the terminal is.
#[derive(Debug, Clone, Default)]
pub struct Table {
    /// One [`Line`] per cell. Already styled: inline code inside a cell is
    /// coloured before the table sees it, which is exactly why the layout
    /// cannot be done on strings.
    pub header: Vec<Line>,
    pub rows: Vec<Vec<Line>>,
    /// From markdown's `:---`, `:---:`, `---:`. One per column; short vectors
    /// are read as [`Align::Left`], because a malformed delimiter row is not
    /// worth refusing a table over.
    pub align: Vec<Align>,
}

/// `│ ` + ` │` per column, plus the closing `│`.
const CELL_OVERHEAD: usize = 3;
/// The narrowest a column may be squeezed to before the whole table is
/// abandoned for a list. Three cells holds an ellipsis or a short word; below
/// that every column is a stack of single letters and the grid is noise.
const MIN_COLUMN: usize = 3;

impl Table {
    pub fn columns(&self) -> usize {
        let widest_row = self.rows.iter().map(Vec::len).max().unwrap_or(0);
        self.header.len().max(widest_row)
    }

    fn align_of(&self, column: usize) -> Align {
        self.align.get(column).copied().unwrap_or_default()
    }

    /// The finished rows, ready for [`super::block`].
    pub fn render(&self, width: usize, depth: Depth) -> Vec<Line> {
        let columns = self.columns();
        if columns == 0 {
            return Vec::new();
        }
        let frame = columns * CELL_OVERHEAD + 1;
        let available = width.saturating_sub(frame);
        let (minimum, maximum) = self.content_widths(columns);

        if available < minimum.iter().sum::<usize>() || available < columns * MIN_COLUMN {
            return self.as_list(width, depth);
        }
        let widths = distribute(&minimum, &maximum, available);
        self.as_grid(&widths, depth)
    }

    /// For each column: the narrowest it can be without breaking a word, and
    /// the widest it would like to be.
    ///
    /// The minimum is the longest *word*, not the longest cell — a column of
    /// prose can be squeezed to one word per row, and it is only unbreakable
    /// tokens (a path, an identifier) that set a real floor.
    fn content_widths(&self, columns: usize) -> (Vec<usize>, Vec<usize>) {
        let mut minimum = vec![MIN_COLUMN; columns];
        let mut maximum = vec![0; columns];
        let mut visit = |cells: &[Line]| {
            for (index, cell) in cells.iter().enumerate() {
                if index >= columns {
                    continue;
                }
                maximum[index] = maximum[index].max(cell.width());
                let longest_word = cell
                    .text()
                    .split_whitespace()
                    .map(super::line::cells)
                    .max()
                    .unwrap_or(0);
                minimum[index] = minimum[index].max(longest_word.min(24));
            }
        };
        visit(&self.header);
        for row in &self.rows {
            visit(row);
        }
        // A column can never be asked to be wider than its widest content.
        for index in 0..columns {
            minimum[index] = minimum[index].min(maximum[index].max(MIN_COLUMN));
        }
        (minimum, maximum)
    }

    fn as_grid(&self, widths: &[usize], depth: Depth) -> Vec<Line> {
        let border = Style::fg(Palette::BORDER);
        let mut out = Vec::new();
        out.push(rule(widths, '┌', '┬', '┐', border));
        if !self.header.is_empty() {
            out.extend(self.body_row(&self.header, widths, true));
            out.push(rule(widths, '├', '┼', '┤', border));
        }
        for row in &self.rows {
            out.extend(self.body_row(row, widths, false));
        }
        out.push(rule(widths, '└', '┴', '┘', border));
        // `depth` is not consulted here: a `Line` carries styles, and
        // `super::block` decides whether they become bytes. Taking it as an
        // argument keeps the signature honest for the list form, which does
        // need it.
        let _ = depth;
        out
    }

    /// One logical row, which is as many terminal rows as its tallest cell.
    fn body_row(&self, cells: &[Line], widths: &[usize], header: bool) -> Vec<Line> {
        let border = Style::fg(Palette::BORDER);
        let wrapped: Vec<Vec<Line>> = widths
            .iter()
            .enumerate()
            .map(|(index, width)| {
                let cell = cells.get(index).cloned().unwrap_or_default();
                let cell = if header { emphasise(&cell) } else { cell };
                cell.wrap(*width)
            })
            .collect();
        let height = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);

        (0..height)
            .map(|row| {
                let mut line = Line::empty();
                for (index, width) in widths.iter().enumerate() {
                    line.push(Span::new("│", border));
                    line.push(Span::plain(" "));
                    let content = wrapped[index].get(row).cloned().unwrap_or_default();
                    for span in align(&content, *width, self.align_of(index)).spans {
                        line.push(span);
                    }
                    line.push(Span::plain(" "));
                }
                line.push(Span::new("│", border));
                line
            })
            .collect()
    }

    /// The degradation. One block per row, `header: value` per field.
    ///
    /// Not a narrower grid: below the floor every column holds one short word
    /// per line and the box characters outnumber the content. A list at 30
    /// columns is readable; a four-column table at 30 columns is not, at any
    /// choice of widths.
    fn as_list(&self, width: usize, depth: Depth) -> Vec<Line> {
        let _ = depth;
        let label = Style::fg(Palette::CODE).bold();
        let columns = self.columns();
        let mut out = Vec::new();
        for (position, row) in self.rows.iter().enumerate() {
            if position > 0 {
                out.push(Line::empty());
            }
            for index in 0..columns {
                let name = self
                    .header
                    .get(index)
                    .map(Line::text)
                    .unwrap_or_else(|| (index + 1).to_string());
                let value = row.get(index).cloned().unwrap_or_default();
                if value.text().trim().is_empty() {
                    continue;
                }
                let mut head = Line::empty();
                head.push(Span::new(format!("{name}: "), label));
                let indent = super::line::cells(&name) + 2;
                let body_width = width.saturating_sub(indent).max(MIN_COLUMN);
                // Soft: a value in this form is usually a path, a URL or an
                // identifier, and chopping one at the column costs the reader
                // a copy-paste. The emulator's own soft wrap handles the
                // overflow without losing the token.
                let mut wrapped = value.wrap_soft(body_width).into_iter();
                if let Some(first) = wrapped.next() {
                    for span in first.spans {
                        head.push(span);
                    }
                }
                out.push(head);
                for rest in wrapped {
                    let mut line = Line::plain(" ".repeat(indent));
                    for span in rest.spans {
                        line.push(span);
                    }
                    out.push(line);
                }
            }
        }
        // A header with no rows still says something: the shape of what was
        // empty.
        if self.rows.is_empty() && !self.header.is_empty() {
            out.push(Line::styled(
                self.header
                    .iter()
                    .map(Line::text)
                    .collect::<Vec<_>>()
                    .join(" · "),
                label,
            ));
        }
        out
    }
}

/// Distribute `available` cells over columns that want between `minimum` and
/// `maximum`.
///
/// Proportional to what each column still *wants* (`maximum - minimum`), not
/// to how wide it is: a 60-cell prose column and a 6-cell status column
/// splitting the surplus by total width would give the status column three
/// cells it has no use for while the prose column wraps.
fn distribute(minimum: &[usize], maximum: &[usize], available: usize) -> Vec<usize> {
    let wanted: usize = maximum.iter().sum();
    if wanted <= available {
        return maximum.to_vec();
    }
    let floor: usize = minimum.iter().sum();
    let mut widths = minimum.to_vec();
    let mut surplus = available.saturating_sub(floor);
    let appetite: usize = minimum
        .iter()
        .zip(maximum)
        .map(|(low, high)| high.saturating_sub(*low))
        .sum();
    if appetite == 0 || surplus == 0 {
        return widths;
    }
    // Largest remainder: hand out the integer share first, then the leftovers
    // one cell at a time to the hungriest columns, so the total lands exactly
    // on `available` instead of one or two cells short of it.
    let mut remainders: Vec<(usize, usize)> = Vec::new();
    for index in 0..widths.len() {
        let want = maximum[index].saturating_sub(minimum[index]);
        let share = want * surplus / appetite;
        widths[index] += share;
        remainders.push((want * surplus % appetite, index));
    }
    let handed: usize = widths.iter().sum::<usize>() - floor;
    surplus -= handed.min(surplus);
    remainders.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    for (_, index) in remainders {
        if surplus == 0 {
            break;
        }
        if widths[index] < maximum[index] {
            widths[index] += 1;
            surplus -= 1;
        }
    }
    widths
}

fn rule(widths: &[usize], left: char, join: char, right: char, style: Style) -> Line {
    let mut text = String::new();
    text.push(left);
    for (index, width) in widths.iter().enumerate() {
        if index > 0 {
            text.push(join);
        }
        for _ in 0..(width + 2) {
            text.push('─');
        }
    }
    text.push(right);
    Line::styled(text, style)
}

/// Pad a wrapped cell line to the column width, on the side alignment asks
/// for.
fn align(content: &Line, width: usize, alignment: Align) -> Line {
    let have = content.width();
    if have >= width {
        return content.truncate_to(width);
    }
    let gap = width - have;
    let (before, after) = match alignment {
        Align::Left => (0, gap),
        Align::Right => (gap, 0),
        Align::Center => (gap / 2, gap - gap / 2),
    };
    let mut line = Line::empty();
    if before > 0 {
        line.push(Span::plain(" ".repeat(before)));
    }
    for span in &content.spans {
        line.push(span.clone());
    }
    if after > 0 {
        line.push(Span::plain(" ".repeat(after)));
    }
    line
}

/// Header cells are bold, and keep whatever colour they already had.
///
/// Not "bold and re-coloured": a header cell containing inline code was
/// coloured by the markdown pass, and overwriting that would make the header
/// row the one place in the document where `` `code` `` does not look like
/// code.
fn emphasise(cell: &Line) -> Line {
    Line::of(
        cell.spans
            .iter()
            .map(|span| Span::new(span.text.clone(), span.style.bold()))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> Table {
        Table {
            header: vec![Line::plain("flag"), Line::plain("what it does")],
            rows: vec![
                vec![Line::plain("-q"), Line::plain("quiet")],
                vec![Line::plain("--no-color"), Line::plain("no colour")],
            ],
            align: vec![Align::Left, Align::Right],
        }
    }

    fn texts(lines: &[Line]) -> Vec<String> {
        lines.iter().map(Line::text).collect()
    }

    #[test]
    fn a_table_that_fits_is_as_wide_as_its_content() {
        let rendered = table().render(80, Depth::None);
        assert_eq!(
            texts(&rendered),
            vec![
                "┌────────────┬──────────────┐",
                "│ flag       │ what it does │",
                "├────────────┼──────────────┤",
                "│ -q         │        quiet │",
                "│ --no-color │    no colour │",
                "└────────────┴──────────────┘",
            ]
        );
    }

    /// Every row is the same width, in cells. The property a `len()`-based
    /// layout gets wrong the moment a cell contains an accent or an escape.
    #[test]
    fn every_row_is_the_same_width_even_with_wide_and_styled_cells() {
        let table = Table {
            header: vec![Line::plain("a"), Line::plain("b")],
            rows: vec![vec![
                Line::styled("日本語", Style::PLAIN.bold()),
                Line::plain("e\u{301}"),
            ]],
            align: vec![],
        };
        let rendered = table.render(40, Depth::None);
        let widths: Vec<usize> = rendered.iter().map(Line::width).collect();
        assert!(
            widths.windows(2).all(|pair| pair[0] == pair[1]),
            "ragged: {widths:?}"
        );
    }

    #[test]
    fn alignment_comes_from_the_delimiter_row() {
        let table = Table {
            header: vec![Line::plain("l"), Line::plain("c"), Line::plain("r")],
            rows: vec![vec![Line::plain("x"), Line::plain("x"), Line::plain("x")]],
            align: vec![Align::Left, Align::Center, Align::Right],
        };
        let rendered = table.render(40, Depth::None);
        // Columns are 1 cell wide, so alignment is invisible; widen them.
        let table = Table {
            rows: vec![vec![Line::plain("x"), Line::plain("x"), Line::plain("x")]],
            header: vec![
                Line::plain("left--"),
                Line::plain("centre"),
                Line::plain("right-"),
            ],
            ..table
        };
        assert!(!rendered.is_empty());
        let rendered = table.render(40, Depth::None);
        assert_eq!(rendered[3].text(), "│ x      │   x    │      x │");
    }

    /// The header keeps the colour a markdown pass gave it and gains weight.
    /// Overwriting the colour would make the header the one row where inline
    /// code does not look like inline code.
    #[test]
    fn a_header_gains_weight_without_losing_its_colour() {
        let table = Table {
            header: vec![Line::styled("--flag", Style::fg(Palette::CODE))],
            rows: vec![vec![Line::plain("x")]],
            align: vec![],
        };
        let rendered = table.render(40, Depth::TrueColor);
        let header = rendered[1].render(Depth::TrueColor);
        assert!(
            header.contains("\x1b[1;38;2;177;185;249m--flag"),
            "{header}"
        );
    }

    /// A cell too wide for its column wraps and the row grows taller. Nothing
    /// is lost, and the grid stays rectangular.
    #[test]
    fn a_long_cell_wraps_and_the_row_grows() {
        let table = Table {
            header: vec![Line::plain("k"), Line::plain("v")],
            rows: vec![vec![
                Line::plain("id"),
                Line::plain("one two three four five six seven"),
            ]],
            align: vec![],
        };
        let rendered = table.render(30, Depth::None);
        let body: Vec<String> = texts(&rendered);
        let joined = body.join("\n");
        for word in ["one", "seven"] {
            assert!(joined.contains(word), "{word} was dropped:\n{joined}");
        }
        let widths: Vec<usize> = rendered.iter().map(Line::width).collect();
        assert!(widths.iter().all(|width| *width <= 30), "{widths:?}");
        assert!(widths.windows(2).all(|pair| pair[0] == pair[1]));
    }

    /// The decision the ADR spends a paragraph on: below the floor the table
    /// becomes a list rather than losing columns. Truncating would delete what
    /// the model said.
    #[test]
    fn a_table_too_narrow_to_draw_becomes_a_list_and_keeps_every_value() {
        let table = Table {
            header: vec![
                Line::plain("provider"),
                Line::plain("endpoint"),
                Line::plain("credential"),
            ],
            rows: vec![vec![
                Line::plain("anthropic"),
                Line::plain("https://api.anthropic.com/v1/messages"),
                Line::plain("ANTHROPIC_API_KEY"),
            ]],
            align: vec![],
        };
        let rendered = table.render(28, Depth::None);
        let joined = texts(&rendered).join("\n");
        assert!(!joined.contains('┌'), "not a grid:\n{joined}");
        // Whole, not merely present: an unbreakable value is what the reader
        // is going to copy, and a URL chopped at column 28 is a URL they
        // cannot use. It is allowed to overflow; the emulator soft-wraps it.
        for value in [
            "anthropic",
            "https://api.anthropic.com/v1/messages",
            "ANTHROPIC_API_KEY",
        ] {
            assert!(joined.contains(value), "{value} was broken up:\n{joined}");
        }
        // Prose, which can break, still respects the width.
        let table = Table {
            header: vec![Line::plain("note")],
            rows: vec![vec![Line::plain(
                "a sentence long enough to need wrapping at this width",
            )]],
            align: vec![],
        };
        assert!(
            table
                .render(28, Depth::None)
                .iter()
                .all(|line| line.width() <= 28)
        );
    }

    #[test]
    fn a_row_shorter_than_the_header_is_padded_rather_than_refused() {
        let table = Table {
            header: vec![Line::plain("a"), Line::plain("b")],
            rows: vec![vec![Line::plain("only")]],
            align: vec![],
        };
        let rendered = table.render(40, Depth::None);
        assert_eq!(rendered.len(), 5);
        assert!(rendered[3].text().ends_with("│"));
    }

    #[test]
    fn an_empty_table_renders_nothing_rather_than_a_frame_around_nothing() {
        assert!(Table::default().render(80, Depth::None).is_empty());
    }

    /// The surplus goes where it is wanted. A 6-cell column and a 60-cell
    /// column splitting by total width would widen the narrow one for nothing.
    #[test]
    fn surplus_is_shared_by_appetite_not_by_size() {
        let widths = distribute(&[3, 3], &[6, 60], 30);
        assert_eq!(widths.iter().sum::<usize>(), 30);
        assert!(widths[1] > widths[0] * 3, "{widths:?}");
        // And a column never grows past what it asked for.
        assert!(widths[0] <= 6);
    }

    #[test]
    fn distribution_lands_exactly_on_the_available_width() {
        for available in 10..60 {
            let widths = distribute(&[3, 3, 3], &[17, 23, 29], available);
            let total: usize = widths.iter().sum();
            assert!(total <= available, "overflow at {available}: {widths:?}");
            assert!(
                total == available || widths == vec![17, 23, 29],
                "left {} cells unused at {available}: {widths:?}",
                available - total
            );
        }
    }
}
