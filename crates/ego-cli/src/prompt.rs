//! Shared prompt precedence and `@file` handling. Agent files and argv prompts are mutually
//! exclusive. `-` selects stdin; argv plus piped stdin appends a delimited context block so
//! piped data cannot impersonate user prompt text. Attachments remain separate content blocks.

use std::io::{IsTerminal, Read};
use std::path::Path;

use crate::cli::{Stdin, UsageError};

/// The fence around piped input.
///
/// Fixed rather than random. A random delimiter defends against text that
/// contains the fence, which is a real attack on a *model*, but it also makes
/// the request unreproducible and the difference invisible in a diff. Fenced
/// and named is what a reader of the log needs; the injection defence that
/// actually holds is that this arrives as `Role::User` content and grants
/// nothing.
const FENCE: &str = "---";

/// Whatever is on stdin, as text.
pub trait StdinSource {
    /// Whether stdin is a terminal. A terminal means nobody piped anything,
    /// so nothing is waiting to be read and reading would block forever.
    fn is_terminal(&self) -> bool;
    fn read(&self) -> std::io::Result<String>;
}

/// The process's own stdin.
pub struct ProcessStdin;

impl StdinSource for ProcessStdin {
    fn is_terminal(&self) -> bool {
        std::io::stdin().is_terminal()
    }

    fn read(&self) -> std::io::Result<String> {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        Ok(text)
    }
}

/// Assemble the prompt this run was given.
///
/// The precedence, in order, and it is total — every combination lands
/// somewhere and nothing is silently dropped:
///
/// | argv | `-` | piped stdin | prompt |
/// | --- | --- | --- | --- |
/// | — | yes | — | stdin |
/// | text | — | no | the argument |
/// | text | — | yes | the argument, then stdin as a fenced block |
/// | — | — | yes | stdin |
/// | — | — | no | refused |
pub fn assemble(
    argv: Option<&str>,
    mode: Stdin,
    stdin: &dyn StdinSource,
) -> Result<String, UsageError> {
    let piped = || -> Result<String, UsageError> {
        stdin
            .read()
            .map_err(|err| UsageError(format!("the prompt could not be read from stdin: {err}")))
    };

    if mode == Stdin::IsThePrompt {
        let text = piped()?;
        return non_empty(text.trim(), "`-` was given and stdin was empty");
    }

    match (argv, stdin.is_terminal()) {
        (Some(argv), true) => non_empty(argv.trim(), "the prompt was empty"),
        (Some(argv), false) => {
            let context = piped()?;
            if context.trim().is_empty() {
                return non_empty(argv.trim(), "the prompt was empty");
            }
            Ok(format!(
                "{}\n\n{FENCE} piped input {FENCE}\n{}\n{FENCE}",
                argv.trim(),
                context.trim_end()
            ))
        }
        (None, false) => {
            let text = piped()?;
            non_empty(text.trim(), "stdin was empty and no prompt was given")
        }
        (None, true) => Err(UsageError(
            "no prompt was supplied; pass your prompt as an argument".to_string(),
        )),
    }
}

/// Assemble an agent-file body with the same stdin context rule as argv.
///
/// Unlike an argv prompt, the versioned file body is an exact artifact: it is
/// not trimmed or otherwise normalized after its declared substitutions.
pub(crate) fn assemble_agent_file(
    body: &str,
    mode: Stdin,
    stdin: &dyn StdinSource,
) -> Result<String, UsageError> {
    if mode == Stdin::IsThePrompt {
        return Err(UsageError(
            "an agent file cannot be combined with '-'; remove '-' to use the file".to_string(),
        ));
    }
    if body.is_empty() {
        return Err(UsageError(
            "the agent-file prompt body is empty: there is nothing to ask".to_string(),
        ));
    }
    if stdin.is_terminal() {
        return Ok(body.to_string());
    }
    let context = stdin
        .read()
        .map_err(|err| UsageError(format!("the prompt could not be read from stdin: {err}")))?;
    if context.trim().is_empty() {
        return Ok(body.to_string());
    }
    Ok(format!(
        "{body}\n\n{FENCE} piped input {FENCE}\n{}\n{FENCE}",
        context.trim_end()
    ))
}

/// The prompt an interactive session *starts* with, which it is allowed not to
/// have.
///
/// [`assemble`]'s last row refuses when there is no argument and stdin is a
/// terminal, and that refusal is right for a one-shot: a headless run with
/// nothing to ask has nothing to do. An interactive session with nothing to
/// ask has something to do — it asks the person. A closed, empty non-terminal
/// stdin is the same absence (as in a test harness or redirected `/dev/null`),
/// while non-empty piped input remains the seed. Explicit `-` is unchanged: it
/// promises stdin contains a prompt, so an empty stream is refused.
pub fn seed(
    argv: Option<&str>,
    mode: Stdin,
    stdin: &dyn StdinSource,
) -> Result<String, UsageError> {
    if mode != Stdin::IsThePrompt && argv.is_none() {
        if stdin.is_terminal() {
            return Ok(String::new());
        }
        return stdin
            .read()
            .map(|text| text.trim().to_string())
            .map_err(|err| UsageError(format!("the prompt could not be read from stdin: {err}")));
    }
    assemble(argv, mode, stdin)
}

/// The interactive twin of [`assemble_agent_file`].
///
/// A file always supplies an instruction, so the promptless interactive
/// exception does not apply.
pub(crate) fn seed_agent_file(
    body: &str,
    mode: Stdin,
    stdin: &dyn StdinSource,
) -> Result<String, UsageError> {
    assemble_agent_file(body, mode, stdin)
}

fn non_empty(text: &str, why: &str) -> Result<String, UsageError> {
    if text.is_empty() {
        Err(UsageError(format!("{why}: there is nothing to ask")))
    } else {
        Ok(text.to_string())
    }
}

/// Resolve inline text or `@file` content for system and schema inputs.
///
/// A missing file is a usage error and not a warning: a run whose system
/// prompt silently became the literal text `@missing.md` has been misled about
/// what it asked for.
pub fn at_file(value: &str) -> Result<String, UsageError> {
    let Some(path) = value.strip_prefix('@') else {
        return Ok(value.to_string());
    };
    std::fs::read_to_string(Path::new(path))
        .map_err(|err| UsageError(format!("{path} could not be read: {err}")))
}
