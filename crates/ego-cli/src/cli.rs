//! The argument grammar: `CLI.md` §3 and the parse half of §4.5.
//!
//! Three things happen here, in this order, and the order is the contract:
//!
//! 1. **`--` is honoured first.** §4.5 reserves it for prompt
//!    disambiguation — `ego -- run` prompts the word "run" — so nothing
//!    downstream may look at a token past it. `clap`'s own `--` handling is
//!    per-subcommand and would have to decide `run` is a subcommand *before*
//!    it could apply, which is the wrong way round.
//! 2. **The applicability matrix.** A flag `CLI.md` documents whose subsystem
//!    this build does not have is refused **by name**, naming what it waits
//!    on, and a retired flag is refused by name with its replacement (§4.3:
//!    "flags are never removed silently"). Both run before `clap` sees the
//!    vector, so the answer is a sentence about *that flag* rather than
//!    clap's "unexpected argument".
//! 3. **`clap`, over what is left.** `docs/04-crates.md` names clap for
//!    argv, and generating the reference and the completions from the same
//!    definition (§4.5's last clause) is only possible while there is exactly
//!    one definition.

use std::path::PathBuf;
use std::str::FromStr;

use clap::{ArgAction, Args as ClapArgs, Parser};
use ego_core::event::id::SessionId;
use ego_core::generator::types::ThinkingLevel;

/// What the process was asked to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Invocation {
    /// `ego run [PROMPT]` — the headless one-shot of `CLI.md` §2.
    Run(Box<Args>),
    /// `ego resume [ID] [PROMPT]` — the same run against a session that
    /// already exists. One [`Args`] and not a second shape: everything after
    /// "which session" is identical, and a `ResumeArgs` would be the parallel
    /// implementation `AGENTS.md`'s reuse rule forbids.
    Resume(Box<Args>),
    /// `ego ls` — the catalogue, this workspace's by default.
    Ls(Box<LsArgs>),
    /// `ego acp` — serve the Agent Client Protocol on stdio, until the host
    /// closes it.
    Acp(Box<AcpArgs>),
}

/// A refusal decided before any turn exists.
///
/// One type and not several, because every one of them is `CLI.md` §4.3's
/// exit 2 and the caller turns it into `RunTermination::UsageError` at a
/// single place. The message is the whole product: a script that gets 2 and
/// no sentence has been told nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageError(pub String);

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UsageError {}

/// A flag `CLI.md` §3 documents that this build cannot honour, and the
/// subsystem it is waiting on.
///
/// **Not a convenience.** Each entry names a component that does not exist —
/// not a switch someone has not got round to reading. A `--allow` that parsed
/// a rule and enforced nothing would be worse than this refusal by exactly the
/// margin that matters: it would look like a permission boundary.
struct Unavailable {
    flags: &'static [&'static str],
    waiting_on: &'static str,
}

/// The applicability matrix of `CLI.md` §4.5.
///
/// `ARCHITECTURE.md`'s milestone table (line 424 onwards) is the source for
/// every `waiting_on` here: these are subsystems, and the sentence says which.
const UNAVAILABLE: &[Unavailable] = &[
    Unavailable {
        flags: &["--mode", "--plan", "--yolo", "--allow", "--deny"],
        waiting_on: "the permission rule grammar — a canonical call AST, \
                     stable rule IDs and an approver that can mint a Permit. \
                     `PolicyEngine` today admits a read inside the workspace \
                     and nothing else, so a rule would parse and enforce \
                     nothing",
    },
    Unavailable {
        flags: &["--sandbox"],
        waiting_on: "a sandbox backend. No process is spawned anywhere in \
                     this build, so there is nothing yet to confine",
    },
    Unavailable {
        flags: &["-f", "--file", "--var"],
        waiting_on: "executable agent files (`CLI.md` §5): YAML frontmatter, \
                     `{{placeholder}}` variables and a versioned format",
    },
    Unavailable {
        flags: &["--attach"],
        waiting_on: "a non-text content block. The provider-neutral \
                     conversation model carries text, tool calls and tool \
                     results, so an image has no representation to be sent in",
    },
    Unavailable {
        flags: &["--mcp"],
        waiting_on: "the MCP client",
    },
    Unavailable {
        flags: &["--retry", "--fallback"],
        waiting_on: "a retry seam. A generation owns a reservation for its \
                     whole life, so re-issuing one is a change to the attempt \
                     contract rather than a loop that runs twice",
    },
];

/// The commands of `CLI.md` §2 this build implements.
///
/// The dispatch below reads this rather than matching three literals, so the
/// list is a value a test can hold — and the test it exists for is the one
/// that says [`BUILT`] and [`UNBUILT`] are disjoint. Without it the two tables
/// were only bound to the *document*: a command could be implemented and still
/// listed as waiting on a subsystem, and since the `UNBUILT` check runs first
/// the refusal would win, so a working command would answer that it does not
/// exist. That is `every_waiting_on_reason_is_the_one_the_document_prints`'s
/// defect moved one link along the chain — two surfaces agreeing perfectly
/// about something that has stopped being true.
pub const BUILT: &[&str] = &["run", "resume", "ls", "acp"];

/// A command `CLI.md` §2 documents that this build does not implement, and the
/// subsystem it is waiting on.
///
/// The subcommand twin of [`UNAVAILABLE`], and it exists for the same reason.
/// §2 offers fourteen commands and `run` is the only one here, so the other
/// thirteen used to fall through to the *interactive* refusal below — which
/// told someone who typed `ego resume` that the permission dialog was missing.
/// True, and about a surface they had not asked for. `CLI.md` §3.1 states the
/// standard for flags ("refused **by name**, with the subsystem it waits on")
/// and nothing had applied it one level up.
///
/// A tuple rather than a struct, matching [`RETIRED`], because both halves are
/// needed by the test that binds this table to the document.
///
/// `waiting_on` is sourced from `ARCHITECTURE.md` §8's milestone table where it
/// places the work, and from the code where it does not — a subsystem that
/// table does not place still gets a reason, never a milestone invented for it.
pub const UNBUILT: &[(&str, &str)] = &[
    (
        "fork",
        "session forking (**M3**), which needs the branch point to be nameable \
         in the log rather than just a file copy",
    ),
    (
        "export",
        "replay and export with `--sanitize` (**M3**). The projection exists; \
         the redaction contract that makes an exported session safe to hand \
         over does not",
    ),
    (
        "mcp",
        "the MCP client (**M2**). There is no server to add, remove or log in to",
    ),
    (
        "mcp-server",
        "ego's own MCP server surface (**M2**): the scoped run/continue/cancel/\
         status tools it would expose",
    ),
    (
        "auth",
        "a credential store. Every provider reads one environment variable \
         (`Auth::bearer_from_env`), so there is nowhere for a login to put \
         anything and nothing for a logout to remove",
    ),
    (
        "models",
        "a catalogue to enumerate. Hosted facts are supplied and local ones are \
         discovered per resolution \
         (`docs/decisions/2026-08-21-what-a-model-resolves-to.md`), so this \
         build resolves one model at a time and holds no list to print or \
         refresh; generated catalogues are **M5**",
    ),
    (
        "config",
        "a config writer. The loader is strict and read-only, and nothing in \
         this build writes a config file — `-c/--set` overrides one run",
    ),
    (
        "rules",
        "the permission rule grammar (**M1**), the same subsystem `--allow` and \
         `--deny` wait on: stable rule IDs are what `rules ls` would list and \
         `rules revoke` would name",
    ),
    (
        "usage",
        "a report across sessions, which needs both the workspace attribution \
         `ls` waits on and the per-session cost line budgets add in **M3**",
    ),
    (
        "doctor",
        "most of what it would diagnose: the credential store, the MCP client \
         and the sandbox backend are each waiting on their own milestone, and a \
         report on three absent subsystems is not a diagnosis",
    ),
];

/// A flag that was removed, and what replaced it.
///
/// Empty, because nothing has been retired yet. It ships anyway, with its own
/// test: `CLI.md` §4.3 says a retired flag errors by name with its
/// replacement, and the moment that rule is easiest to forget is the first
/// retirement — when there is no mechanism, only a note in a changelog.
pub const RETIRED: &[(&str, &str)] = &[];

/// The flag name in `token`, if it is one. `--budget=2.5$` is `--budget`.
fn flag_name(token: &str) -> Option<&str> {
    if !token.starts_with('-') || token == "-" || token == "--" {
        return None;
    }
    Some(match token.split_once('=') {
        Some((name, _)) => name,
        None => token,
    })
}

/// Refuse a documented-but-unavailable or retired flag, by name.
fn check_matrix(tokens: &[String]) -> Result<(), UsageError> {
    for token in tokens {
        let Some(name) = flag_name(token) else {
            continue;
        };
        if let Some((_, replacement)) = RETIRED.iter().find(|(retired, _)| *retired == name) {
            return Err(UsageError(format!(
                "{name} has been retired; use {replacement}"
            )));
        }
        if let Some(entry) = UNAVAILABLE.iter().find(|entry| entry.flags.contains(&name)) {
            return Err(UsageError(format!(
                "{name} is documented in CLI.md §3 and is not available in this \
                 build: it is waiting on {}",
                entry.waiting_on
            )));
        }
    }
    Ok(())
}

/// Where the prompt comes from when it is not on the command line.
///
/// `CLI.md` §2's stdin contract, which is Codex's: `-` reads the prompt from
/// stdin, and with *both* an argv prompt and piped stdin, stdin is appended
/// as a delimited context block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stdin {
    /// `-` was given: stdin *is* the prompt.
    IsThePrompt,
    /// Nothing said otherwise; whether stdin is piped is decided at run time.
    Ambient,
}

/// How the run reports itself (`CLI.md` §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum Output {
    Text,
    Json,
    Jsonl,
}

/// Where a session is written, or that it is not (`CLI.md` §3, "Session").
///
/// One value rather than three independent flags, because §4.5 calls them a
/// mutually-exclusive group and a group is a thing the type can *be*, not a
/// rule someone has to remember to check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionChoice {
    /// A fresh session, written under the store root.
    New,
    /// A named session: created if it is not there, attached if it is.
    Named(String),
    /// An id the caller supplied, so there is no capture-then-resume dance.
    Id(String),
    /// Nothing is written.
    Ephemeral,
    /// `ego resume [ID]`: a session that **already exists** — the one named,
    /// or the most recently updated one belonging to this workspace.
    ///
    /// Not [`Id`](SessionChoice::Id), and the difference is the whole point.
    /// `--session-id` is documented as a caller-supplied id and creates the
    /// session when it is not there; routing `resume` through it would make a
    /// mistyped id succeed by silently starting an empty session, so the
    /// caller who asked to continue something would get a new thing wearing
    /// its name. See
    /// `docs/decisions/2026-08-21-the-two-commands-the-header-unblocked.md` §3.
    Resume(Option<String>),
}

/// The parsed command line of `ego ls`.
///
/// Four fields and not [`Args`]: `ls` starts no turn, so a model, a budget and
/// a schema have nothing to act on. Three of the four come from the same
/// [`Common`] block `run` and `resume` use, so the spelling of `--session-dir`,
/// `-C` and `--output` cannot drift between the command that writes a session
/// and the command that lists them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LsArgs {
    /// Every session in the store, not just this workspace's (`CLI.md` §2).
    pub all: bool,
    pub session_dir: Option<PathBuf>,
    pub directory: Option<PathBuf>,
    pub output: Output,
}

/// The parsed command line of `ego acp`.
///
/// Two fields, and the absent third is the point: `ego acp`'s stdout **is** the
/// JSON-RPC stream, so there is no `--output` to choose. See [`Places`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpArgs {
    pub session_dir: Option<PathBuf>,
    pub directory: Option<PathBuf>,
}

/// The parsed command line of `ego run`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub prompt: Option<String>,
    pub stdin: Stdin,
    pub model: Option<String>,
    pub offline: bool,
    pub thinking: ThinkingLevel,
    pub system: Option<String>,
    pub append_system: Option<String>,
    pub instructions: Option<PathBuf>,
    pub bare: bool,
    pub session: SessionChoice,
    pub session_dir: Option<PathBuf>,
    pub output: Output,
    pub schema: Option<String>,
    pub schema_retries: u32,
    pub budget: Option<String>,
    pub max_turns: Option<u32>,
    pub timeout: Option<String>,
    pub tools: Option<Vec<String>>,
    pub directory: Option<PathBuf>,
    pub set: Vec<String>,
    pub profile: Option<String>,
    pub quiet: bool,
    pub verbose: bool,
    pub no_color: bool,
}

/// The clap definition. Kept separate from [`Args`] so that the shape the
/// rest of the binary reads is the shape §4.5 describes — a mutually
/// exclusive group is one field — rather than clap's flat set of options.
#[derive(Debug, Parser)]
#[command(
    name = "ego",
    about = "Run an agent from a prompt, headless.",
    disable_help_subcommand = true
)]
struct RunCommand {
    /// The prompt. `-` reads it from stdin.
    prompt: Option<String>,

    #[command(flatten)]
    flags: Flags,
}

/// `ego resume [ID] [PROMPT]` — the same [`Flags`], against a session that
/// already exists.
#[derive(Debug, Parser)]
#[command(
    name = "ego resume",
    about = "Continue a session; no ID = latest in this workspace.",
    disable_help_subcommand = true
)]
struct ResumeCommand {
    /// The session to continue. Omit it for the latest in this workspace.
    id: Option<String>,

    /// The prompt. `-` reads it from stdin.
    prompt: Option<String>,

    #[command(flatten)]
    flags: Flags,
}

/// `ego ls` — the catalogue.
#[derive(Debug, Parser)]
#[command(
    name = "ego ls",
    about = "List sessions (this workspace; --all for every one).",
    disable_help_subcommand = true
)]
struct LsCommand {
    /// Every session in the store, not just this workspace's.
    #[arg(long)]
    all: bool,

    #[command(flatten)]
    common: Common,
}

/// `ego acp` — the protocol, on stdio.
#[derive(Debug, Parser)]
#[command(
    name = "ego acp",
    about = "Serve the Agent Client Protocol on stdio.",
    disable_help_subcommand = true
)]
struct AcpCommand {
    #[command(flatten)]
    places: Places,
}

/// The three flags every subcommand takes, declared once.
///
/// `ls` starts no turn, so it shares almost nothing with `run` — but these
/// three it does share, and they are the three where a divergence would be
/// invisible: a `--session-dir` that `run` honoured and `ls` did not would
/// list the wrong store, and a `-C` that only one of them applied would list
/// the wrong workspace. Declaring them once is what makes "the same flag"
/// true by construction rather than by two authors agreeing.
#[derive(Debug, ClapArgs)]
struct Common {
    #[command(flatten)]
    places: Places,

    /// text|json|jsonl
    #[arg(long, value_enum, default_value_t = Output::Text)]
    output: Output,
}

/// Which store and which workspace — the two `acp` shares with the rest.
///
/// Split out of [`Common`] rather than flattened wholesale, because `--output`
/// is the one flag `ego acp` must not have: its stdout *is* the JSON-RPC
/// stream, and a second writer on that descriptor does not produce a run with
/// noisy output, it produces a protocol the host cannot parse. A flag that
/// would corrupt the transport is not a flag with a bad default — it is one
/// the command does not take.
#[derive(Debug, ClapArgs)]
struct Places {
    /// Override the store location.
    #[arg(long)]
    session_dir: Option<PathBuf>,

    /// Run as if started in DIR.
    #[arg(short = 'C', value_name = "DIR")]
    directory: Option<PathBuf>,
}

#[derive(Debug, ClapArgs)]
struct Flags {
    /// provider/model slug, e.g. ollama/qwen3
    #[arg(short = 'm', long)]
    model: Option<String>,

    /// No non-loopback egress from ego's own HTTP transport.
    #[arg(long)]
    offline: bool,

    /// off|minimal|low|medium|high|xhigh|max
    #[arg(long, value_parser = parse_thinking, default_value = "off")]
    thinking: ThinkingLevel,

    /// Replace the system prompt. `@file` reads it from a file.
    #[arg(long)]
    system: Option<String>,

    /// Append to the system prompt. `@file` reads it from a file.
    #[arg(long)]
    append_system: Option<String>,

    /// Use this instruction file instead of discovering one.
    #[arg(long)]
    instructions: Option<PathBuf>,

    /// No ambient discovery, even in a trusted workspace.
    #[arg(long)]
    bare: bool,

    /// Named session: created or attached.
    #[arg(long, group = "session-choice")]
    session: Option<String>,

    /// A session id supplied by the caller.
    #[arg(long, group = "session-choice")]
    session_id: Option<String>,

    /// Write no session at all.
    #[arg(long, group = "session-choice")]
    ephemeral: bool,

    #[command(flatten)]
    common: Common,

    /// JSON Schema the final answer must validate against; `@file` or inline.
    #[arg(long)]
    schema: Option<String>,

    /// How many times to re-prompt on a schema mismatch.
    #[arg(long, default_value_t = 2)]
    schema_retries: u32,

    /// Hard cap: '2.5$' or '400k' tokens.
    #[arg(long)]
    budget: Option<String>,

    /// Cap the turns this run may take.
    #[arg(long)]
    max_turns: Option<u32>,

    /// Wall clock, e.g. '90s' or '10m'.
    #[arg(long)]
    timeout: Option<String>,

    /// Restrict the builtin tools to this comma-separated list.
    #[arg(long, value_delimiter = ',')]
    tools: Option<Vec<String>>,

    /// Offer no tools at all.
    #[arg(long, conflicts_with = "tools")]
    no_tools: bool,

    /// Dotted-path config override, value parsed as TOML.
    #[arg(short = 'c', long = "set", value_name = "K=V", action = ArgAction::Append)]
    set: Vec<String>,

    /// Layer [profile.NAME] from config.
    #[arg(long)]
    profile: Option<String>,

    #[arg(short = 'q', long)]
    quiet: bool,

    #[arg(short = 'v', long)]
    verbose: bool,

    #[arg(long)]
    no_color: bool,
}

fn parse_thinking(value: &str) -> Result<ThinkingLevel, String> {
    match value {
        "off" => Ok(ThinkingLevel::Off),
        "minimal" => Ok(ThinkingLevel::Minimal),
        "low" => Ok(ThinkingLevel::Low),
        "medium" => Ok(ThinkingLevel::Medium),
        "high" => Ok(ThinkingLevel::High),
        "xhigh" => Ok(ThinkingLevel::XHigh),
        "max" => Ok(ThinkingLevel::Max),
        other => Err(format!(
            "`{other}` is not a thinking level: off|minimal|low|medium|high|xhigh|max"
        )),
    }
}

/// Split `argv` at the first bare `--`.
///
/// Everything after it is prompt words. `ego -- run` is therefore the
/// *interactive* form with the prompt "run", never the `run` subcommand, and
/// `ego run -- --model` prompts the two words after the separator.
fn split_at_separator(argv: Vec<String>) -> (Vec<String>, Vec<String>) {
    match argv.iter().position(|token| token == "--") {
        Some(at) => {
            let mut head = argv;
            let tail = head.split_off(at + 1);
            head.pop();
            (head, tail)
        }
        None => (argv, Vec::new()),
    }
}

/// Parse a full argument vector, `argv[0]` included.
pub fn parse(argv: Vec<String>) -> Result<Invocation, UsageError> {
    let mut argv = argv;
    let program = if argv.is_empty() {
        "ego".to_string()
    } else {
        argv.remove(0)
    };
    let (head, tail) = split_at_separator(argv);

    // Before clap, and before the subcommand is decided: a refusal must be
    // able to name a flag clap would have rejected as unknown.
    check_matrix(&head)?;

    // A documented command answers for itself, before the fallback below can
    // answer for it. Only an exact first token counts: §2's first line is
    // `ego [PROMPT]`, so a word that is not on this list is a prompt, and
    // refusing every unrecognised token would turn a prompt into a typo.
    if let Some(first) = head.first()
        && let Some((name, waiting_on)) = UNBUILT.iter().find(|(name, _)| name == first)
    {
        return Err(UsageError(format!(
            "`{program} {name}` is documented in CLI.md §2 and is not available \
             in this build: it is waiting on {waiting_on}"
        )));
    }

    let subcommand = match head.first().map(String::as_str) {
        Some(name) if BUILT.contains(&name) => name.to_string(),
        _ => {
            return Err(UsageError(format!(
                "`{program}` with no subcommand starts an interactive session, \
                 and the interactive surface is not in this build: it is \
                 waiting on the permission dialog, which needs an approver \
                 that can mint a Permit. Use `{program} run` for a headless \
                 one-shot"
            )));
        }
    };

    let mut tokens: Vec<String> = head;
    tokens[0] = program;
    // The separator and everything after it go back on the end, so clap sees
    // the prompt words as positionals it must not interpret.
    if !tail.is_empty() {
        tokens.push("--".to_string());
        tokens.extend(tail);
    }

    // clap's own rendering, which already names the offending argument and the
    // group it violated. Re-writing it here would be a second voice saying the
    // same thing, worse.
    let clap_said = |err: clap::Error| UsageError(err.to_string().trim_end().to_string());

    match subcommand.as_str() {
        "resume" => {
            let parsed = ResumeCommand::try_parse_from(&tokens).map_err(clap_said)?;
            Ok(Invocation::Resume(Box::new(into_resume_args(parsed)?)))
        }
        "acp" => {
            let parsed = AcpCommand::try_parse_from(&tokens).map_err(clap_said)?;
            Ok(Invocation::Acp(Box::new(AcpArgs {
                session_dir: parsed.places.session_dir,
                directory: parsed.places.directory,
            })))
        }
        "ls" => {
            let parsed = LsCommand::try_parse_from(&tokens).map_err(clap_said)?;
            Ok(Invocation::Ls(Box::new(LsArgs {
                all: parsed.all,
                session_dir: parsed.common.places.session_dir,
                directory: parsed.common.places.directory,
                output: parsed.common.output,
            })))
        }
        _ => {
            let parsed = RunCommand::try_parse_from(&tokens).map_err(clap_said)?;
            Ok(Invocation::Run(Box::new(into_args(parsed)?)))
        }
    }
}

/// `ego resume [ID] [PROMPT]` becomes the same [`Args`] a run produces, with
/// [`SessionChoice::Resume`] in place of the session group.
///
/// **The first positional is an id only if it is one.** `CLI.md` §2 spells
/// both positionals optional, so `ego resume "next step"` has to mean the
/// latest session and that prompt — deciding by position alone would make it a
/// malformed-id complaint. When a *second* positional is present the first was
/// unambiguously meant as an id, so a word that is not one is refused rather
/// than silently demoted to a prompt that would then have a stray argument
/// after it.
fn into_resume_args(command: ResumeCommand) -> Result<Args, UsageError> {
    let ResumeCommand { id, prompt, flags } = command;

    for (given, flag) in [
        (flags.session.is_some(), "--session"),
        (flags.session_id.is_some(), "--session-id"),
        (flags.ephemeral, "--ephemeral"),
    ] {
        if given {
            return Err(UsageError(format!(
                "{flag} and `resume` both name a session, and they disagree: \
                 drop {flag}, or use `run` if you meant to start one"
            )));
        }
    }

    let (id, prompt) = match (id, prompt) {
        (Some(first), None) if SessionId::from_str(&first).is_err() => (None, Some(first)),
        (Some(first), None) => (Some(first), None),
        (first, second) => (first, second),
    };
    if let Some(text) = &id
        && let Err(err) = SessionId::from_str(text)
    {
        return Err(UsageError(format!(
            "`{text}` is not a session id: {err}. Pass the id of a session, or \
             omit it for the latest in this workspace"
        )));
    }

    // §2's second positional is what a resumed session is *for*. Without it
    // there is nothing to say to the session, and this build has no
    // interactive surface to hand it to — so opening one and stopping would be
    // work with no product.
    let Some(prompt) = prompt else {
        return Err(UsageError(
            "`resume` needs something to say to the session: pass a prompt, or \
             `-` to read one from stdin"
                .to_string(),
        ));
    };

    let mut args = into_args(RunCommand {
        prompt: Some(prompt),
        flags,
    })?;
    args.session = SessionChoice::Resume(id);
    Ok(args)
}

fn into_args(command: RunCommand) -> Result<Args, UsageError> {
    let RunCommand { prompt, flags } = command;

    // §4.5's applicability conflict, and the one the instructions ADR said
    // belonged here rather than in the loader: from a resolved posture alone,
    // "the user demanded bare" and "the workspace is untrusted" are the same
    // value and want opposite answers.
    if flags.bare && flags.instructions.is_some() {
        return Err(UsageError(
            "--bare and --instructions contradict each other: --bare turns \
             ambient discovery off, and --instructions names a file to use \
             instead. Drop --bare — an explicitly named file is not ambient"
                .to_string(),
        ));
    }

    let (prompt, stdin) = match prompt.as_deref() {
        Some("-") => (None, Stdin::IsThePrompt),
        _ => (prompt, Stdin::Ambient),
    };

    let session = if flags.ephemeral {
        SessionChoice::Ephemeral
    } else if let Some(name) = flags.session {
        SessionChoice::Named(name)
    } else if let Some(id) = flags.session_id {
        SessionChoice::Id(id)
    } else {
        SessionChoice::New
    };

    Ok(Args {
        prompt,
        stdin,
        model: flags.model,
        offline: flags.offline,
        thinking: flags.thinking,
        system: flags.system,
        append_system: flags.append_system,
        instructions: flags.instructions,
        bare: flags.bare,
        session,
        session_dir: flags.common.places.session_dir,
        output: flags.common.output,
        schema: flags.schema,
        schema_retries: flags.schema_retries,
        budget: flags.budget,
        max_turns: flags.max_turns,
        timeout: flags.timeout,
        // `--no-tools` is the empty list rather than a second field: "no
        // tools" and "these tools" are the same question answered, and two
        // fields would let them disagree.
        tools: if flags.no_tools {
            Some(Vec::new())
        } else {
            flags.tools
        },
        directory: flags.common.places.directory,
        set: flags.set,
        profile: flags.profile,
        quiet: flags.quiet,
        verbose: flags.verbose,
        no_color: flags.no_color,
    })
}
