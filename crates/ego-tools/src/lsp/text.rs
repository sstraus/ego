//! The server table and the unit conversions between the model and LSP.

use std::path::Path;

/// A language server command ego knows. ego chooses it from the file
/// extension; the model never supplies a command.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Server {
    pub(crate) argv: &'static [&'static str],
}

const RUST: Server = Server {
    argv: &["rust-analyzer"],
};
const GO: Server = Server { argv: &["gopls"] };
const CLANGD: Server = Server { argv: &["clangd"] };
const PYTHON: Server = Server {
    argv: &["pyright-langserver", "--stdio"],
};
const TYPESCRIPT: Server = Server {
    argv: &["typescript-language-server", "--stdio"],
};

/// The extensions with a server, for the refusal text.
pub(crate) const SUPPORTED: &str = ".rs (rust-analyzer), .go (gopls), .c/.h/.cc/.cpp/.hpp (clangd), .py (pyright-langserver), .ts/.tsx/.js/.jsx (typescript-language-server)";

/// The server and the LSP language identifier for `path`.
pub(crate) fn server_for(path: &Path) -> Option<(&'static Server, &'static str)> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "rs" => (&RUST, "rust"),
        "go" => (&GO, "go"),
        "c" | "h" => (&CLANGD, "c"),
        "cc" | "cpp" | "cxx" | "hh" | "hpp" | "hxx" => (&CLANGD, "cpp"),
        "py" | "pyi" => (&PYTHON, "python"),
        "ts" | "mts" | "cts" => (&TYPESCRIPT, "typescript"),
        "tsx" => (&TYPESCRIPT, "typescriptreact"),
        "js" | "mjs" | "cjs" => (&TYPESCRIPT, "javascript"),
        "jsx" => (&TYPESCRIPT, "javascriptreact"),
        _ => return None,
    })
}

/// Line `line` (0-based) of `text`, without its line ending.
pub(crate) fn line_of(text: &str, line: usize) -> Option<&str> {
    text.split('\n')
        .nth(line)
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
}

/// A 1-based character column to the 0-based UTF-16 offset LSP uses.
/// A column past the end of the line is the end of the line.
pub(crate) fn utf16_offset(line: &str, column: u32) -> u32 {
    line.chars()
        .take(column.saturating_sub(1) as usize)
        .map(|c| c.len_utf16() as u32)
        .sum()
}

/// A 0-based UTF-16 offset from a server to a 1-based character column.
pub(crate) fn char_column(line: &str, offset: u32) -> u32 {
    let mut units = 0;
    let mut chars = 0;
    for c in line.chars() {
        if units >= offset {
            break;
        }
        units += c.len_utf16() as u32;
        chars += 1;
    }
    chars + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    // Catches: columns counted in bytes or UTF-16 units instead of
    // characters, so a symbol after an accented letter or an emoji is
    // looked up one or two positions off and the server answers for the
    // wrong token. The line and offset are from the gopls recording.
    #[test]
    fn columns_after_multi_unit_characters_round_trip_to_the_server_offset() {
        let line = "\ta := \"héllo😀\"; b := Greeting(a)";
        let column = line[..line.find("Greeting").unwrap()].chars().count() as u32 + 1;
        assert_eq!(
            utf16_offset(line, column),
            22,
            "gopls placed Greeting at 22"
        );
        assert_eq!(char_column(line, 22), column);
        assert_eq!(utf16_offset(line, 1), 0);
        assert_eq!(
            utf16_offset(line, 1_000),
            line.encode_utf16().count() as u32
        );
    }

    // Catches: a CRLF file reporting a carriage return at the end of every
    // source line shown to the model.
    #[test]
    fn lines_lose_their_carriage_return() {
        assert_eq!(line_of("a\r\nb\r\n", 1), Some("b"));
        assert_eq!(line_of("a\nb", 2), None);
    }
}
