//! Encode untrusted text for human-readable terminal surfaces.

use std::fmt::{self, Write as _};

/// Text with terminal controls and directional controls visibly escaped.
pub struct TerminalSafe<'a>(pub &'a str);

impl fmt::Display for TerminalSafe<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for character in self.0.chars() {
            if character.is_control() || is_bidi_control(character) {
                for escaped in character.escape_default() {
                    f.write_char(escaped)?;
                }
            } else {
                f.write_char(character)?;
            }
        }
        Ok(())
    }
}

/// Render text without allowing it to act on or reorder a terminal.
pub fn terminal_safe(text: &str) -> String {
    TerminalSafe(text).to_string()
}

/// Render tool output without SGR decoration. Other terminal and directional
/// controls keep the same visible encoding as [`terminal_safe`].
/// Only complete SGR sequences are removed, not cursor commands or OSC payloads.
pub fn terminal_output_safe(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        if character == '\x1b' {
            let mut sequence = characters.clone();
            if sequence.next() == Some('[') {
                let end = sequence.find(|c| !c.is_ascii_digit() && *c != ';' && *c != ':');
                if end == Some('m') {
                    characters = sequence;
                    continue;
                }
            }
        }
        plain.push(character);
    }
    terminal_safe(&plain)
}

/// Render one argument with shell quoting and visible control characters.
/// This is display text, not a command to execute.
pub fn shell_word(word: &str) -> String {
    if !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c))
    {
        word.to_owned()
    } else {
        format!("'{}'", terminal_safe(&word.replace('\'', "'\\''")))
    }
}

fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200e}' | '\u{200f}' | '\u{61c}')
}

#[cfg(test)]
mod tests {
    use super::*;

    // Catches: directional marks or isolates remain active outside the override case.
    #[test]
    fn all_directional_controls_are_visible_without_changing_unicode_text() {
        for c in [
            '\u{202a}', '\u{202b}', '\u{202c}', '\u{202d}', '\u{202e}', '\u{2066}', '\u{2067}',
            '\u{2068}', '\u{2069}', '\u{200e}', '\u{200f}', '\u{61c}',
        ] {
            let expected = format!("\\u{{{:x}}}", u32::from(c));
            assert_eq!(terminal_safe(&c.to_string()), expected);
            assert_eq!(shell_word(&c.to_string()), format!("'{expected}'"));
        }
        assert_eq!(terminal_safe("café 日本語"), "café 日本語");
    }

    // Catches: shared encoding changes quote boundaries or terminal control spellings.
    #[test]
    fn shell_words_preserve_quotes_empty_words_and_visible_controls() {
        assert_eq!(shell_word("it's"), "'it'\\''s'");
        assert_eq!(shell_word(""), "''");
        assert_eq!(shell_word("/bin/echo"), "/bin/echo");
        assert_eq!(terminal_safe("a\u{1b}[2Jb\n\t"), "a\\u{1b}[2Jb\\n\\t");
    }
}
