//! Colour named once, spelled by the gate.
//!
//! Every colour in this build is a 24-bit [`Rgb`]. Nothing anywhere names an
//! SGR index, because a palette written in indices cannot be downgraded — it
//! is *already* the downgrade, taken before anyone knew which terminal was
//! listening. [`Style::paint`] takes the [`Depth`] the gate decided and spells
//! the deepest form allowed, snapping to the xterm cube or to the sixteen
//! basics only where it must.
//!
//! `docs/architecture.md` §2 is
//! the reason `fg` and `bg` are separate options rather than one "colour": the
//! diff writes background and the syntax pass writes foreground, and a type
//! that could hold only one of them would force one of the two to win.

use std::fmt::Write as _;

use super::caps::Depth;

/// A colour, as the palette states it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    /// The xterm-256 index that looks most like this colour.
    ///
    /// Greys are checked against the 24-step ramp *and* against the cube,
    /// because the cube also contains greys and for some values it is the
    /// closer of the two; taking the ramp unconditionally is the common bug
    /// and it makes near-white land two steps off.
    fn ansi256(self) -> u8 {
        let Rgb(r, g, b) = self;
        let cube_index = |value: u8| -> u8 {
            // The cube's levels are 0, 95, 135, 175, 215, 255 — not evenly
            // spaced, so a division would put everything dark in the wrong
            // cell.
            const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
            let mut best = 0;
            let mut best_distance = u16::MAX;
            for (index, level) in LEVELS.iter().enumerate() {
                let distance = value.abs_diff(*level) as u16;
                if distance < best_distance {
                    best_distance = distance;
                    best = index as u8;
                }
            }
            best
        };
        let (ri, gi, bi) = (cube_index(r), cube_index(g), cube_index(b));
        const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
        let cube = Rgb(
            LEVELS[ri as usize],
            LEVELS[gi as usize],
            LEVELS[bi as usize],
        );

        let grey_step = ((u16::from(r) + u16::from(g) + u16::from(b)) / 3).min(255) as u8;
        // The ramp runs 8, 18, … 238 at index 232..=255.
        let ramp = (u16::from(grey_step).saturating_sub(8) / 10).min(23) as u8;
        let grey_value = 8 + ramp * 10;
        let grey = Rgb(grey_value, grey_value, grey_value);

        if self.distance(grey) < self.distance(cube) {
            232 + ramp
        } else {
            16 + 36 * ri + 6 * gi + bi
        }
    }

    /// The SGR 30-37/90-97 code for this colour. `offset` is 0 for foreground
    /// and 10 for background.
    ///
    /// **Not** nearest-neighbour in RGB, which is the obvious implementation
    /// and the wrong one. The palette's most-used colour is
    /// [`Palette::CODE`] — a muted blue — and its nearest of the sixteen,
    /// by Euclidean distance and by redmean alike, is *white*: the pale blue
    /// is closer to `(229,229,229)` than to `(92,92,255)` by a factor of
    /// three. That answer is defensible as arithmetic and useless as a
    /// rendering, because inline code would become indistinguishable from
    /// prose at exactly the depth where nothing else distinguishes it.
    ///
    /// So hue is preserved and brightness is spent on choosing normal against
    /// bright. A colour with *no* hue to preserve does not fall back to the
    /// nearest grey — it names no colour at all, for the reason given at the
    /// achromatic branch below.
    fn ansi16(self, offset: u8) -> Rung {
        let Rgb(r, g, b) = self;
        let (r, g, b) = (
            f32::from(r) / 255.0,
            f32::from(g) / 255.0,
            f32::from(b) / 255.0,
        );
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let chroma = max - min;
        // Below this there is no hue worth keeping and the colour is a grey.
        // 0.12 rather than 0: an almost-grey rendered as a saturated primary
        // is a worse lie than a grey rendered as a grey.
        if chroma < 0.12 {
            // **No colour at all**, and this is the important line in the
            // function.
            //
            // The four ANSI greys are 30 (black), 90, 37 and 97 (white). Every
            // one of them is invisible against one of the two backgrounds a
            // terminal can have: name 97 and the text vanishes on a light
            // theme, name 30 and it vanishes on a dark one. There is no safe
            // grey, because the sixteen are defined against a background this
            // process cannot see and must not guess.
            //
            // The terminal's *own* default foreground is legible against its
            // own background by definition — it is the one colour that is
            // correct on both polarities. So an achromatic colour spends no
            // colour and keeps its place in the hierarchy through SGR 2
            // instead, which dims relative to whatever that default is.
            //
            // xAI's harness reaches the same conclusion from the same failure
            // (`xai-grok-markdown/src/colors.rs:252`, `polarity_safe_syntax_ansi`).
            // The palette expresses "quieter than body text" as *darker*: its
            // greys are 85, 128 and 153 for rules, gutters and quotes, against
            // 248 for code. The boundary goes between those two groups, so a
            // grey that was chosen to recede still recedes and one that was
            // chosen to carry text still carries it.
            const RECEDES_BELOW: f32 = 0.75;
            return Rung::Inherit {
                dim: max < RECEDES_BELOW,
            };
        }
        // Sector of the hue circle, in sixths. The three primaries and the
        // three secondaries are exactly the six ANSI chromatic slots.
        let hue = if max == r {
            (g - b) / chroma
        } else if max == g {
            2.0 + (b - r) / chroma
        } else {
            4.0 + (r - g) / chroma
        };
        let sector = ((hue.rem_euclid(6.0)) + 0.5).floor() as u8 % 6;
        let base = match sector {
            0 => 31, // red
            1 => 33, // yellow
            2 => 32, // green
            3 => 36, // cyan
            4 => 34, // blue
            _ => 35, // magenta
        };
        // Bright when the colour is light, which is the axis the sixteen have
        // left once hue is spent.
        Rung::Code(if max > 0.62 {
            base + 60 + offset
        } else {
            base + offset
        })
    }

    fn distance(self, other: Rgb) -> u32 {
        let d = |a: u8, b: u8| {
            let delta = i32::from(a) - i32::from(b);
            (delta * delta) as u32
        };
        d(self.0, other.0) + d(self.1, other.1) + d(self.2, other.2)
    }
}

/// What a 24-bit colour becomes at the sixteen-colour rung.
///
/// Two answers and not one, because "the nearest of the sixteen" is not always
/// an answer worth giving — see [`Rgb::ansi16`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rung {
    /// One of the sixteen, already offset for foreground or background.
    Code(u8),
    /// Spend no colour and keep the terminal's own, which is the only one
    /// guaranteed legible against the terminal's own background.
    Inherit {
        /// Whether to ask for SGR 2, so a grey stays quieter than body text
        /// without naming a grey.
        dim: bool,
    },
}

/// One cell's appearance: two independent colour axes and the attributes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Style {
    pub fg: Option<Rgb>,
    pub bg: Option<Rgb>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub reverse: bool,
}

impl Style {
    pub const PLAIN: Style = Style {
        fg: None,
        bg: None,
        bold: false,
        dim: false,
        italic: false,
        underline: false,
        strike: false,
        reverse: false,
    };

    pub fn fg(colour: Rgb) -> Self {
        Style {
            fg: Some(colour),
            ..Style::PLAIN
        }
    }

    pub fn bg(colour: Rgb) -> Self {
        Style {
            bg: Some(colour),
            ..Style::PLAIN
        }
    }

    pub fn with_fg(mut self, colour: Rgb) -> Self {
        self.fg = Some(colour);
        self
    }

    pub fn with_bg(mut self, colour: Rgb) -> Self {
        self.bg = Some(colour);
        self
    }

    pub fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    pub fn dim(mut self) -> Self {
        self.dim = true;
        self
    }

    pub fn italic(mut self) -> Self {
        self.italic = true;
        self
    }

    pub fn underline(mut self) -> Self {
        self.underline = true;
        self
    }

    pub fn strike(mut self) -> Self {
        self.strike = true;
        self
    }

    pub fn reverse(mut self) -> Self {
        self.reverse = true;
        self
    }

    fn is_plain(&self) -> bool {
        *self == Style::PLAIN
    }

    /// The opening sequence for this style at this depth, or `""`.
    ///
    /// One `ESC[…m` with every parameter in it rather than a run of them: the
    /// captures show Claude Code emitting two (`ESC[38;2;…m ESC[48;2;…m`) and
    /// it costs a sequence per cell for nothing. Fewer bytes is not the point
    /// — one sequence is atomic under a host that damages per-cell.
    pub fn open(&self, depth: Depth) -> String {
        if depth == Depth::None || self.is_plain() {
            return String::new();
        }
        let mut parameters: Vec<String> = Vec::new();
        if self.bold {
            parameters.push("1".to_string());
        }
        if self.dim {
            parameters.push("2".to_string());
        }
        if self.italic {
            parameters.push("3".to_string());
        }
        if self.underline {
            parameters.push("4".to_string());
        }
        if self.reverse {
            parameters.push("7".to_string());
        }
        if self.strike {
            parameters.push("9".to_string());
        }
        // An achromatic colour at the last rung asks for the terminal's own
        // foreground rather than one of the four unsafe greys, and carries its
        // place in the hierarchy as SGR 2. Added here rather than in
        // `colour_parameters` because it is an attribute, not a colour, and
        // because a background never dims — SGR 2 is a foreground concept.
        if depth == Depth::Ansi16
            && !self.dim
            && self
                .fg
                .is_some_and(|colour| matches!(colour.ansi16(0), Rung::Inherit { dim: true }))
        {
            parameters.push("2".to_string());
        }
        for (colour, background) in [(self.fg, false), (self.bg, true)] {
            let Some(colour) = colour else { continue };
            let parameter = Self::colour_parameters(colour, depth, background);
            if !parameter.is_empty() {
                parameters.push(parameter);
            }
        }
        if parameters.is_empty() {
            return String::new();
        }
        format!("\x1b[{}m", parameters.join(";"))
    }

    /// The closing sequence.
    ///
    /// `ESC[0m` and not the precise inverses (`22;23;24;29;39;49`): a renderer
    /// that closed selectively would have to know what was open underneath,
    /// and this build never nests — [`super::line::Line`] re-opens the active
    /// style at the start of every wrapped row for exactly that reason.
    pub fn close(&self, depth: Depth) -> &'static str {
        if depth == Depth::None || self.is_plain() {
            ""
        } else {
            "\x1b[0m"
        }
    }

    /// `text`, wrapped in this style. The one way a colour reaches a byte.
    pub fn paint(&self, depth: Depth, text: &str) -> String {
        if depth == Depth::None || self.is_plain() {
            return text.to_string();
        }
        let mut painted = String::with_capacity(text.len() + 16);
        painted.push_str(&self.open(depth));
        painted.push_str(text);
        painted.push_str(self.close(depth));
        painted
    }

    fn colour_parameters(colour: Rgb, depth: Depth, background: bool) -> String {
        let Rgb(r, g, b) = colour;
        match depth {
            Depth::None => String::new(),
            Depth::TrueColor => {
                let lead = if background { 48 } else { 38 };
                let mut out = String::new();
                let _ = write!(out, "{lead};2;{r};{g};{b}");
                out
            }
            Depth::Ansi256 => {
                let lead = if background { 48 } else { 38 };
                let mut out = String::new();
                let _ = write!(out, "{lead};5;{}", colour.ansi256());
                out
            }
            Depth::Ansi16 => match colour.ansi16(if background { 10 } else { 0 }) {
                Rung::Code(code) => code.to_string(),
                // Handled by the caller: there is no SGR parameter for "the
                // colour you already had", and 39/49 would *reset* it, which
                // is not the same thing inside a run that inherits nothing.
                Rung::Inherit { .. } => String::new(),
            },
        }
    }
}

/// Every colour this build can name, in one place.
///
/// The diff half is the palette read off the capture in the ADR — not copied
/// for its own sake, but because those six values are the only ones in this
/// file that were *measured* rather than chosen, and a diff whose two
/// backgrounds are not clearly ordered (line, then changed span) stops
/// carrying the second signal.
pub struct Palette;

impl Palette {
    // --- markdown ---------------------------------------------------------
    /// `**bold**` is weight and no colour; `` `code` `` is colour and no
    /// weight. Both rules are read off the capture, and together they are what
    /// keeps a sentence containing three identifiers from becoming a ransom
    /// note.
    pub const CODE: Rgb = Rgb(174, 185, 213);
    pub const HEADING: Rgb = Rgb(155, 181, 204);
    pub const LINK: Rgb = Rgb(150, 173, 209);
    pub const QUOTE: Rgb = Rgb(153, 153, 153);
    pub const RULE: Rgb = Rgb(85, 85, 85);
    pub const BULLET: Rgb = Rgb(155, 181, 204);
    /// Table borders. Dim on purpose: the grid is scaffolding and the cells
    /// are the content, and a full-brightness box wins the eye every time.
    pub const BORDER: Rgb = Rgb(85, 85, 85);

    // --- diff -------------------------------------------------------------
    pub const REMOVED_FG: Rgb = Rgb(216, 150, 154);
    pub const REMOVED_BG: Rgb = Rgb(61, 1, 0);
    pub const REMOVED_SPAN_BG: Rgb = Rgb(92, 2, 0);
    pub const ADDED_FG: Rgb = Rgb(151, 207, 167);
    pub const ADDED_BG: Rgb = Rgb(2, 40, 0);
    pub const ADDED_SPAN_BG: Rgb = Rgb(4, 71, 0);
    pub const GUTTER_FG: Rgb = Rgb(128, 128, 128);
    /// The base foreground inside a diff. Named rather than left to the
    /// terminal's default because the diff sets a background, and a terminal
    /// whose default foreground is dark renders black on dark red.
    pub const CODE_FG: Rgb = Rgb(248, 248, 242);

    // --- diagnostics ------------------------------------------------------
    pub const TOOL: Rgb = Rgb(155, 181, 204);
    pub const ERROR: Rgb = Rgb(221, 151, 158);
    pub const WARNING: Rgb = Rgb(217, 190, 129);
    pub const NOTE: Rgb = Rgb(153, 153, 153);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depth_none_writes_the_text_and_nothing_else() {
        let style = Style::fg(Rgb(1, 2, 3)).bold().underline();
        assert_eq!(style.paint(Depth::None, "hello"), "hello");
        assert_eq!(style.open(Depth::None), "");
        assert_eq!(style.close(Depth::None), "");
    }

    #[test]
    fn a_plain_style_is_free_at_every_depth() {
        for depth in [Depth::Ansi16, Depth::Ansi256, Depth::TrueColor] {
            assert_eq!(Style::PLAIN.paint(depth, "hello"), "hello");
        }
    }

    /// Both axes in one sequence, foreground first, matching the order the
    /// captured renderer uses — and, more to the point, *both present*: this
    /// is the type that makes §2's two passes composable.
    #[test]
    fn the_two_axes_are_independent_and_travel_together() {
        let style = Style::fg(Rgb(248, 248, 242)).with_bg(Rgb(2, 40, 0));
        assert_eq!(
            style.paint(Depth::TrueColor, "x"),
            "\x1b[38;2;248;248;242;48;2;2;40;0mx\x1b[0m"
        );
    }

    #[test]
    fn attributes_precede_colours() {
        let style = Style::fg(Rgb(1, 2, 3)).bold().dim().italic().strike();
        assert_eq!(style.open(Depth::TrueColor), "\x1b[1;2;3;9;38;2;1;2;3m");
    }

    /// A palette in indices cannot be downgraded, because it is already the
    /// downgrade. These are the same colour at three depths.
    #[test]
    fn one_colour_is_spelled_three_ways() {
        let style = Style::fg(Palette::CODE);
        assert_eq!(style.open(Depth::TrueColor), "\x1b[38;2;174;185;213m");
        assert_eq!(style.open(Depth::Ansi256), "\x1b[38;5;146m");
        assert_eq!(style.open(Depth::Ansi16), "\x1b[94m");
    }

    /// Nearest-neighbour in RGB answers *white* here, by a factor of three,
    /// and white is the one colour that makes inline code invisible. Hue is
    /// what has to survive the last rung.
    #[test]
    fn the_last_rung_keeps_the_hue_rather_than_the_distance() {
        let nearest_by_distance = {
            let white = Rgb(229, 229, 229);
            let bright_blue = Rgb(92, 92, 255);
            assert!(Palette::CODE.distance(white) < Palette::CODE.distance(bright_blue));
            "white"
        };
        assert_eq!(nearest_by_distance, "white");
        assert_eq!(Style::fg(Palette::CODE).open(Depth::Ansi16), "\x1b[94m");
    }

    /// The other half of the same rule, and the one that costs a colour rather
    /// than choosing one: **a grey names no colour at all**.
    ///
    /// Each of the four ANSI greys is invisible against one of the two
    /// backgrounds a terminal can have, and this process cannot see which it
    /// has. So a grey spends the terminal's own foreground — legible on both
    /// by definition — and keeps its rank with SGR 2 instead. A near-white
    /// stays undimmed, because it was the prominent one.
    #[test]
    fn a_grey_names_no_colour_because_there_is_no_safe_grey() {
        // Dim, not `\x1b[90m`: 90 is unreadable on a light theme.
        assert_eq!(Style::fg(Palette::NOTE).open(Depth::Ansi16), "\x1b[2m");
        assert_eq!(Style::fg(Rgb(0, 0, 0)).open(Depth::Ansi16), "\x1b[2m");
        // Nothing at all: white was the emphatic end of the scale, and on a
        // light terminal the emphatic colour is black.
        assert_eq!(Style::fg(Rgb(255, 255, 255)).open(Depth::Ansi16), "");
        // The attribute is not doubled when the style already asked for it.
        assert_eq!(
            Style::fg(Palette::NOTE).dim().open(Depth::Ansi16),
            "\x1b[2m"
        );
        // And a grey *background* simply goes: SGR 2 is a foreground concept,
        // and there is no safe grey there either.
        assert_eq!(Style::bg(Rgb(40, 40, 40)).open(Depth::Ansi16), "");
    }

    /// The rule is polarity-safety, not "avoid colour". A colour with a hue
    /// still gets one, because the six chromatic slots are legible on both
    /// backgrounds — that is what distinguishes them from the greys.
    #[test]
    fn a_colour_with_a_hue_still_gets_one_at_the_last_rung() {
        assert_eq!(
            Style::fg(Palette::REMOVED_FG).open(Depth::Ansi16),
            "\x1b[91m"
        );
        assert_eq!(Style::fg(Palette::CODE).open(Depth::Ansi16), "\x1b[94m");
    }

    #[test]
    fn a_background_downgrades_on_its_own_axis() {
        let style = Style::bg(Palette::ADDED_BG);
        assert_eq!(style.open(Depth::TrueColor), "\x1b[48;2;2;40;0m");
        assert_eq!(style.open(Depth::Ansi256), "\x1b[48;5;232m");
        assert_eq!(style.open(Depth::Ansi16), "\x1b[42m");
    }

    /// The common bug: taking the greyscale ramp for anything desaturated puts
    /// pure white two steps below white, because the ramp stops at 238 and the
    /// cube has 255.
    #[test]
    fn near_white_snaps_to_the_cube_and_not_to_the_ramp() {
        assert_eq!(Rgb(255, 255, 255).ansi256(), 231);
        assert_eq!(Rgb(0, 0, 0).ansi256(), 16);
        // A real grey does take the ramp: 128 is far from every cube level.
        assert_eq!(Rgb(128, 128, 128).ansi256(), 244);
    }

    /// The cube's levels are 0, 95, 135, 175, 215, 255. Dividing by 51 — the
    /// arithmetic that looks right — puts 95 in cell 1 by luck and 40 in cell
    /// 0 by luck, and 100 in the wrong one.
    #[test]
    fn the_cube_is_not_evenly_spaced() {
        assert_eq!(Rgb(95, 0, 0).ansi256(), 16 + 36);
        assert_eq!(Rgb(215, 0, 0).ansi256(), 16 + 36 * 4);
    }

    #[test]
    fn the_diff_backgrounds_are_ordered_so_the_span_reads_as_more() {
        // Not a colour-theory claim: the changed span must be lighter than the
        // line it sits in, or the word-level signal is invisible.
        let brightness = |Rgb(r, g, b): Rgb| u32::from(r) + u32::from(g) + u32::from(b);
        assert!(brightness(Palette::ADDED_SPAN_BG) > brightness(Palette::ADDED_BG));
        assert!(brightness(Palette::REMOVED_SPAN_BG) > brightness(Palette::REMOVED_BG));
    }
}
