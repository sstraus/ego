//! maintainer-selected startup artwork: the braille εgo with a speed trail and a
//! magenta → violet → cyan gradient. Preserve the recorded glyphs and colours.
use super::Depth;
use super::style::{Rgb, Style};

pub(crate) const WIDTH: usize = 25;

const ROWS: [&str; 3] = [
    "⠠⠀⠠⠀⠤⠀⠤⠄⠤⢤⡴⠋⠉⠉⠀⣴⠋⠉⣷⠀⢠⡞⠉⢳⡄",
    "⠂⠀⠂⠐⠂⠐⠒⠐⠒⢶⡋⠉⠁⠀⠀⠻⣄⣀⣿⠀⠸⣇⠀⣸⠇",
    "⠈⠀⠈⠀⠉⠀⠉⠁⠉⠉⠉⠉⠉⠀⠀⠲⣄⣠⠟⠀⠀⠈⠉⠁⠀",
];

/// Columns the trail occupies in the original 27-cell design (two blank
/// leading cells were trimmed; colours keep the original column index).
const TRAIL: f32 = 10.0;
const TRIMMED: usize = 2;
const LETTERS: f32 = 17.0;
const TRAIL_DARK: Rgb = Rgb(40, 30, 70);
const MAGENTA: Rgb = Rgb(255, 60, 200);
const VIOLET: Rgb = Rgb(170, 80, 255);
const CYAN: Rgb = Rgb(40, 230, 255);

fn lerp(from: Rgb, to: Rgb, t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    let channel = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * t) as u8;
    Rgb(
        channel(from.0, to.0),
        channel(from.1, to.1),
        channel(from.2, to.2),
    )
}

fn colour(row: usize, column: usize) -> Rgb {
    let column = (column + TRIMMED) as f32;
    if column < TRAIL {
        return lerp(TRAIL_DARK, MAGENTA, column / TRAIL);
    }
    let t = ((column - TRAIL + row as f32 * 0.5) / LETTERS).clamp(0.0, 1.0);
    if t < 0.5 {
        lerp(MAGENTA, VIOLET, t * 2.0)
    } else {
        lerp(VIOLET, CYAN, (t - 0.5) * 2.0)
    }
}

pub(crate) fn render(depth: Depth) -> String {
    let mut out = String::new();
    for (row, text) in ROWS.iter().enumerate() {
        let mut open = String::new();
        for (column, glyph) in text.chars().enumerate() {
            let next = Style::fg(colour(row, column)).open(depth);
            if next != open {
                out.push_str(&next);
                open = next;
            }
            out.push(glyph);
        }
        if !open.is_empty() {
            out.push_str("\x1b[0m");
        }
        out.push('\n');
    }
    out
}
