//! Terminal shortcut grammar. Printable text and terminal control aliases are
//! deliberately not configurable: IME/AltGr input and host controls keep ownership.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::keys::{Intercept, intercept};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct Shortcut {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
    pub label: String,
}

impl Shortcut {
    pub fn parse(raw: &str) -> Result<Self, String> {
        let lower = raw.to_ascii_lowercase();
        let (code, modifiers, label) = if let Some(number) = lower.strip_prefix('f') {
            let number: u8 = number.parse().map_err(|_| invalid(raw))?;
            if !(1..=12).contains(&number) || lower != format!("f{number}") {
                return Err(invalid(raw));
            }
            (KeyCode::F(number), KeyModifiers::NONE, format!("F{number}"))
        } else {
            let (modifier, letter) = lower.split_once('+').ok_or_else(|| invalid(raw))?;
            let mut chars = letter.chars();
            let letter = chars.next().ok_or_else(|| invalid(raw))?;
            if !letter.is_ascii_alphabetic() || chars.next().is_some() {
                return Err(invalid(raw));
            }
            let (modifiers, prefix) = match modifier {
                "ctrl" => (KeyModifiers::CONTROL, "Ctrl"),
                "alt" | "option" | "meta" => (KeyModifiers::ALT, "Option"),
                _ => return Err(invalid(raw)),
            };
            // These bytes alias Tab, LF, CR, Backspace, interrupt, EOF and
            // job suspension on terminals without enhanced keyboard reporting.
            if modifiers == KeyModifiers::CONTROL
                && matches!(letter, 'c' | 'd' | 'h' | 'i' | 'j' | 'm' | 'z')
            {
                return Err(format!("reserved terminal control: {raw}"));
            }
            if !matches!(
                intercept(KeyCode::Char(letter), modifiers),
                Intercept::Editor
            ) {
                return Err(format!("reserved composer control: {raw}"));
            }
            (
                KeyCode::Char(letter),
                modifiers,
                format!("{prefix}+{}", letter.to_ascii_uppercase()),
            )
        };
        Ok(Self {
            code,
            modifiers,
            label,
        })
    }

    pub fn matches(&self, key: KeyEvent) -> bool {
        let modifiers = if key.modifiers == KeyModifiers::META {
            KeyModifiers::ALT
        } else {
            key.modifiers
        };
        let code = match key.code {
            KeyCode::Char(character) => KeyCode::Char(character.to_ascii_lowercase()),
            other => other,
        };
        self.code == code && self.modifiers == modifiers
    }
}

fn invalid(raw: &str) -> String {
    format!("invalid shortcut `{raw}`: use Ctrl+LETTER, Alt+LETTER, or F1 through F12")
}
