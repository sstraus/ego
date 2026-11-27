//! Direct owner input for explicit API-key registration, never a program result.
use std::io::{IsTerminal as _, Write as _};
use std::path::PathBuf;

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ego_core::credentials::SecretMaterial;
use ego_core::turn::CancelToken;

use super::AuthError;

const MAX_KEY_BYTES: usize = 4096;

fn io_error(source: std::io::Error) -> AuthError {
    AuthError::Io {
        path: PathBuf::from("credential input"),
        source,
    }
}

fn invalid_key() -> AuthError {
    AuthError::InvalidConfig(format!(
        "API key must contain {}..={MAX_KEY_BYTES} printable ASCII bytes without whitespace",
        ego_core::secrets::MIN_ENV_SECRET_LEN,
    ))
}

pub(super) fn valid(bytes: &[u8]) -> bool {
    (ego_core::secrets::MIN_ENV_SECRET_LEN..=MAX_KEY_BYTES).contains(&bytes.len())
        && bytes.iter().all(u8::is_ascii_graphic)
}

pub(super) async fn read(
    provider: &str,
    cancel: &CancelToken,
) -> Result<SecretMaterial, AuthError> {
    let key = hidden(format!("API key for {provider} (input hidden): "), cancel).await?;
    if !valid(key.expose_secret_bytes()) {
        return Err(invalid_key());
    }
    Ok(key)
}

pub(super) async fn method(cancel: &CancelToken) -> Result<crate::cli::AuthLoginMethod, AuthError> {
    if !std::io::stdin().is_terminal() {
        return Err(AuthError::InvalidConfig(
            "login-method selection requires an interactive terminal".into(),
        ));
    }
    let cancel = cancel.clone();
    tokio::task::spawn_blocking(move || read_method(&cancel))
        .await
        .map_err(|_| AuthError::InvalidConfig("login-method selection failed".into()))?
}

fn read_method(cancel: &CancelToken) -> Result<crate::cli::AuthLoginMethod, AuthError> {
    let previously_raw = crossterm::terminal::is_raw_mode_enabled().map_err(io_error)?;
    if !previously_raw {
        crossterm::terminal::enable_raw_mode().map_err(io_error)?;
    }
    let _terminal = HiddenInput { previously_raw };
    crossterm::execute!(std::io::stderr(), crossterm::event::EnableBracketedPaste)
        .map_err(io_error)?;
    eprint!("Login method: 1) API key  2) OAuth (choose 1 or 2, then Enter): ");
    std::io::stderr().flush().map_err(io_error)?;
    let mut choice = None;
    loop {
        if cancel.is_cancelled() {
            return Err(method_cancelled());
        }
        let selected = match next_event(cancel).map_err(|error| {
            if cancel.is_cancelled() {
                method_cancelled()
            } else {
                error
            }
        })? {
            Event::Key(event) if event.kind != KeyEventKind::Release => match event.code {
                KeyCode::Enter => match choice {
                    Some('1') => return Ok(crate::cli::AuthLoginMethod::ApiKey),
                    Some('2') => return Ok(crate::cli::AuthLoginMethod::Oauth),
                    _ => continue,
                },
                KeyCode::Esc => return Err(method_cancelled()),
                KeyCode::Char('c' | 'd') if event.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Err(method_cancelled());
                }
                KeyCode::Backspace => None,
                KeyCode::Char(digit @ ('1' | '2'))
                    if !event
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    Some(digit)
                }
                KeyCode::Char(_)
                    if !event
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    None
                }
                _ => continue,
            },
            Event::Paste(text) => {
                let pasted = SecretMaterial::from(text);
                match pasted.expose_secret_bytes() {
                    b"1" => Some('1'),
                    b"2" => Some('2'),
                    _ => None,
                }
            }
            _ => continue,
        };
        if choice.is_some() {
            eprint!("\x08 \x08");
        }
        choice = selected;
        // Echo only a valid, non-secret method digit, never arbitrary input.
        if let Some(digit) = choice {
            eprint!("{digit}");
        }
        std::io::stderr().flush().map_err(io_error)?;
    }
}

fn method_cancelled() -> AuthError {
    AuthError::InvalidConfig("login-method selection cancelled; no credential saved".into())
}

async fn hidden(prompt: String, cancel: &CancelToken) -> Result<SecretMaterial, AuthError> {
    // A pipe can block indefinitely and encourages shell commands containing
    // secrets. This owner-input surface intentionally requires a real terminal.
    if !std::io::stdin().is_terminal() {
        return Err(AuthError::InvalidConfig("API-key entry requires an interactive terminal; never put a key in command arguments or a shell command".into()));
    }
    let cancel = cancel.clone();
    tokio::task::spawn_blocking(move || read_terminal(&prompt, &cancel))
        .await
        .map_err(|_| {
            AuthError::InvalidConfig("credential input failed; no credential saved".into())
        })?
}

fn next_event(cancel: &CancelToken) -> Result<Event, AuthError> {
    loop {
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
        if crossterm::event::poll(std::time::Duration::from_millis(100)).map_err(io_error)? {
            return crossterm::event::read().map_err(io_error);
        }
    }
}

fn read_terminal(prompt: &str, cancel: &CancelToken) -> Result<SecretMaterial, AuthError> {
    // Set no-echo before the prompt; a caller waiting for it can paste safely.
    let previously_raw = crossterm::terminal::is_raw_mode_enabled().map_err(io_error)?;
    if !previously_raw {
        crossterm::terminal::enable_raw_mode().map_err(io_error)?;
    }
    let _terminal = HiddenInput { previously_raw };
    crossterm::execute!(std::io::stderr(), crossterm::event::EnableBracketedPaste)
        .map_err(io_error)?;
    eprint!("{prompt}");
    std::io::stderr().flush().map_err(io_error)?;

    // Never grow a secret-bearing allocation: old reallocations cannot be wiped.
    let mut key = SecretMaterial::new(Vec::with_capacity(MAX_KEY_BYTES));
    let mut invalid = false;
    loop {
        match next_event(cancel)? {
            Event::Key(event) if event.kind != KeyEventKind::Release => match event.code {
                KeyCode::Enter => return if invalid { Err(invalid_key()) } else { Ok(key) },
                KeyCode::Esc => return Err(cancelled()),
                KeyCode::Char('c' | 'd') if event.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Err(cancelled());
                }
                KeyCode::Backspace => {
                    let bytes = key.expose_secret_bytes();
                    let mut replacement = SecretMaterial::new(Vec::with_capacity(MAX_KEY_BYTES));
                    replacement.extend_from_slice(&bytes[..bytes.len().saturating_sub(1)]);
                    key = replacement;
                }
                KeyCode::Char(character)
                    if !event
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    if !character.is_ascii_graphic()
                        || key.expose_secret_bytes().len() == MAX_KEY_BYTES
                    {
                        invalid = true;
                    }
                    if !invalid {
                        key.extend_from_slice(&[character as u8]);
                    }
                }
                _ => {}
            },
            Event::Paste(text) => {
                let pasted = SecretMaterial::from(text);
                let bytes = pasted.expose_secret_bytes();
                let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
                let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
                if key.expose_secret_bytes().len() + bytes.len() > MAX_KEY_BYTES
                    || bytes.iter().any(|byte| !byte.is_ascii_graphic())
                {
                    invalid = true;
                }
                if !invalid {
                    key.extend_from_slice(bytes);
                }
            }
            _ => {}
        }
    }
}

fn cancelled() -> AuthError {
    AuthError::InvalidConfig("API key entry cancelled; no credential saved".into())
}

struct HiddenInput {
    previously_raw: bool,
}

impl Drop for HiddenInput {
    fn drop(&mut self) {
        // Before restoring echo, discard the immediately queued tail of typed
        // or unbracketed pasted input on every exit (including cancellation).
        // Never consume input owned by a host that already had raw mode enabled.
        if !self.previously_raw {
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
            while let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) {
                let quiet = remaining.min(std::time::Duration::from_millis(75));
                if !matches!(crossterm::event::poll(quiet), Ok(true)) {
                    break;
                }
                match crossterm::event::read() {
                    Ok(Event::Paste(text)) => drop(SecretMaterial::from(text)),
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        }
        let _ = crossterm::execute!(std::io::stderr(), crossterm::event::DisableBracketedPaste);
        if !self.previously_raw {
            let _ = crossterm::terminal::disable_raw_mode();
        }
        eprintln!();
    }
}
