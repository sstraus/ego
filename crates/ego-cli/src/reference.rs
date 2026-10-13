//! The two artifacts that are written down from the parser, not beside it.
//!
//! `CLI.md` §4 requires the reference and the shell completions to come from
//! "the same final clap definitions", and says why: a hand-maintained command
//! list is not release evidence. It is a second description of the grammar,
//! and a second description is a thing that can quietly stop being true while
//! every test around it stays green.
//!
//! So both artifacts are rendered from [`crate::cli::root_command`], the file
//! on disk is the snapshot, and a test regenerates and compares. A flag added
//! to a subcommand struct and not regenerated fails that test — which is the
//! only mechanism that has ever kept a reference honest.

use std::fmt::Write as _;

use clap::Command;

/// Where the rendered artifacts live, relative to the repository root.
///
/// Named here rather than in the test so the generator and its snapshot cannot
/// be pointed at different files.
pub const REFERENCE_PATH: &str = "docs/reference/cli.md";

/// The completion scripts this build supports, and the file each is written to.
///
/// Five shells because those are the five `clap_complete` generates without a
/// third-party crate, and a shell whose script ego cannot regenerate on demand
/// is one whose completions go stale silently.
pub const COMPLETIONS: &[(clap_complete::Shell, &str)] = &[
    (clap_complete::Shell::Bash, "completions/ego.bash"),
    (clap_complete::Shell::Zsh, "completions/_ego"),
    (clap_complete::Shell::Fish, "completions/ego.fish"),
    (clap_complete::Shell::PowerShell, "completions/_ego.ps1"),
    (clap_complete::Shell::Elvish, "completions/ego.elv"),
];

/// The completion script for one shell.
pub fn completion(shell: clap_complete::Shell) -> String {
    let mut command = crate::cli::root_command();
    let mut rendered = Vec::new();
    clap_complete::generate(shell, &mut command, "ego", &mut rendered);
    // `clap_complete` writes UTF-8 and nothing else; a panic here would mean
    // clap itself broke, which is not a case worth a `Result` in the signature.
    String::from_utf8(rendered).expect("clap_complete writes UTF-8")
}

/// The command reference, one section per subcommand.
///
/// Deliberately not a man page and not `--help` dumped to a file. It is the
/// flat list of every command and every flag, which is what someone checks a
/// reference against, and what a diff of it shows when the grammar moves.
pub fn reference() -> String {
    let mut root = crate::cli::root_command();
    // Without this, clap has not yet resolved the derived value counts, and
    // every flag would be listed as if it took none.
    root.build();
    let mut out = String::new();
    out.push_str("# ego command reference\n\n");
    out.push_str(
        "Generated from the clap definitions in `crates/ego-cli/src/cli.rs`.\n\
         Do not edit: `crates/ego-cli/tests/reference.rs` regenerates and compares it.\n\n\
         `ego PROMPT` with no subcommand starts an interactive session and is not\n\
         listed below, because it is the absence of a subcommand rather than one.\n\n",
    );
    for command in root.get_subcommands() {
        section(&mut out, command);
    }
    out
}

fn section(out: &mut String, command: &Command) {
    let _ = writeln!(out, "## `ego {}`\n", command.get_name());
    if let Some(about) = command.get_about() {
        let _ = writeln!(out, "{about}\n");
    }
    for sub in command.get_subcommands() {
        let _ = writeln!(
            out,
            "- `{} {}` — {}",
            command.get_name(),
            sub.get_name(),
            sub.get_about().map_or(String::new(), |a| a.to_string())
        );
    }
    if command.get_subcommands().next().is_some() {
        out.push('\n');
    }
    let mut flags: Vec<(bool, String, String)> = command
        .get_arguments()
        .filter(|arg| !arg.is_hide_set())
        .map(|arg| {
            (
                // Positionals last: they are read after the flags that qualify
                // them, which is also the order the usage line puts them in.
                arg.get_long().is_none() && arg.get_short().is_none(),
                arg.get_id().as_str().to_owned(),
                spelling(arg),
            )
        })
        .collect();
    // Sorted by name, because clap's declaration order is a property of the
    // struct and this file is read by someone looking one flag up.
    flags.sort();
    for (_, _, flag) in flags {
        let _ = writeln!(out, "{flag}");
    }
    out.push('\n');
    if matches!(command.get_name(), "resume" | "continue") {
        out.push_str(
            "`ego continue` shares the saved-session admission and options of `ego resume`.\n\
             In the terminal use `/resume [ID]` or `/continue`. Current execution authority\n\
             is measured before effects: narrower authority is automatic; potentially\n\
             wider authority requires explicit human consent. Headless callers cannot\n\
             approve wider authority.\n\n\
             With terminal stdin and stdout, `ego resume ID \"next task\"` opens the\n\
             interactive seat, submits the task once, and accepts further messages.\n\
             Explicit `--output` (including `text`), stdin `-`, `--answer`, an agent file,\n\
             or profile `ci` selects the headless driver. Redirected stdin or stdout\n\
             also selects headless execution. `EGO_PROFILE=ci` has the same effect.\n\n\
             Headless execution never asks for permission: an unresolved approval exits 3.\n\
             Pre-approve only the required operations in trusted CI profile configuration:\n\n\
             ```toml\n[profile.ci]\nmodel = \"ollama/qwen3\"\n\
             session_dir = \"/absolute/ci-sessions\"\nartifact_dir = \"/absolute/ci-artifacts\"\n\n\
             [profile.ci.permissions]\nallow = [\"exec(seq 1 3)\"]\n```\n\n\
             Pre-create both roots outside the workspace and outside each other.\n\
             Supply a non-interactive provider credential when the model requires one.\n\
             Then run `ego resume ID \"next task\" --profile ci --instructions ci.md`.\n\
             Allow rules grant policy approval; sandbox and system ceilings still apply.\n\n",
        );
    }
    if command.get_name() == "acp" {
        out.push_str(
            "Prompt flags use the same composition as `ego run`. For example:\n\n\
             ```sh\nego acp --system @agent-prompt.txt --append-system \"Use concise answers\" --bare\n```\n\n\
             `--bare` and `--instructions` conflict. ACP hosts can also pass per-session\n\
             `_meta.ego.systemPrompt` (`mode` and `text`) and `_meta.ego.bare` in `session/new`.\n\
             See [session initialization prompt](../integrations/acp.md#session-initialization-prompt)\n\
             for the wire example and launch-default precedence.\n\n",
        );
    }
}

/// One argument as the reference spells it: how it is typed, then what it does.
fn spelling(arg: &clap::Arg) -> String {
    let mut spelling = match (arg.get_short(), arg.get_long()) {
        (Some(short), Some(long)) => format!("-{short}, --{long}"),
        (Some(short), None) => format!("-{short}"),
        (None, Some(long)) => format!("--{long}"),
        // A positional has neither, and its id is the name the usage line uses.
        (None, None) => return positional(arg),
    };
    if arg.get_num_args().is_some_and(|args| args.takes_values()) {
        let _ = write!(spelling, " <{}>", value_name(arg));
    }
    format!("- `{spelling}` — {}", help(arg))
}

fn positional(arg: &clap::Arg) -> String {
    format!("- `<{}>` — {}", value_name(arg), help(arg))
}

fn value_name(arg: &clap::Arg) -> String {
    arg.get_value_names()
        .and_then(<[clap::builder::Str]>::first)
        .map_or_else(
            || arg.get_id().as_str().to_uppercase(),
            std::string::ToString::to_string,
        )
}

/// The help clap would print, or a named absence.
///
/// Not an empty string: a reference line ending in an em dash and nothing is
/// indistinguishable from a rendering bug, and this way the missing doc
/// comment is the thing a reader reports.
fn help(arg: &clap::Arg) -> String {
    arg.get_help()
        .map_or_else(|| "(undocumented)".to_owned(), |help| help.to_string())
}
