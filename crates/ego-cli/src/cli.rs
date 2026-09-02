//! The argument grammar: `CLI.md` §3 and the parse half of §4.5.
//!
//! Three things happen here, in this order, and the order is the contract:
//!
//! 1. **`--` is honoured first.** §4.5 reserves it for prompt
//!    disambiguation — `ego -- run` prompts the word "run" — and the MCP
//!    grammar reserves it for the exact stdio child argv. Nothing downstream
//!    may reinterpret a token past it. `clap`'s own `--` handling is per-
//!    subcommand and would have to decide `run` is a subcommand *before* it
//!    could apply, which is the wrong way round.
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

use std::num::{NonZeroU64, NonZeroUsize};
use std::path::PathBuf;
use std::str::FromStr;

use clap::error::ErrorKind;
use clap::{ArgAction, Args as ClapArgs, Parser, Subcommand};
use ego_core::config::{
    McpEnvironmentName, McpHttpAuth, McpHttpConfig, McpHttpUrl, McpServerConfig, McpServerName,
    Mode,
};
use ego_core::event::id::{ItemId, SessionId};
use ego_core::generator::types::ThinkingLevel;
use ego_core::permit::Confinement;
use ego_core::run::RequestId;
use ego_core::usage::UsagePeriod;

use crate::agent_file::{self, AgentFile};

/// What the process was asked to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Invocation {
    /// `ego auth login|logout|status [PROVIDER]`.
    Auth(Box<AuthArgs>),
    /// `ego mcp add|rm|ls` — typed MCP configuration management.
    Mcp(Box<McpArgs>),
    /// `ego hooks ls [--json]` — the admitted system/user hook catalogue.
    Hooks(HooksArgs),
    /// `ego run [PROMPT]` — the headless one-shot of `CLI.md` §2.
    Run(Box<Args>),
    /// `ego resume [ID] [PROMPT]` — the same run against a session that
    /// already exists. One [`Args`] and not a second shape: everything after
    /// "which session" is identical, and a `ResumeArgs` would be the parallel
    /// implementation `AGENTS.md`'s reuse rule forbids.
    Resume(Box<Args>),
    /// `ego ls` — the catalogue, this workspace's by default.
    Ls(Box<LsArgs>),
    /// `ego usage` — one versioned report for this workspace.
    Usage(Box<UsageArgs>),
    /// `ego fork ID` — create a target at the source's latest durable idle boundary.
    Fork(Box<ForkArgs>),
    /// `ego compact ID --request-id UUID` — publish one deterministic successor.
    Compact(Box<CompactArgs>),
    /// `ego export ID` — render one local or structurally sanitized transcript.
    Export(Box<ExportArgs>),
    /// `ego models` — the shared model catalogue snapshot.
    Models(Box<ModelsArgs>),
    /// `ego acp` — serve the Agent Client Protocol on stdio, until the host
    /// closes it.
    Acp(Box<AcpArgs>),
    /// `ego mcp-server` — serve scoped ego runs over MCP on stdio.
    McpServer(Box<McpServerArgs>),
    /// `ego [PROMPT]`, or promptless `ego resume [ID]` — the interactive
    /// session of `CLI.md` §6.
    ///
    /// One [`Args`] again, and for [`Invocation::Resume`]'s reason: a prompt
    /// given on the command line is the first thing said, not a different
    /// mode, and everything else a session needs to resolve is the same.
    Interactive(Box<Args>),
    /// `--help` or `--version`: the question was answered, and there is
    /// nothing to run.
    ///
    /// A success and not a [`UsageError`], which is what it used to be. clap
    /// reports both through `Err`, and treating every `Err` the same sent help
    /// to *stderr* behind an `ego:` prefix and exited 2 — the code `CLI.md`
    /// §4.3 defines as "usage error". Asking what a command does is not
    /// misusing it, and a script that pipes `--help` got an empty stdout.
    Answered(String),
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
        flags: &["-v", "--verbose"],
        waiting_on: "a decision about what it shows. Every other entry here \
                     waits on a subsystem; this one waits on a meaning. \
                     `CLI.md` §4.7 says exactly what `-q` suppresses and \
                     nothing says what `-v` adds — and the obvious answer, \
                     turning up `tracing`, stops being obvious at R12: this \
                     build installs no subscriber, and adding one puts a \
                     second writer on the stream the marks and the \
                     notification already own",
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
pub const BUILT: &[&str] = &[
    "run",
    "resume",
    "fork",
    "compact",
    "export",
    "ls",
    "usage",
    "models",
    "hooks",
    "acp",
    "auth",
    "mcp",
    "mcp-server",
];

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
        "config",
        "a config writer. The loader is strict and read-only, and nothing in \
         this build writes a config file — `-c/--set` overrides one run",
    ),
    (
        "rules",
        "somewhere to keep a rule. The grammar and its stable IDs are built and \
         `--allow`/`--deny` already reach the engine through them, so `ls` and \
         `explain` have their material; what `allow`, `deny`, `revoke` and \
         `remember` have nowhere to write to is a rule store — the same \
         read-only loader `config` waits on",
    ),
    (
        "doctor",
        "a unified diagnostic contract and actionable probes. Authentication, \
         model resolution, sandbox execution, and MCP now exist, but no health \
         projection composes their sanitized status and actionable remediation",
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

/// The spelling `--output` accepts, so a refusal can quote back something the
/// operator can retype rather than a debug rendering they cannot.
impl std::fmt::Display for Output {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Output::Text => "text",
            Output::Json => "json",
            Output::Jsonl => "jsonl",
        })
    }
}

/// Which durable session a run writes (`CLI.md` §3, "Session").
///
/// One value rather than independent durable flags, because §4.5 calls them a
/// mutually-exclusive group and a group is a thing the type can *be*, not a
/// rule someone has to remember to check. The recognized `--ephemeral`
/// spelling is refused before this value is built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionChoice {
    /// A fresh session, written under the store root.
    New,
    /// A named session: created if it is not there, attached if it is.
    Named(String),
    /// An id the caller supplied, so there is no capture-then-resume dance.
    Id(String),
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

/// The exact `ego usage [--period P] [--json]` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageArgs {
    pub period: UsagePeriod,
    pub session_dir: Option<PathBuf>,
    pub directory: Option<PathBuf>,
    pub json: bool,
}

/// The exact `ego fork ID` request and its shared store/workspace/output flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkArgs {
    pub source: SessionId,
    pub session_dir: Option<PathBuf>,
    pub directory: Option<PathBuf>,
    pub output: Output,
}

/// The exact deterministic headless compaction request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactArgs {
    pub source: SessionId,
    pub request_id: RequestId,
    pub session_dir: Option<PathBuf>,
    pub directory: Option<PathBuf>,
    pub output: Output,
}

/// The exact read-only `ego export ID` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportArgs {
    pub source: SessionId,
    pub sanitize: bool,
    pub format: ExportFormat,
    pub file: Option<PathBuf>,
    pub session_dir: Option<PathBuf>,
    pub directory: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum ExportFormat {
    Md,
    Json,
    Jsonl,
}

impl std::fmt::Display for ExportFormat {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Md => "md",
            Self::Json => "json",
            Self::Jsonl => "jsonl",
        })
    }
}

/// The exact `ego models [--provider P] [--refresh] [--json]` grammar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelsArgs {
    pub provider: Option<String>,
    pub refresh: bool,
    pub json: bool,
}

/// The exact `ego hooks ls [--json]` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HooksArgs {
    pub json: bool,
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

/// Fixed deployment bounds for `ego mcp-server`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerArgs {
    pub session_dir: Option<PathBuf>,
    pub directory: Option<PathBuf>,
    pub max_concurrent_runs: NonZeroUsize,
    pub retained_requests: NonZeroUsize,
    pub max_resource_bytes: NonZeroUsize,
}

/// One explicitly selected MCP server for a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpSelector {
    /// A server selected from effective configuration.
    Named(McpServerName),
    /// A strict invocation-scoped JSON server object, decoded during composition.
    File(PathBuf),
}

/// The exact semantic payload of `ego mcp`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpArgs {
    pub action: McpAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpAction {
    Add {
        name: McpServerName,
        server: McpServerConfig,
    },
    Remove {
        name: McpServerName,
    },
    List {
        json: bool,
    },
    Login {
        name: McpServerName,
    },
    Reload {
        name: Option<McpServerName>,
    },
}

/// The parsed command line of `ego run`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub prompt: Option<String>,
    pub(crate) agent_file: Option<AgentFile>,
    pub(crate) attach: Vec<String>,
    pub stdin: Stdin,
    pub model: Option<String>,
    pub offline: bool,
    /// Filesystem confinement, orthogonal to the permission mode. `None`
    /// leaves precedence resolution to config; the effective default is
    /// `workspace`.
    pub sandbox: Option<Confinement>,
    pub thinking: Option<ThinkingLevel>,
    pub system: Option<String>,
    pub append_system: Option<String>,
    pub instructions: Option<PathBuf>,
    pub bare: bool,
    /// The permission mode (`ARCHITECTURE.md` §3.4 axis 1).
    ///
    /// One field, not three, for the same reason [`SessionChoice`] is one
    /// field: `--plan` and `--yolo` are spellings of `--mode`, and three
    /// booleans would be three ways to say something that can only be one
    /// value — plus a fourth state, all-false, that means nothing.
    pub mode: Option<Mode>,
    /// Permission rules, in the grammar of
    /// `2026-08-21-a-rule-that-means-exactly-one-thing.md`. Carried as text
    /// and parsed by `Rule::load` inside the config layer, so a flag and a
    /// config file go through exactly one parser.
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    /// Answers that settle pending questions while resuming this session.
    /// Empty for every command except `resume --answer`.
    pub answers: Vec<ResumeAnswer>,
    pub session: SessionChoice,
    pub session_dir: Option<PathBuf>,
    pub artifact_dir: Option<PathBuf>,
    pub output: Output,
    pub schema: Option<String>,
    pub schema_retries: u32,
    pub budget: Option<String>,
    pub max_turns: Option<u32>,
    pub timeout: Option<String>,
    pub tools: Option<Vec<String>>,
    /// Ordered effective-name and invocation-file MCP selections.
    pub mcp: Vec<McpSelector>,
    pub directory: Option<PathBuf>,
    pub set: Vec<String>,
    pub profile: Option<String>,
    pub(crate) profile_source: Option<ego_core::config::Source>,
    pub quiet: bool,
    pub no_color: bool,
}

/// One validated `resume --answer QUESTION_ID=JSON` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeAnswer {
    pub id: ItemId,
    pub json: String,
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

    /// Settle a pending question. Repeat for multiple sibling questions.
    #[arg(
        long = "answer",
        value_name = "QUESTION_ID=JSON",
        action = ArgAction::Append,
        value_parser = parse_resume_answer
    )]
    answers: Vec<ResumeAnswer>,

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

/// `ego usage` — deterministic accounting for this workspace's local logs.
#[derive(Debug, Parser)]
#[command(
    name = "ego usage",
    about = "Report usage, context and cost for this workspace.",
    disable_help_subcommand = true
)]
struct UsageCommand {
    /// all|Ns|Nm|Nh
    #[arg(
        long,
        default_value = "all",
        value_parser = parse_usage_period,
        allow_hyphen_values = true
    )]
    period: UsagePeriod,

    /// Emit the exact versioned usage-report object.
    #[arg(long)]
    json: bool,

    #[command(flatten)]
    places: Places,
}

/// `ego fork ID` — the source boundary is always its latest durable idle event.
#[derive(Debug, Parser)]
#[command(
    name = "ego fork",
    about = "Fork a session at its latest durable idle boundary.",
    disable_help_subcommand = true
)]
struct ForkCommand {
    source: String,

    #[command(flatten)]
    common: Common,
}

/// `ego compact ID --request-id UUID` — no implicit idempotency identity.
#[derive(Debug, Parser)]
#[command(
    name = "ego compact",
    about = "Compact a session into its deterministic successor.",
    disable_help_subcommand = true
)]
struct CompactCommand {
    source: String,

    #[arg(long = "request-id")]
    request_id: String,

    #[command(flatten)]
    common: Common,
}

/// `ego export ID` — no writer, provider, prompt, or host runtime.
#[derive(Debug, Parser)]
#[command(
    name = "ego export",
    about = "Export a session locally or as a structurally sanitized bundle.",
    disable_help_subcommand = true
)]
struct ExportCommand {
    source: String,

    #[arg(long)]
    sanitize: bool,

    #[arg(long, value_enum, default_value_t = ExportFormat::Md)]
    format: ExportFormat,

    #[arg(long, value_name = "PATH")]
    file: Option<PathBuf>,

    #[command(flatten)]
    places: Places,
}

#[derive(Debug, Parser)]
#[command(
    name = "ego models",
    about = "List the shared model catalogue.",
    disable_help_subcommand = true
)]
struct ModelsCommand {
    /// Restrict enumeration and output to one admitted provider.
    #[arg(long)]
    provider: Option<String>,

    /// Refresh the selected discoverable provider slices.
    #[arg(long)]
    refresh: bool,

    /// Emit the exact versioned snapshot object.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Parser)]
#[command(
    name = "ego hooks",
    about = "Inspect the admitted system and user hook catalogue.",
    disable_help_subcommand = true
)]
struct HooksCommand {
    #[command(subcommand)]
    action: HooksSubcommand,
}

#[derive(Debug, Subcommand)]
enum HooksSubcommand {
    /// List the admitted hook catalogue without changing configuration.
    Ls {
        /// Emit the exact versioned hook-listing object.
        #[arg(long)]
        json: bool,
    },
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

/// `ego mcp-server` — the scoped run protocol, on stdio.
#[derive(Debug, Parser)]
#[command(
    name = "ego mcp-server",
    about = "Serve scoped ego runs over MCP on stdio.",
    disable_help_subcommand = true
)]
struct McpServerCommand {
    #[command(flatten)]
    places: Places,

    #[arg(long)]
    max_concurrent_runs: NonZeroUsize,

    #[arg(long)]
    retained_requests: NonZeroUsize,

    #[arg(long)]
    max_resource_bytes: NonZeroUsize,
}

/// Arguments shared by the three auth operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthArgs {
    pub action: AuthAction,
    pub provider: Option<String>,
    pub device: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthAction {
    Login,
    Logout,
    Status,
}

#[derive(Debug, Parser)]
#[command(name = "ego auth", about = "Manage subscription credentials.")]
struct AuthCommand {
    #[command(subcommand)]
    action: AuthSubcommand,
}

#[derive(Debug, Subcommand)]
enum AuthSubcommand {
    /// Authenticate and store a provider credential.
    Login {
        provider: Option<String>,
        /// Prefer device-code login when the provider offers it.
        #[arg(long)]
        device: bool,
    },
    /// Remove a stored provider credential.
    Logout { provider: Option<String> },
    /// Show credential type and expiry, never token values.
    Status { provider: Option<String> },
}

#[derive(Debug, Parser)]
#[command(
    name = "ego mcp",
    about = "Manage configured MCP servers.",
    disable_help_subcommand = true
)]
struct McpCommand {
    #[command(subcommand)]
    action: McpSubcommand,
}

#[derive(Debug, Subcommand)]
enum McpSubcommand {
    /// Add or replace one complete server entry.
    Add(McpAddCommand),
    /// Remove, mask, or reveal one configured server entry.
    #[command(name = "rm")]
    Remove { name: String },
    /// List configured and admitted MCP state.
    #[command(name = "ls")]
    List {
        /// Emit the typed machine-readable projection.
        #[arg(long)]
        json: bool,
    },
    /// Authorize one configured HTTP server through the product CIMD binding.
    Login { name: String },
    /// Reconcile every configured server, or one named server.
    Reload { name: Option<String> },
}

#[derive(Debug, ClapArgs)]
#[command(
    override_usage = "ego mcp add <NAME> --stdio [--cwd <DIR>] [--inherit-env <NAME>]... -- <COMMAND> [ARG]...\n       ego mcp add <NAME> --http <URL> [--auth <none|oauth>] [--allow-insecure-loopback]"
)]
struct McpAddCommand {
    name: String,

    /// Configure a subprocess transport. Its command follows literal `--`.
    #[arg(long, required_unless_present = "http", conflicts_with = "http")]
    stdio: bool,

    /// Configure a Streamable HTTP transport.
    #[arg(
        long,
        value_name = "URL",
        required_unless_present = "stdio",
        conflicts_with = "stdio"
    )]
    http: Option<String>,

    /// Working directory for the stdio child.
    #[arg(long, value_name = "DIR", requires = "stdio", conflicts_with = "http")]
    cwd: Option<PathBuf>,

    /// Environment variable inherited by the stdio child. Repeatable.
    #[arg(
        long,
        value_name = "NAME",
        action = ArgAction::Append,
        requires = "stdio",
        conflicts_with = "http"
    )]
    inherit_env: Vec<String>,

    /// HTTP authentication mode.
    #[arg(
        long,
        value_parser = ["none", "oauth"],
        requires = "http",
        conflicts_with = "stdio"
    )]
    auth: Option<String>,

    /// Consent to plaintext HTTP for an exact loopback host.
    #[arg(long, requires = "http", conflicts_with = "stdio")]
    allow_insecure_loopback: bool,
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
    /// Read the instruction and its invocation configuration from an agent file.
    #[arg(short = 'f', long = "file", value_name = "FILE")]
    file: Option<PathBuf>,

    /// Supply one declared agent-file template variable.
    #[arg(long = "var", value_name = "KEY=VALUE", action = ArgAction::Append)]
    vars: Vec<String>,

    /// Add exact image bytes as ordered context before the prompt text.
    #[arg(
        long = "attach",
        value_name = "IMAGE_PATH|HTTPS_IMAGE_URL",
        action = ArgAction::Append
    )]
    attach: Vec<String>,

    /// provider/model slug, e.g. ollama/qwen3
    #[arg(short = 'm', long)]
    model: Option<String>,

    /// No non-loopback egress from ego's own HTTP transport.
    #[arg(long)]
    offline: bool,

    /// Filesystem confinement: ro|workspace|off.
    #[arg(long)]
    sandbox: Option<Confinement>,

    /// off|minimal|low|medium|high|xhigh|max
    #[arg(long, value_parser = parse_thinking)]
    thinking: Option<ThinkingLevel>,

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

    /// Refused: every run uses a durable session.
    #[arg(long, group = "session-choice")]
    ephemeral: bool,

    #[command(flatten)]
    common: Common,

    /// Override the sanitized run-artifact root for profile `ci`.
    #[arg(long)]
    artifact_dir: Option<PathBuf>,

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

    /// Attach a named effective or strict invocation-scoped JSON MCP server.
    #[arg(
        long,
        value_name = "NAME|@FILE",
        action = ArgAction::Append,
        value_parser = parse_mcp_selector
    )]
    mcp: Vec<McpSelector>,

    /// Permission mode: plan|default|auto|edits|yolo.
    #[arg(long, value_parser = parse_mode, group = "mode-choice")]
    mode: Option<Mode>,

    /// Shorthand for --mode plan.
    #[arg(long, group = "mode-choice")]
    plan: bool,

    /// Shorthand for --mode yolo.
    #[arg(long, group = "mode-choice")]
    yolo: bool,

    /// Allow rule, e.g. 'read(src/**)'. Repeatable.
    #[arg(long, value_name = "RULE", action = ArgAction::Append)]
    allow: Vec<String>,

    /// Deny rule, e.g. 'write(**)'. Repeatable.
    #[arg(long, value_name = "RULE", action = ArgAction::Append)]
    deny: Vec<String>,

    /// Dotted-path config override, value parsed as TOML.
    #[arg(short = 'c', long = "set", value_name = "K=V", action = ArgAction::Append)]
    set: Vec<String>,

    /// Layer [profile.NAME] from config.
    #[arg(long)]
    profile: Option<String>,

    #[arg(short = 'q', long)]
    quiet: bool,

    #[arg(long)]
    no_color: bool,
}

/// The five modes, by the names an operator types.
///
/// Hand-written rather than a `clap::ValueEnum` derive on [`Mode`], for the
/// reason `parse_thinking` below is hand-written: `Mode` lives in `ego-core`,
/// which has no `clap` dependency and should not gain one to answer a
/// question about spelling.
fn parse_mode(value: &str) -> Result<Mode, String> {
    match value {
        "plan" => Ok(Mode::Plan),
        "default" => Ok(Mode::Default),
        "auto" => Ok(Mode::Auto),
        "edits" => Ok(Mode::Edits),
        "yolo" => Ok(Mode::Yolo),
        other => Err(format!(
            "`{other}` is not a permission mode: plan|default|auto|edits|yolo"
        )),
    }
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

fn parse_mcp_selector(value: &str) -> Result<McpSelector, String> {
    if let Some(path) = value.strip_prefix('@') {
        if path.is_empty() {
            return Err("an MCP file selector requires a path after `@`".to_string());
        }
        return Ok(McpSelector::File(PathBuf::from(path)));
    }
    value
        .parse::<McpServerName>()
        .map(McpSelector::Named)
        .map_err(|error| error.to_string())
}

fn parse_mcp_name(value: String) -> Result<McpServerName, UsageError> {
    value
        .parse::<McpServerName>()
        .map_err(|error| UsageError(error.to_string()))
}

fn parse_mcp_env_name(value: String) -> Result<McpEnvironmentName, UsageError> {
    value
        .parse::<McpEnvironmentName>()
        .map_err(|error| UsageError(error.to_string()))
}

fn into_mcp_args(
    command: McpCommand,
    separator_tail: Option<Vec<String>>,
) -> Result<McpArgs, UsageError> {
    let action = match command.action {
        McpSubcommand::Add(add) => {
            let name = parse_mcp_name(add.name)?;
            let server = if add.stdio {
                let mut child = separator_tail.ok_or_else(|| {
                    UsageError(
                        "--stdio requires literal `--` followed by a non-empty command".to_string(),
                    )
                })?;
                if child.first().is_none_or(String::is_empty) {
                    return Err(UsageError(
                        "--stdio requires literal `--` followed by a non-empty command".to_string(),
                    ));
                }
                let command = child.remove(0);
                let inherit_env = add
                    .inherit_env
                    .into_iter()
                    .map(parse_mcp_env_name)
                    .collect::<Result<Vec<_>, _>>()?;
                McpServerConfig::stdio(command, child, add.cwd, inherit_env)
                    .map_err(|error| UsageError(error.to_string()))?
            } else {
                if separator_tail.is_some() {
                    return Err(UsageError(
                        "--http does not accept a command or arguments after literal `--`"
                            .to_string(),
                    ));
                }
                let raw_url = add.http.ok_or_else(|| {
                    UsageError("ego mcp add requires exactly one transport".to_string())
                })?;
                let url = raw_url
                    .parse::<McpHttpUrl>()
                    .map_err(|error| UsageError(error.to_string()))?;
                let auth = match add.auth.as_deref() {
                    None | Some("none") => McpHttpAuth::None,
                    Some("oauth") => McpHttpAuth::Oauth,
                    Some(_) => unreachable!("clap restricts MCP HTTP auth values"),
                };
                McpHttpConfig::new(url, auth, add.allow_insecure_loopback)
                    .map(McpServerConfig::Http)
                    .map_err(|error| UsageError(error.to_string()))?
            };
            McpAction::Add { name, server }
        }
        McpSubcommand::Remove { name } => {
            reject_mcp_separator(separator_tail)?;
            McpAction::Remove {
                name: parse_mcp_name(name)?,
            }
        }
        McpSubcommand::List { json } => {
            reject_mcp_separator(separator_tail)?;
            McpAction::List { json }
        }
        McpSubcommand::Login { name } => {
            reject_mcp_separator(separator_tail)?;
            McpAction::Login {
                name: parse_mcp_name(name)?,
            }
        }
        McpSubcommand::Reload { name } => {
            reject_mcp_separator(separator_tail)?;
            McpAction::Reload {
                name: name.map(parse_mcp_name).transpose()?,
            }
        }
    };
    Ok(McpArgs { action })
}

fn reject_mcp_separator(separator_tail: Option<Vec<String>>) -> Result<(), UsageError> {
    if separator_tail.is_some() {
        Err(UsageError(
            "literal `--` and trailing arguments apply only to `ego mcp add --stdio`".to_string(),
        ))
    } else {
        Ok(())
    }
}

fn parse_resume_answer(value: &str) -> Result<ResumeAnswer, String> {
    let (id, json) = value.split_once('=').ok_or_else(|| {
        "an answer must be QUESTION_ID=JSON; the first `=` separates the stable id from JSON"
            .to_string()
    })?;
    let id =
        ItemId::from_str(id).map_err(|error| format!("`{id}` is not a question id: {error}"))?;
    serde_json::from_str::<serde_json::Value>(json)
        .map_err(|error| format!("the answer for {id} is not valid JSON: {error}"))?;
    Ok(ResumeAnswer {
        id,
        json: json.to_string(),
    })
}

/// Split `argv` at the first bare `--`.
///
/// Everything after it is opaque positional input. It is prompt text for the
/// run grammar and the child argv for `ego mcp add --stdio`; neither matrix nor
/// clap may reinterpret those tokens as ego flags.
fn split_at_separator(argv: Vec<String>) -> (Vec<String>, Option<Vec<String>>) {
    match argv.iter().position(|token| token == "--") {
        Some(at) => {
            let mut head = argv;
            let tail = head.split_off(at + 1);
            head.pop();
            (head, Some(tail))
        }
        None => (argv, None),
    }
}

/// What clap produced: the command, or an answer to a question about it.
enum Parsed<T> {
    Command(T),
    Answered(String),
}

/// Run one of the command structs, keeping `--help` and `--version` out of the
/// error path.
///
/// clap signals both through `Err`, alongside the genuine failures, and the
/// single `map_err` this replaces flattened the two into `UsageError` — so
/// `ego ls --help` printed to stderr and exited 2. Only [`ErrorKind`] can tell
/// them apart, which is why this is a function and not a scan of `tokens`: a
/// `--help` sitting where a flag's value belongs is clap's question to answer.
///
/// The failure branch is unchanged: clap's own rendering already names the
/// offending argument and the group it violated, and re-writing it here would
/// be a second voice saying the same thing, worse.
fn clap_or_answer<T: Parser>(tokens: &[String]) -> Result<Parsed<T>, UsageError> {
    match T::try_parse_from(tokens) {
        Ok(parsed) => Ok(Parsed::Command(parsed)),
        Err(err)
            if matches!(
                err.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            Ok(Parsed::Answered(err.to_string()))
        }
        Err(err) => Err(UsageError(err.to_string().trim_end().to_string())),
    }
}

/// `ego --help`: the command set, from the tables the refusals read.
///
/// Deliberately short. It names what this build has and points at `CLI.md` §2
/// for the rest rather than reprinting every row of "waiting on", because a
/// caller who types one of them is told which subsystem it waits on by
/// the refusal itself — which is where that sentence is already maintained.
fn overview(program: &str) -> String {
    let mut text = format!(
        "ego {} — an agent you run from a terminal.\n\nUsage:\n  {program} [OPTIONS] [PROMPT]\n  {program} <COMMAND> [OPTIONS]\n\nInteractive:\n  {program} -m ollama/qwen3\n\nCommands:\n",
        env!("CARGO_PKG_VERSION")
    );
    for name in BUILT {
        let about = match *name {
            "run" => "Run an agent from a prompt, headless",
            "resume" => "Continue a session; no ID = latest in this workspace",
            "fork" => "Fork a session at its latest durable idle boundary",
            "ls" => "List sessions (this workspace; --all for every one)",
            "usage" => "Report usage, context and cost for this workspace",
            "compact" => "Compact a session into its deterministic successor",
            "export" => "Export a session or sanitized bundle",
            "models" => "List the shared model catalogue",
            "hooks" => "Inspect the admitted hook catalogue",
            "acp" => "Serve the Agent Client Protocol on stdio",
            "mcp-server" => "Serve scoped ego runs over MCP on stdio",
            "auth" => "Manage subscription credentials",
            "mcp" => "Manage configured MCP servers",
            other => other,
        };
        text.push_str(&format!("  {name:<8}{about}\n"));
    }
    text.push_str(&format!(
        "\n`{program} <COMMAND> --help` describes one of them. CLI.md §2 \
         documents the remaining commands; each is refused by name, with the subsystem it \
         waits on. Start Ollama and run `ollama pull qwen3` before the local \
         example above.\n"
    ));
    text
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

    // `ego --help` on its own, which reached the interactive refusal below and
    // therefore told someone looking for the command set about the permission
    // dialog. There is no top-level clap command to render — §2's first line is
    // `ego [PROMPT]`, so the four built commands are dispatched by hand — so
    // this is written out, from the same table the refusals read.
    //
    // Exactly these argument vectors and no more. A `head` with anything else
    // in it may be a flag carrying `--help` as its value, which is clap's
    // question to answer and not one a scan here can answer correctly.
    if let [only] = head.as_slice() {
        match only.as_str() {
            "--help" | "-h" => return Ok(Invocation::Answered(overview(&program))),
            "--version" | "-V" => {
                return Ok(Invocation::Answered(format!(
                    "ego {}\n",
                    env!("CARGO_PKG_VERSION")
                )));
            }
            _ => {}
        }
    }

    // No subcommand is the interactive session of `CLI.md` §6, not an error.
    // What is left is parsed by exactly the `run` grammar, because a session
    // resolves a model, a workspace and a ruleset the same way a one-shot
    // does — the difference is who supplies the prompts, and that is a
    // difference in driver rather than in grammar.
    let subcommand = match head.first().map(String::as_str) {
        Some(name) if BUILT.contains(&name) => Some(name.to_string()),
        _ => None,
    };

    let mut tokens: Vec<String> = head;
    // Both words, not just the program. clap takes the command it names in
    // `Usage:` from argv[0] and not from the `name` attribute on the struct, so
    // replacing the subcommand with the bare program made `ego ls --help`
    // answer `Usage: ego [OPTIONS]` — which is a line that does not work:
    // `ego --all` has no subcommand, so it reaches the interactive refusal.
    // Every error clap renders for these structs names the command this way,
    // so it is one substitution rather than one per message.
    match &subcommand {
        // Replaces the subcommand token, which has now been read.
        Some(name) => tokens[0] = format!("{program} {name}"),
        // Nothing to replace: the first token is already an argument. clap
        // still needs an argv[0], so the program name is prepended rather
        // than written over something a person typed.
        None => tokens.insert(0, program.to_string()),
    }
    // The stdio MCP form consumes the already-separated tail semantically so
    // every later child token, including another `--`, remains byte-for-byte
    // argv. Other commands give the delimiter back to clap as prompt syntax.
    if subcommand.as_deref() != Some("mcp")
        && let Some(tail) = &tail
    {
        tokens.push("--".to_string());
        tokens.extend(tail.iter().cloned());
    }

    match subcommand.as_deref() {
        None => match clap_or_answer::<RunCommand>(&tokens)? {
            Parsed::Answered(text) => Ok(Invocation::Answered(text)),
            Parsed::Command(parsed) => Ok(Invocation::Interactive(Box::new(into_args(parsed)?))),
        },
        Some("resume") => match clap_or_answer::<ResumeCommand>(&tokens)? {
            Parsed::Answered(text) => Ok(Invocation::Answered(text)),
            Parsed::Command(parsed) => {
                let args = into_resume_args(parsed)?;
                if args.prompt.is_none()
                    && args.agent_file.is_none()
                    && args.stdin == Stdin::Ambient
                    && args.answers.is_empty()
                {
                    Ok(Invocation::Interactive(Box::new(args)))
                } else {
                    Ok(Invocation::Resume(Box::new(args)))
                }
            }
        },
        Some("acp") => match clap_or_answer::<AcpCommand>(&tokens)? {
            Parsed::Answered(text) => Ok(Invocation::Answered(text)),
            Parsed::Command(parsed) => Ok(Invocation::Acp(Box::new(AcpArgs {
                session_dir: parsed.places.session_dir,
                directory: parsed.places.directory,
            }))),
        },
        Some("mcp-server") => match clap_or_answer::<McpServerCommand>(&tokens)? {
            Parsed::Answered(text) => Ok(Invocation::Answered(text)),
            Parsed::Command(parsed) => Ok(Invocation::McpServer(Box::new(McpServerArgs {
                session_dir: parsed.places.session_dir,
                directory: parsed.places.directory,
                max_concurrent_runs: parsed.max_concurrent_runs,
                retained_requests: parsed.retained_requests,
                max_resource_bytes: parsed.max_resource_bytes,
            }))),
        },
        Some("ls") => match clap_or_answer::<LsCommand>(&tokens)? {
            Parsed::Answered(text) => Ok(Invocation::Answered(text)),
            Parsed::Command(parsed) => Ok(Invocation::Ls(Box::new(LsArgs {
                all: parsed.all,
                session_dir: parsed.common.places.session_dir,
                directory: parsed.common.places.directory,
                output: parsed.common.output,
            }))),
        },
        Some("usage") => match clap_or_answer::<UsageCommand>(&tokens)? {
            Parsed::Answered(text) => Ok(Invocation::Answered(text)),
            Parsed::Command(parsed) => Ok(Invocation::Usage(Box::new(UsageArgs {
                period: parsed.period,
                session_dir: parsed.places.session_dir,
                directory: parsed.places.directory,
                json: parsed.json,
            }))),
        },
        Some("fork") => match clap_or_answer::<ForkCommand>(&tokens)? {
            Parsed::Answered(text) => Ok(Invocation::Answered(text)),
            Parsed::Command(parsed) => {
                let source = SessionId::from_str(&parsed.source).map_err(|error| {
                    UsageError(format!("`{}` is not a session id: {error}", parsed.source))
                })?;
                Ok(Invocation::Fork(Box::new(ForkArgs {
                    source,
                    session_dir: parsed.common.places.session_dir,
                    directory: parsed.common.places.directory,
                    output: parsed.common.output,
                })))
            }
        },
        Some("compact") => match clap_or_answer::<CompactCommand>(&tokens)? {
            Parsed::Answered(text) => Ok(Invocation::Answered(text)),
            Parsed::Command(parsed) => {
                let source = SessionId::from_str(&parsed.source).map_err(|error| {
                    UsageError(format!("`{}` is not a session id: {error}", parsed.source))
                })?;
                if source.uuid().is_nil() {
                    return Err(UsageError(
                        "the compaction source must not be the nil session id".to_owned(),
                    ));
                }
                let request_id = RequestId::from_str(&parsed.request_id).map_err(|error| {
                    UsageError(format!(
                        "`{}` is not a compaction request id: {error}",
                        parsed.request_id
                    ))
                })?;
                if request_id.uuid().is_nil() {
                    return Err(UsageError(
                        "the compaction request id must not be nil".to_owned(),
                    ));
                }
                Ok(Invocation::Compact(Box::new(CompactArgs {
                    source,
                    request_id,
                    session_dir: parsed.common.places.session_dir,
                    directory: parsed.common.places.directory,
                    output: parsed.common.output,
                })))
            }
        },
        Some("export") => match clap_or_answer::<ExportCommand>(&tokens)? {
            Parsed::Answered(text) => Ok(Invocation::Answered(text)),
            Parsed::Command(parsed) => {
                let source = SessionId::from_str(&parsed.source).map_err(|error| {
                    UsageError(format!("`{}` is not a session id: {error}", parsed.source))
                })?;
                if parsed.format == ExportFormat::Jsonl && parsed.file.is_some() {
                    return Err(UsageError(
                        "ego export --format jsonl cannot be combined with --file".to_owned(),
                    ));
                }
                Ok(Invocation::Export(Box::new(ExportArgs {
                    source,
                    sanitize: parsed.sanitize,
                    format: parsed.format,
                    file: parsed.file,
                    session_dir: parsed.places.session_dir,
                    directory: parsed.places.directory,
                })))
            }
        },
        Some("models") => match clap_or_answer::<ModelsCommand>(&tokens)? {
            Parsed::Answered(text) => Ok(Invocation::Answered(text)),
            Parsed::Command(parsed) => Ok(Invocation::Models(Box::new(ModelsArgs {
                provider: parsed.provider,
                refresh: parsed.refresh,
                json: parsed.json,
            }))),
        },
        Some("hooks") => match clap_or_answer::<HooksCommand>(&tokens)? {
            Parsed::Answered(text) => Ok(Invocation::Answered(text)),
            Parsed::Command(parsed) => match parsed.action {
                HooksSubcommand::Ls { json } => Ok(Invocation::Hooks(HooksArgs { json })),
            },
        },
        Some("auth") => match clap_or_answer::<AuthCommand>(&tokens)? {
            Parsed::Answered(text) => Ok(Invocation::Answered(text)),
            Parsed::Command(parsed) => {
                let (action, provider, device) = match parsed.action {
                    AuthSubcommand::Login { provider, device } => {
                        (AuthAction::Login, provider, device)
                    }
                    AuthSubcommand::Logout { provider } => (AuthAction::Logout, provider, false),
                    AuthSubcommand::Status { provider } => (AuthAction::Status, provider, false),
                };
                Ok(Invocation::Auth(Box::new(AuthArgs {
                    action,
                    provider,
                    device,
                })))
            }
        },
        Some("mcp") => match clap_or_answer::<McpCommand>(&tokens)? {
            Parsed::Answered(text) => Ok(Invocation::Answered(text)),
            Parsed::Command(parsed) => Ok(Invocation::Mcp(Box::new(into_mcp_args(parsed, tail)?))),
        },
        _ => match clap_or_answer::<RunCommand>(&tokens)? {
            Parsed::Answered(text) => Ok(Invocation::Answered(text)),
            Parsed::Command(parsed) => Ok(Invocation::Run(Box::new(into_args(parsed)?))),
        },
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
    let ResumeCommand {
        id,
        prompt,
        answers,
        flags,
    } = command;

    reject_ephemeral(flags.ephemeral)?;

    for (given, flag) in [
        (flags.session.is_some(), "--session"),
        (flags.session_id.is_some(), "--session-id"),
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

    let mut args = into_args(RunCommand { prompt, flags })?;
    args.session = SessionChoice::Resume(id);
    args.answers = answers;
    Ok(args)
}

fn into_args(command: RunCommand) -> Result<Args, UsageError> {
    let RunCommand { prompt, flags } = command;

    reject_ephemeral(flags.ephemeral)?;

    if flags.file.is_some() && prompt.is_some() {
        return Err(UsageError(
            "an argv prompt and an agent file are competing instruction inputs: \
             drop the prompt to use --file, or drop --file to use the argv prompt"
                .to_string(),
        ));
    }
    if flags.file.is_none() && !flags.vars.is_empty() {
        return Err(UsageError(
            "--var applies only to an agent file: add --file FILE or drop --var".to_string(),
        ));
    }
    let agent_file = flags
        .file
        .as_ref()
        .map(|path| {
            let path = match (&flags.common.places.directory, path.is_absolute()) {
                (_, true) | (None, false) => path.clone(),
                (Some(directory), false) => directory.join(path),
            };
            agent_file::load(&path, &flags.vars)
        })
        .transpose()?;

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

    // The same collapse `SessionChoice` gets, one axis over. clap's group
    // has already refused two of them together, so this cannot silently
    // prefer one spelling over another.
    let mode = if flags.plan {
        Some(Mode::Plan)
    } else if flags.yolo {
        Some(Mode::Yolo)
    } else {
        flags.mode
    };

    let session = if let Some(name) = flags.session {
        SessionChoice::Named(name)
    } else if let Some(id) = flags.session_id {
        SessionChoice::Id(id)
    } else {
        SessionChoice::New
    };

    Ok(Args {
        prompt,
        agent_file,
        attach: flags.attach,
        stdin,
        model: flags.model,
        offline: flags.offline,
        sandbox: flags.sandbox,
        thinking: flags.thinking,
        system: flags.system,
        append_system: flags.append_system,
        instructions: flags.instructions,
        bare: flags.bare,
        mode,
        allow: flags.allow,
        deny: flags.deny,
        answers: Vec::new(),
        session,
        session_dir: flags.common.places.session_dir,
        artifact_dir: flags.artifact_dir,
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
        mcp: flags.mcp,
        directory: flags.common.places.directory,
        set: flags.set,
        profile_source: flags
            .profile
            .as_ref()
            .map(|_| ego_core::config::Source::Flags),
        profile: flags.profile,
        quiet: flags.quiet,
        no_color: flags.no_color,
    })
}

fn reject_ephemeral(ephemeral: bool) -> Result<(), UsageError> {
    if ephemeral {
        return Err(UsageError(
            "--ephemeral is not a supported session mode: use the durable default, \
             --session NAME, or --session-id ID"
                .to_string(),
        ));
    }
    Ok(())
}

/// Parse the strict duration arithmetic shared by `--timeout` and usage.
pub(crate) fn duration_seconds(spec: &str) -> Result<u64, UsageError> {
    let refuse = || {
        UsageError(format!(
            "`{spec}` is not a duration: write it as '90s', '10m' or '2h'"
        ))
    };
    let spec = spec.trim();
    let (digits, scale) = if let Some(digits) = spec.strip_suffix('s') {
        (digits, 1)
    } else if let Some(digits) = spec.strip_suffix('m') {
        (digits, 60)
    } else if let Some(digits) = spec.strip_suffix('h') {
        (digits, 3_600)
    } else {
        return Err(refuse());
    };
    if digits.is_empty() || !digits.bytes().all(|digit| digit.is_ascii_digit()) {
        return Err(refuse());
    }
    let count: u64 = digits.parse().map_err(|_| refuse())?;
    count.checked_mul(scale).ok_or_else(refuse)
}

fn parse_usage_period(spec: &str) -> Result<UsagePeriod, String> {
    if spec != spec.trim() {
        return Err(format!(
            "`{spec}` is not a usage period: write `all`, `90s`, `10m` or `2h`"
        ));
    }
    if spec == "all" {
        return Ok(UsagePeriod::All);
    }
    let seconds = duration_seconds(spec).map_err(|_| {
        format!("`{spec}` is not a usage period: write `all`, `90s`, `10m` or `2h`")
    })?;
    let seconds = NonZeroU64::new(seconds)
        .ok_or_else(|| format!("`{spec}` is not a usage period: the duration must be positive"))?;
    Ok(UsagePeriod::Last { seconds })
}
