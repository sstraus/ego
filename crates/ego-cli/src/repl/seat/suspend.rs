//! `Ctrl+Z`: stop ego as a shell job and take the terminal back on `fg`.
//!
//! Raw mode clears `ISIG`, so the tty driver no longer turns `Ctrl+Z` into
//! `SIGTSTP`; the key reaches the editor as an ordinary event, and stopping is
//! ego's own job. It is done in place, on the thread that reads keys, so every
//! read — the composer and each modal — suspends the same way and none of them
//! answers its question: `raise` does not return until `SIGCONT` has resumed
//! the process, and what follows it is the whole of the continue handling.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// `Ctrl+Z`, with or without `Shift`. A third modifier asks for another key.
pub(super) fn is_suspend_key(key: &KeyEvent) -> bool {
    key.kind != KeyEventKind::Release
        && matches!(key.code, KeyCode::Char(c) if c.eq_ignore_ascii_case(&'z'))
        && key.modifiers.difference(KeyModifiers::SHIFT) == KeyModifiers::CONTROL
}

/// Stop until `SIGCONT`, then return the terminal size to repaint for.
///
/// The terminal goes back to the shell as the shell expects to find it:
/// cooked mode, bracketed paste off, and the cursor on a fresh line below the
/// composer so the shell's job message overwrites nothing. Time spent stopped
/// is recorded so a running command's timeout does not charge it.
#[cfg(unix)]
pub(super) fn suspend_job() -> Result<(u16, u16), &'static str> {
    use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
    use crossterm::terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode, size};
    use nix::sys::signal::{Signal, raise};
    use std::io::Write as _;

    let mut out = std::io::stdout();
    let _ = crossterm::execute!(out, DisableBracketedPaste, Clear(ClearType::FromCursorDown));
    let _ = out.write_all(b"\r\n");
    let _ = out.flush();
    let _ = disable_raw_mode();
    {
        let _stopped = ego_tools::suspension::begin();
        // `raise` and not `kill`: it targets this thread, so the stop has
        // happened before it returns. A process-directed signal may be taken
        // by another thread while this one runs on and re-enters raw mode.
        // With `SIGTSTP` ignored or the group orphaned there is no stop, and
        // the terminal is simply put back.
        let _ = raise(Signal::SIGTSTP);
    }
    let _ = enable_raw_mode();
    let _ = crossterm::execute!(out, EnableBracketedPaste);
    size().map_err(|_| "the terminal size could not be read after resuming")
}

#[cfg(not(unix))]
pub(super) fn suspend_job() -> Result<(u16, u16), &'static str> {
    Err("Ctrl+Z is not supported on Windows: the console has no job control to suspend ego")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    /// Plain `z` and `Ctrl+Alt+Z` must reach the editor: a match that ignored
    /// modifiers would suspend ego while the user types the letter z.
    #[test]
    fn only_control_z_suspends() {
        assert!(is_suspend_key(&key(
            KeyCode::Char('z'),
            KeyModifiers::CONTROL
        )));
        assert!(is_suspend_key(&key(
            KeyCode::Char('Z'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        )));
        assert!(!is_suspend_key(&key(
            KeyCode::Char('z'),
            KeyModifiers::NONE
        )));
        assert!(!is_suspend_key(&key(
            KeyCode::Char('z'),
            KeyModifiers::CONTROL | KeyModifiers::ALT
        )));
        assert!(!is_suspend_key(&key(
            KeyCode::Char('x'),
            KeyModifiers::CONTROL
        )));
    }
}
