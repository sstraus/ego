//! maintainer-selected startup artwork. Preserve the recorded glyphs and SGR bytes.
use super::Depth;

pub(crate) const WIDTH: usize = 17;

pub(crate) fn render(depth: Depth) -> &'static str {
    if depth == Depth::None {
        concat!(
            "⠀⢀⡴⠋⠉⠉⠀⣴⠋⠉⣷⠀⢠⡞⠉⢳⡄\n",
            "⠀⢴⡋⠉⠁⠀⠀⠻⣄⣀⣿⠀⠸⣇⠀⣸⠇\n",
            "⠀⠀⠉⠉⠉⠀⠀⠲⣄⣠⠟⠀⠀⠈⠉⠁⠀\n",
        )
    } else {
        concat!(
            "\x1b[1;35m⠀⢀⡴⠋⠉⠉\x1b[36m⠀⣴⠋⠉⣷⠀⢠⡞⠉⢳⡄\x1b[0m\n",
            "\x1b[1;35m⠀⢴⡋⠉⠁⠀\x1b[36m⠀⠻⣄⣀⣿⠀⠸⣇⠀⣸⠇\x1b[0m\n",
            "\x1b[1;35m⠀⠀⠉⠉⠉⠀\x1b[36m⠀⠲⣄⣠⠟⠀⠀⠈⠉⠁⠀\x1b[0m\n",
        )
    }
}
