//! The input-precedence algorithm of `CLI.md` §4.5, and the `@file`
//! convention `--system` and `--schema` share.
//!
//! §2's stdin contract, which the document takes from Codex as "the cleanest
//! surveyed": the prompt is an argument; `-` reads it from stdin; and with
//! *both* an argv prompt and piped stdin, stdin is appended as a delimited
//! context block. The delimiter matters — text a pipe supplied and text a
//! person typed are different things, and running them together lets the
//! first impersonate the second.

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
        stdin.read().map_err(|err| {
            UsageError(format!("the prompt could not be read from stdin: {err}"))
        })
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
            "no prompt: pass one as an argument, pipe one in, or use `-` to \
             read it from stdin"
                .to_string(),
        )),
    }
}

fn non_empty(text: &str, why: &str) -> Result<String, UsageError> {
    if text.is_empty() {
        Err(UsageError(format!("{why}: there is nothing to ask")))
    } else {
        Ok(text.to_string())
    }
}

/// Resolve a value that may be `@file`, the convention `CLI.md` §3 gives
/// `--system`, `--append-system` and `--schema`.
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
