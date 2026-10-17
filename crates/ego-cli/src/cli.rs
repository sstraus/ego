//! The argument grammar: `CLI.md` §3 and the parse half of §4.5.
//!
//! The literal `--` boundary is separated before command dispatch. Clap then
//! owns option/value boundaries, including hidden diagnostic spellings. Only
//! parsed flags can earn a retired/unavailable refusal; prompt values and MCP
//! child argv are never searched for flags. Resume metadata is dispatched from
//! typed fields after the same clap parser used by both aliases.

use std::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use clap::error::ErrorKind;
use clap::{ArgAction, Args as ClapArgs, CommandFactory, Parser, Subcommand};
use ego_core::config::{
    ManagedConfigKey, ManagedConfigValue, ManagedRuleEffect, McpEnvironmentName, McpHttpAuth,
    McpHttpConfig, McpHttpUrl, McpServerConfig, McpServerName, Mode, ModelRef, ProfileName,
    UserConfigTarget,
};
use ego_core::endpoint::{Admitted, PublicBind, Refusal, Requested, admit};
use ego_core::event::id::{ItemId, SessionId};
use ego_core::generator::types::ThinkingLevel;
use ego_core::permit::Confinement;
use ego_core::permit::rule::{Rule, RuleId};
use ego_core::run::RequestId;
use ego_core::usage::UsagePeriod;

use crate::agent_file::{self, AgentFile};

/// What the process was asked to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Invocation {
    Worktree(WorktreeAction),
    Orchestrator(Box<OrchestratorArgs>),
    /// `ego config get|set|unset|ls|edit` — typed user configuration management.
    Config(Box<ConfigArgs>),
    /// `ego rules ls|allow|deny|revoke|remember|explain`.
    Rules(Box<RulesArgs>),
    /// `ego auth login|logout|status [PROVIDER]`.
    Auth(Box<AuthArgs>),
    /// `ego mcp add|rm|ls` — typed MCP configuration management.
    Mcp(Box<McpArgs>),
    /// `ego hooks ls [--json]` — the admitted system/user hook catalogue.
    Hooks(HooksArgs),
    /// `ego doctor [--json]` — the read-only redacted health snapshot.
    Doctor(DoctorArgs),
    Checkpoint(CheckpointArgs),
    /// Transient read-only setup diagnostics, outside the ordinary run pipeline.
    Setup(SetupArgs),
    /// `ego run [PROMPT]` — the headless one-shot of `CLI.md` §2.
    Run(Box<Args>),
    /// `ego resume [ID] [PROMPT]` — the same run against a session that
    /// already exists. One [`Args`] and not a second shape: everything after
    /// "which session" is identical, and a `ResumeArgs` would be the parallel
    /// implementation `AGENTS.md`'s reuse rule forbids.
    Resume(Box<Args>),
    /// `ego resume ID --name ...|--goal-operation ...` — durable metadata only.
    SessionMetadata(Box<SessionMetadataArgs>),
    /// `ego ls` — the catalogue, this workspace's by default.
    Ls(Box<LsArgs>),
    /// `ego usage` — one versioned report for this workspace.
    Usage(Box<UsageArgs>),
    /// `ego fork ID [--at ITEM_ID]` — branch at the tip or after a completed turn.
    Fork(Box<ForkArgs>),
    Rm(Box<RmArgs>),
    /// `ego compact ID [--request-id UUID]` — publish one deterministic successor.
    Compact(Box<CompactArgs>),
    /// `ego export ID` — render one local or structurally sanitized transcript.
    Export(Box<ExportArgs>),
    /// `ego models` — the shared model catalogue snapshot.
    Models(Box<ModelsArgs>),
    /// `ego acp` — serve the Agent Client Protocol on stdio, until the host
    /// closes it.
    Acp(Box<AcpArgs>),
    /// `ego a2a --listen ADDR` — serve the Agent2Agent protocol on a socket,
    /// until the process is stopped.
    A2a(Box<A2aArgs>),
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
const UNAVAILABLE: &[Unavailable] = &[Unavailable {
    flags: &["-v", "--verbose"],
    waiting_on: "nothing, and it is not coming. The capability it used to \
                     wait on now exists — `EGO_LOG` installs a subscriber for \
                     the `tracing` events this build emits, and `EGO_LOG_FILE` \
                     collects them — but it is deliberately not a flag. \
                     `CLI.md` §3 is the argument grammar of a *run*, and a \
                     diagnostic level changes nothing a run does and is \
                     recorded in none of its evidence; regression case froze this \
                     table with verbose excluded, and reading the environment \
                     is what lets the capability ship without amending it. See \
                     `docs/decisions/\
                     2026-09-07-diagnostics-get-a-destination-before-a-switch.md`",
}];

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
    "continue",
    "fork",
    "rm",
    "compact",
    "export",
    "ls",
    "usage",
    "models",
    "hooks",
    "doctor",
    "checkpoint",
    "setup",
    "acp",
    "a2a",
    "auth",
    "mcp",
    "config",
    "roots",
    "rules",
    "mcp-server",
    "orchestrator",
    "worktree",
];

/// A command `CLI.md` §2 documents that this build does not implement, and the
/// subsystem it is waiting on.
///
/// Empty, because `ego doctor` was the last one and story `regression_case` built it.
/// Every command `CLI.md` §2 documents is now in [`BUILT`].
///
/// It ships empty rather than being deleted, for the reason [`RETIRED`] ships
/// empty. The subcommand twin of [`UNAVAILABLE`]: in the first availability
/// table `run` was the only implemented named command, so every other
/// documented command fell through to the *interactive* refusal below — which
/// told someone who typed `ego resume` that the permission dialog was missing.
/// True, and about a surface they had not asked for. `CLI.md` §3.1 states the
/// standard for flags ("refused **by name**, with the subsystem it waits on")
/// and nothing had applied it one level up. The next command documented before
/// it is written wants that refusal already there, not invented under deadline.
///
/// A tuple rather than a struct, matching [`RETIRED`], because both halves are
/// needed by the test that binds this table to the document.
///
/// `waiting_on` is sourced from `ARCHITECTURE.md` §8's milestone table where it
/// places the work, and from the code where it does not — a subsystem that
/// table does not place still gets a reason, never a milestone invented for it.
pub const UNBUILT: &[(&str, &str)] = &[];

/// A flag that was removed, and what replaced it.
///
/// Empty, because nothing has been retired yet. It ships anyway, with its own
/// test: `CLI.md` §4.3 says a retired flag errors by name with its
/// replacement, and the moment that rule is easiest to forget is the first
/// retirement — when there is no mechanism, only a note in a changelog.
pub const RETIRED: &[(&str, &str)] = &[];

/// Every `CLI.md` §2 command as one clap tree.
///
/// Argv is not parsed through this. The top level is a hand-written dispatch
/// because `ego PROMPT` has to mean a prompt and not an unknown subcommand,
/// and clap has no shape for that. What this exists for is everything
/// *derived* from the grammar — the reference and the shell completions — so
/// they come from the same definitions the parser uses instead of being a
/// second description of it that can quietly stop being true.
///
/// The pairing with [`BUILT`] is the invariant, and a test holds it: a command
/// this build offers and this tree omits would be missing from the reference
/// and uncompletable, while the reverse would document one that does not exist.
pub fn root_command() -> clap::Command {
    use clap::CommandFactory;
    // Each subcommand names itself `ego X` for its own `--help`; under a root
    // it is just `X`. The bin name is deliberately *not* set here — clap
    // derives `ego X` for every subcommand when the tree is built, and an
    // explicit one blocks that derivation and takes `clap_complete` with it.
    fn named(name: &'static str, command: clap::Command) -> clap::Command {
        command.name(name)
    }
    RunCommand::command()
        .name("ego")
        .version(env!("CARGO_PKG_VERSION"))
        .about("Run an agent, from one command line.")
        // `ego help` is not in the command inventory `CLI.md` §2 documents, and
        // building the tree would otherwise invent it — putting a subcommand in
        // the reference and the completions that the real parser answers by
        // treating "help" as a prompt.
        .disable_help_subcommand(true)
        .subcommand(named("run", headless_run_command()))
        .subcommand(named("resume", resume_command()))
        .subcommand(named("continue", resume_command()))
        .subcommand(named("fork", ForkCommand::command()))
        .subcommand(named("rm", RmCommand::command()))
        .subcommand(named("compact", CompactCommand::command()))
        .subcommand(named("export", ExportCommand::command()))
        .subcommand(named("ls", LsCommand::command()))
        .subcommand(named("usage", UsageCommand::command()))
        .subcommand(named("models", ModelsCommand::command()))
        .subcommand(named("hooks", HooksCommand::command()))
        .subcommand(named("doctor", DoctorCommand::command()))
        .subcommand(named("checkpoint", CheckpointArgs::command()))
        .subcommand(named("setup", SetupCommand::command()))
        .subcommand(named("acp", AcpCommand::command()))
        .subcommand(named("a2a", A2aCommand::command()))
        .subcommand(named("auth", AuthCommand::command()))
        .subcommand(named("mcp", McpCommand::command()))
        .subcommand(named("config", ConfigCommand::command()))
        .subcommand(named("roots", RootsCommand::command()))
        .subcommand(named("rules", RulesCommand::command()))
        .subcommand(named("mcp-server", McpServerCommand::command()))
        .subcommand(named("orchestrator", OrchestratorCommand::command()))
        .subcommand(named("worktree", WorktreeCommand::command()))
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum WorktreeAction {
    /// Inspect a durable local worktree receipt without starting a model.
    Show { receipt: uuid::Uuid },
    /// Remove only a clean worktree whose commits are integrated; retain its branch.
    Cleanup { receipt: uuid::Uuid },
}

#[derive(Debug, Parser)]
#[command(
    name = "ego worktree",
    about = "Inspect or clean up a retained local worktree."
)]
struct WorktreeCommand {
    #[command(subcommand)]
    action: WorktreeAction,
}

#[derive(Debug, PartialEq, Eq)]
pub struct OrchestratorArgs {
    pub root: Option<PathBuf>,
}

#[derive(Parser)]
struct OrchestratorCommand {
    #[command(subcommand)]
    action: OrchestratorAction,
}

#[derive(Subcommand)]
enum OrchestratorAction {
    /// Inspect tracked children and unread results without model generation or inbox reads.
    Brief {
        #[arg(long)]
        root: Option<PathBuf>,
    },
}

/// The refusal a retired flag earns, if `name` names one in `table`.
///
/// The table is an argument and not [`RETIRED`] read directly, because
/// `RETIRED` is empty. A rule whose only exercise is a loop over an empty
/// slice is a rule that has never run, and the first retirement is exactly
/// when nobody can afford to discover that. Same for [`unbuilt_refusal`], and
/// for the same reason those two tables ship empty rather than being deleted.
pub fn retired_refusal(name: &str, table: &[(&str, &str)]) -> Option<UsageError> {
    table
        .iter()
        .find(|(retired, _)| *retired == name)
        .map(|(_, replacement)| UsageError(format!("{name} has been retired; use {replacement}")))
}

/// The refusal a documented-but-unbuilt command earns, if `first` names one.
pub fn unbuilt_refusal(program: &str, first: &str, table: &[(&str, &str)]) -> Option<UsageError> {
    table
        .iter()
        .find(|(name, _)| *name == first)
        .map(|(name, waiting_on)| {
            UsageError(format!(
                "`{program} {name}` is documented in CLI.md §2 and is not available \
                 in this build: it is waiting on {waiting_on}"
            ))
        })
}

/// Let clap, not a token scan, decide whether a diagnostic flag was supplied.
fn diagnostic_flags() -> impl Iterator<Item = &'static str> {
    UNAVAILABLE
        .iter()
        .flat_map(|entry| entry.flags.iter().copied())
        .chain(RETIRED.iter().map(|(name, _)| *name))
}

fn with_diagnostic_flags(mut command: clap::Command) -> clap::Command {
    for spelling in diagnostic_flags() {
        let argument = clap::Arg::new(spelling)
            .action(ArgAction::SetTrue)
            .hide(true)
            .global(true);
        let argument = if let Some(long) = spelling.strip_prefix("--") {
            argument.long(long)
        } else {
            argument.short(
                spelling
                    .trim_start_matches('-')
                    .chars()
                    .next()
                    .expect("short flag"),
            )
        };
        command = command.arg(argument);
    }
    command
}

fn check_matrix(matches: &clap::ArgMatches) -> Result<(), UsageError> {
    for name in diagnostic_flags().filter(|name| matches.get_flag(name)) {
        if let Some(refusal) = retired_refusal(name, RETIRED) {
            return Err(refusal);
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

/// Metadata may share store/output selection, but never run options or prompts.
fn headless_run_command() -> clap::Command {
    RunCommand::command().mut_arg("vim", |argument| argument.hide(true))
}

fn resume_command() -> clap::Command {
    let mut command = ResumeCommand::command();
    for id in ["session", "session_id", "worktree"] {
        command = command.mut_arg(id, |argument| argument.hide(true));
    }
    let launch = command
        .get_arguments()
        .filter(|argument| {
            ![
                "id",
                "prompt",
                "answers",
                "name",
                "goal_operation",
                "session_dir",
                "directory",
                "output",
            ]
            .contains(&argument.get_id().as_str())
        })
        .map(|argument| argument.get_id().clone())
        .collect::<Vec<_>>();
    command.group(
        clap::ArgGroup::new("resume-run-options")
            .args(launch)
            .multiple(true),
    )
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
    /// `ARCHITECTURE.md` §3.
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

/// One closed metadata-only resume request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMetadataArgs {
    pub session: SessionId,
    pub operation: SessionMetadataOperation,
    pub session_dir: Option<PathBuf>,
    pub directory: Option<PathBuf>,
    pub output: Output,
    pub(crate) profile: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionMetadataOperation {
    Name(NameOperation),
    Goal(GoalOperation),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameOperation {
    Show,
    Set(String),
    Clear,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalOperation {
    Show,
    Set(String),
    Achieve,
    Block(String),
}

/// The exact `ego usage [--period P] [--json]` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageArgs {
    pub period: UsagePeriod,
    pub session_dir: Option<PathBuf>,
    pub directory: Option<PathBuf>,
    pub json: bool,
    pub all_roles: bool,
}

/// The target and explicit acknowledgements for session deletion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RmArgs {
    pub source: SessionId,
    pub yes: bool,
    pub force: bool,
    pub session_dir: Option<PathBuf>,
    pub directory: Option<PathBuf>,
}

/// The exact fork request and its shared store/workspace/output flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkArgs {
    pub source: SessionId,
    pub at: Option<ItemId>,
    pub session_dir: Option<PathBuf>,
    pub directory: Option<PathBuf>,
    pub output: Output,
}

/// The exact deterministic headless compaction request. The parser mints the
/// request identity when an ordinary caller does not supply an advanced retry
/// override, so downstream compaction stays deterministic without exposing a
/// UUID ritual in the normal command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactArgs {
    pub budget: Option<String>,
    pub max_output_tokens: Option<std::num::NonZeroU64>,
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
    pub follow: bool,
    pub after: Option<u64>,
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
    Transcript,
}

impl std::fmt::Display for ExportFormat {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Md => "md",
            Self::Json => "json",
            Self::Jsonl => "jsonl",
            Self::Transcript => "transcript",
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

/// The exact `ego doctor [--json]` request.
///
/// One field, because doctor takes no subject: it reports on this
/// installation, and there is only one of those.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupArgs {
    pub items: Option<Vec<usize>>,
    pub judge: Option<String>,
    pub prompt: Option<String>,
    pub profile: Option<String>,
    pub directory: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorArgs {
    pub json: bool,
}

/// The parsed command line of `ego acp`.
///
/// With no `listen`, `ego acp`'s stdout **is** the JSON-RPC stream, so there is
/// no `--output` to choose. Naming `listen` opts into the shared inbound
/// endpoint and its finite process-owned limits.
///
/// `sandbox` is a ceiling and not a default. A connected host picks its own
/// confinement per session and can only pick something at least this strong
/// (`ARCHITECTURE.md`).
/// `None` means the operator named none, and the resolved configuration
/// stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpArgs {
    pub session_dir: Option<PathBuf>,
    pub directory: Option<PathBuf>,
    pub sandbox: Option<Confinement>,
    pub shape: SessionShape,
    pub listen: Option<String>,
    pub allow_public_bind: bool,
    pub max_connections: u32,
    pub connection_lifetime: Duration,
    pub max_ingress_bytes: u64,
}

/// The general shape of a session, resolved from the launch line.
///
/// The one place `ego run` and a served protocol agree on what `--tools`,
/// `--allow`, `--deny`, `--profile`, `--append-system` and `--instructions`
/// mean. [`Default`] is "the operator named nothing", which is exactly the
/// session a host got before a served protocol could be shaped at all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionShape {
    pub read_only_dir: Option<PathBuf>,
    pub system: Option<String>,
    pub bare: bool,
    pub append_system: Option<String>,
    pub output_style: Option<String>,
    pub instructions: Option<PathBuf>,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    /// `None` is every builtin; `Some(vec![])` is the tool-less session.
    pub tools: Option<Vec<String>>,
    pub profile: Option<String>,
    pub profile_source: Option<ego_core::config::Source>,
}

impl From<Shape> for SessionShape {
    fn from(shape: Shape) -> Self {
        Self {
            read_only_dir: shape.read_only_dir,
            system: shape.system,
            bare: shape.bare,
            append_system: shape.append_system,
            output_style: shape.output_style,
            instructions: shape.instructions,
            allow: shape.allow,
            deny: shape.deny,
            // `--no-tools` is the empty list rather than a second field: "no
            // tools" and "these tools" are the same question answered, and two
            // fields would let them disagree.
            tools: if shape.no_tools {
                Some(Vec::new())
            } else {
                shape.tools
            },
            profile_source: shape
                .profile
                .as_ref()
                .map(|_| ego_core::config::Source::Flags),
            profile: shape.profile,
        }
    }
}

/// Where `ego a2a` serves, whether the operator asked for it to be reachable
/// from outside this machine, and the confinement ceiling carried forward for
/// the run-serving routes.
///
/// `listen` is unparsed on purpose: which endpoint kind a spelling means is
/// `ego_core::endpoint`'s decision, not the argument parser's, and duplicating
/// it here would be a second place that could disagree about what is loopback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct A2aArgs {
    pub session_dir: Option<PathBuf>,
    pub directory: Option<PathBuf>,
    pub listen: String,
    pub allow_public_bind: bool,
    pub sandbox: Option<Confinement>,
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
        client_id: Option<String>,
        cimd: Option<String>,
        dcr: bool,
        callback_port: u16,
        refresh: bool,
        issuer: Option<String>,
        account: Option<String>,
    },
    Reload {
        name: Option<McpServerName>,
    },
}

/// The exact semantic payload of `ego config`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigArgs {
    pub action: ConfigAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigAction {
    Capabilities,
    Snapshot {
        target: UserConfigTarget,
    },
    Batch {
        changes: Vec<ego_core::config::UserConfigMutation>,
        expected: Option<String>,
    },
    RootsList {
        target: UserConfigTarget,
        json: bool,
    },
    Root {
        target: UserConfigTarget,
        path: PathBuf,
        access: Option<ego_core::permit::RootAccess>,
    },
    Get {
        target: UserConfigTarget,
        key: ManagedConfigKey,
        json: bool,
    },
    Set {
        target: UserConfigTarget,
        key: ManagedConfigKey,
        value: ManagedConfigValue,
    },
    Unset {
        target: UserConfigTarget,
        key: ManagedConfigKey,
    },
    List {
        target: UserConfigTarget,
        json: bool,
    },
    Effective {
        profile: Option<String>,
        json: bool,
    },
    /// Edit the complete user document, including profiles.
    Edit,
}

/// One root rule-management request with all syntax already typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RulesArgs {
    pub action: RulesAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RulesAction {
    List {
        target: UserConfigTarget,
        json: bool,
    },
    Add {
        target: UserConfigTarget,
        effect: ManagedRuleEffect,
        rule: Rule,
    },
    Revoke {
        target: UserConfigTarget,
        id: RuleId,
    },
    Remember {
        target: UserConfigTarget,
        directory: Option<PathBuf>,
        yes: bool,
        effect: ManagedRuleEffect,
        tool: String,
        arguments: serde_json::Map<String, serde_json::Value>,
    },
    Explain {
        target: UserConfigTarget,
        directory: Option<PathBuf>,
        json: bool,
        tool: String,
        arguments: serde_json::Map<String, serde_json::Value>,
    },
}

/// The parsed command line of `ego run`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub worktree: Option<String>,
    /// Launch-owned read grant; never accepted from a configuration layer.
    pub read_only_dir: Option<PathBuf>,
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
    pub session_prompt: Option<ego_core::config::SessionSystemPrompt>,
    pub append_system: Option<String>,
    pub output_style: Option<String>,
    pub instructions: Option<PathBuf>,
    pub bare: bool,
    pub trust_workspace: bool,
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
    pub(crate) output_explicit: bool,
    pub schema: Option<String>,
    pub schema_retries: u32,
    /// Additional delivery-safe attempts allowed for each model candidate.
    pub retry: u32,
    /// Interactive default recovery; an explicit --retry keeps finite ownership.
    pub(crate) provider_recovery: bool,
    /// Ordered model candidates used after a safely-unsent failure or refusal.
    pub fallbacks: Vec<ModelRef>,
    pub budget: Option<String>,
    pub max_output_tokens: Option<std::num::NonZeroU64>,
    pub goal: Option<String>,
    pub goal_turns: u32,
    pub no_context_tool: bool,
    pub context_clear: bool,
    pub context_interval: u32,
    pub goal_check_command: Option<Vec<String>>,
    pub max_turns: Option<u32>,
    pub timeout: Option<String>,
    pub tools: Option<Vec<String>>,
    /// Ordered effective-name and invocation-file MCP selections.
    pub mcp: Vec<McpSelector>,
    pub directory: Option<PathBuf>,
    pub set: Vec<String>,
    pub profile: Option<String>,
    pub(crate) profile_source: Option<ego_core::config::Source>,
    /// ACP host ceiling, independent of the selected repository profile.
    pub(crate) ceiling_profile: Option<String>,
    pub quiet: bool,
    pub no_color: bool,
    pub plain: bool,
    /// Select Vim composer editing for this interactive process.
    pub vim: bool,
}

/// One validated `resume --answer QUESTION_ID=JSON` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeAnswer {
    pub id: ItemId,
    pub json: String,
}

mod commands;
mod resume_seat;
use commands::*;
pub use commands::{AuthAction, AuthArgs};

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

fn parse_model_ref(value: &str) -> Result<ModelRef, String> {
    ego_ai::catalogue::ModelSlug::parse(value)
        .map(|slug| ModelRef(slug.to_string()))
        .map_err(|error| error.to_string())
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
                    .map(|mut config| {
                        if add.profile.as_deref() == Some("legacy-sse") {
                            config.profile = ego_core::config::McpHttpProfile::LegacySse;
                        }
                        McpServerConfig::Http(config)
                    })
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
        McpSubcommand::Login {
            name,
            client_id,
            cimd,
            dcr,
            callback_port,
            refresh,
            issuer,
            account,
        } => {
            reject_mcp_separator(separator_tail)?;
            McpAction::Login {
                name: parse_mcp_name(name)?,
                client_id,
                cimd,
                dcr,
                callback_port,
                refresh,
                issuer,
                account,
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

fn parse_name_operation(value: &str) -> Result<NameOperation, String> {
    match value {
        "show" => Ok(NameOperation::Show),
        "clear" => Ok(NameOperation::Clear),
        _ => value
            .strip_prefix("set=")
            .filter(|text| !text.is_empty())
            .map(|text| NameOperation::Set(text.to_owned()))
            .ok_or_else(|| "expected show, set=TEXT, or clear".to_owned()),
    }
}

fn parse_goal_operation(value: &str) -> Result<GoalOperation, String> {
    match value {
        "show" => Ok(GoalOperation::Show),
        "achieve" => Ok(GoalOperation::Achieve),
        _ => {
            if let Some(text) = value.strip_prefix("set=").filter(|text| !text.is_empty()) {
                return Ok(GoalOperation::Set(text.to_owned()));
            }
            value
                .strip_prefix("block=")
                .filter(|text| !text.is_empty())
                .map(|text| GoalOperation::Block(text.to_owned()))
                .ok_or_else(|| "expected show, set=TEXT, achieve, or block=REASON".to_owned())
        }
    }
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
    clap_command_or_answer(tokens, T::command(), None)
}

fn clap_command_or_answer<T: Parser>(
    tokens: &[String],
    command: clap::Command,
    redacted_usage: Option<&str>,
) -> Result<Parsed<T>, UsageError> {
    match with_diagnostic_flags(command).try_get_matches_from(tokens) {
        Ok(matches) => {
            check_matrix(&matches)?;
            T::from_arg_matches(&matches)
                .map(Parsed::Command)
                .map_err(|error| UsageError(error.to_string().trim_end().to_owned()))
        }
        Err(err)
            if matches!(
                err.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            Ok(Parsed::Answered(err.to_string()))
        }
        Err(err) => Err(UsageError(
            redacted_usage
                .map(str::to_owned)
                .unwrap_or_else(|| err.to_string().trim_end().to_owned()),
        )),
    }
}

/// Parse config without letting clap quote a rejected positional value.
///
/// The set positional may contain a secret even though managed configuration
/// will reject it. Structural failures therefore retain the frozen usage but
/// not clap's echo of the supplied argv.
fn config_clap_or_answer(tokens: &[String]) -> Result<Parsed<ConfigCommand>, UsageError> {
    clap_command_or_answer(
        tokens,
        ConfigCommand::command(),
        Some(
            "invalid ego config invocation\n\nUsage:\n  ego config get [--profile NAME] [--json] KEY\n  ego config set [--profile NAME] KEY=TOML_VALUE\n  ego config unset [--profile NAME] KEY\n  ego config ls [--profile NAME] [--json]\n  ego config edit",
        ),
    )
}

fn rules_clap_or_answer(tokens: &[String]) -> Result<Parsed<RulesCommand>, UsageError> {
    clap_command_or_answer(
        tokens,
        RulesCommand::command(),
        Some(
            "invalid ego rules invocation\n\nUsage:\n  ego rules ls [--profile NAME] [--json]\n  ego rules allow [--profile NAME] RULE\n  ego rules deny [--profile NAME] RULE\n  ego rules revoke [--profile NAME] RULE_ID\n  ego rules remember [--profile NAME] [-C DIR] [--yes] allow|deny TOOL ARGS_JSON\n  ego rules explain [--profile NAME] [-C DIR] [--json] TOOL ARGS_JSON",
        ),
    )
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
    let width = BUILT.iter().map(|name| name.len()).max().unwrap_or(0) + 2;
    for name in BUILT {
        let about = match *name {
            "run" => "Run an agent from a prompt, headless",
            "resume" | "continue" => {
                "Continue a session; terminal picker, otherwise latest in this workspace"
            }
            "rm" => "Delete a stored session",
            "fork" => "Fork a session at its tip or after a completed turn",
            "ls" => "List sessions (this workspace; --all for every one)",
            "usage" => "Report usage, context and cost for this workspace",
            "compact" => "Compact a session into its deterministic successor",
            "export" => "Export a session or sanitized bundle",
            "models" => "List the shared model catalogue",
            "hooks" => "Inspect the admitted hook catalogue",
            "acp" => "Serve the Agent Client Protocol on stdio",
            "a2a" => "Serve Agent2Agent discovery information",
            "setup" => "Inspect expected work without running effects",
            "doctor" => "Print a read-only redacted health snapshot",
            "checkpoint" => "Preview and selectively restore session-owned file edits",
            "mcp-server" => "Serve scoped ego runs over MCP on stdio",
            "orchestrator" => "Inspect native orchestration without model generation",
            "worktree" => "Inspect or safely remove a retained local worktree",
            "auth" => "Manage subscription credentials",
            "mcp" => "Manage configured MCP servers",
            "roots" => "Manage perimeter roots",
            "config" => "Manage stored user configuration",
            "rules" => "Inspect and manage permission rules",
            other => other,
        };
        text.push_str(&format!("  {name:<width$}{about}\n"));
    }
    use clap::CommandFactory;
    text.push('\n');
    text.push_str(
        &RunCommand::command()
            .help_template("{options}")
            .render_help()
            .to_string(),
    );
    text.push_str(&format!(
        "\n`{program} <COMMAND> --help` describes one of them.\n"
    ));
    if !UNBUILT.is_empty() {
        text.push_str("CLI.md §2 documents the remaining commands; each is refused by name, with the subsystem it waits on.\n");
    }
    text.push_str("Start Ollama and run `ollama pull qwen3` before the local example above.\n");
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

    // A documented command answers for itself, before the fallback below can
    // answer for it. Only an exact first token counts: §2's first line is
    // `ego [PROMPT]`, so a word that is not on this list is a prompt, and
    // refusing every unrecognised token would turn a prompt into a typo.
    if let Some(first) = head.first()
        && let Some(refusal) = unbuilt_refusal(&program, first, UNBUILT)
    {
        return Err(refusal);
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

    parse_subcommand(subcommand.as_deref(), tokens, tail)
}

fn parse_subcommand(
    subcommand: Option<&str>,
    tokens: Vec<String>,
    tail: Option<Vec<String>>,
) -> Result<Invocation, UsageError> {
    match subcommand {
        Some("worktree") => match clap_or_answer::<WorktreeCommand>(&tokens)? {
            Parsed::Answered(text) => Ok(Invocation::Answered(text)),
            Parsed::Command(parsed) => Ok(Invocation::Worktree(parsed.action)),
        },
        Some("orchestrator") => match clap_or_answer::<OrchestratorCommand>(&tokens)? {
            Parsed::Answered(text) => Ok(Invocation::Answered(text)),
            Parsed::Command(parsed) => match parsed.action {
                OrchestratorAction::Brief { root } => {
                    Ok(Invocation::Orchestrator(Box::new(OrchestratorArgs {
                        root,
                    })))
                }
            },
        },
        None => match clap_or_answer::<RunCommand>(&tokens)? {
            Parsed::Answered(text) => Ok(Invocation::Answered(text)),
            Parsed::Command(parsed) => Ok(Invocation::Interactive(Box::new(into_args(
                parsed,
                INTERACTIVE_PROVIDER_RETRIES,
            )?))),
        },
        Some("resume" | "continue") => {
            match clap_command_or_answer::<ResumeCommand>(&tokens, resume_command(), None)? {
                Parsed::Answered(text) => Ok(Invocation::Answered(text)),
                Parsed::Command(parsed) => {
                    if parsed.name.is_some() || parsed.goal_operation.is_some() {
                        return into_session_metadata_args(parsed)
                            .map(|args| Invocation::SessionMetadata(Box::new(args)));
                    }
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
            }
        }
        Some("acp") => match clap_or_answer::<AcpCommand>(&tokens)? {
            Parsed::Answered(text) => Ok(Invocation::Answered(text)),
            Parsed::Command(parsed) => into_acp_invocation(parsed),
        },
        Some("a2a") => match clap_or_answer::<A2aCommand>(&tokens)? {
            Parsed::Answered(text) => Ok(Invocation::Answered(text)),
            Parsed::Command(parsed) => Ok(Invocation::A2a(Box::new(A2aArgs {
                session_dir: parsed.places.session_dir,
                directory: parsed.places.directory,
                listen: parsed.listen,
                allow_public_bind: parsed.allow_public_bind,
                sandbox: parsed.ceiling.sandbox,
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
        Some(command @ ("ls" | "usage" | "rm" | "fork" | "compact" | "export")) => {
            parse_session_command(command, &tokens)
        }
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
        Some("setup") => match clap_or_answer::<SetupCommand>(&tokens)? {
            Parsed::Answered(text) => Ok(Invocation::Answered(text)),
            Parsed::Command(parsed) => Ok(Invocation::Setup(SetupArgs {
                items: parsed.items,
                judge: parsed.judge,
                prompt: parsed.prompt,
                profile: parsed.profile,
                directory: parsed.directory,
            })),
        },
        Some("checkpoint") => match clap_or_answer::<CheckpointArgs>(&tokens)? {
            Parsed::Answered(text) => Ok(Invocation::Answered(text)),
            Parsed::Command(parsed) => Ok(Invocation::Checkpoint(parsed)),
        },
        Some("doctor") => match clap_or_answer::<DoctorCommand>(&tokens)? {
            Parsed::Answered(text) => Ok(Invocation::Answered(text)),
            Parsed::Command(parsed) => Ok(Invocation::Doctor(DoctorArgs { json: parsed.json })),
        },
        Some(command @ ("auth" | "roots" | "config" | "rules" | "mcp")) => {
            parse_management_command(command, &tokens, tail)
        }
        _ => match clap_command_or_answer::<RunCommand>(&tokens, headless_run_command(), None)? {
            Parsed::Answered(text) => Ok(Invocation::Answered(text)),
            Parsed::Command(parsed) => Ok(Invocation::Run(Box::new(into_args(parsed, 0)?))),
        },
    }
}

fn parse_session_command(command: &str, tokens: &[String]) -> Result<Invocation, UsageError> {
    match command {
        "ls" => match clap_or_answer::<LsCommand>(tokens)? {
            Parsed::Answered(text) => Ok(Invocation::Answered(text)),
            Parsed::Command(parsed) => Ok(Invocation::Ls(Box::new(LsArgs {
                all: parsed.all,
                session_dir: parsed.common.places.session_dir,
                directory: parsed.common.places.directory,
                output: parsed.common.output.unwrap_or(Output::Text),
            }))),
        },
        "usage" => match clap_or_answer::<UsageCommand>(tokens)? {
            Parsed::Answered(text) => Ok(Invocation::Answered(text)),
            Parsed::Command(parsed) => Ok(Invocation::Usage(Box::new(UsageArgs {
                period: parsed.period,
                session_dir: parsed.places.session_dir,
                directory: parsed.places.directory,
                json: parsed.json,
                all_roles: parsed.all_roles,
            }))),
        },
        "rm" => match clap_or_answer::<RmCommand>(tokens)? {
            Parsed::Answered(text) => Ok(Invocation::Answered(text)),
            Parsed::Command(parsed) => into_rm_invocation(parsed),
        },
        "fork" => match clap_or_answer::<ForkCommand>(tokens)? {
            Parsed::Answered(text) => Ok(Invocation::Answered(text)),
            Parsed::Command(parsed) => {
                let source = SessionId::from_str(&parsed.source).map_err(|error| {
                    UsageError(format!("`{}` is not a session id: {error}", parsed.source))
                })?;
                let at = parsed
                    .at
                    .map(|id| {
                        ItemId::from_str(&id).map_err(|error| {
                            UsageError(format!("`{id}` is not a message id: {error}"))
                        })
                    })
                    .transpose()?;
                Ok(Invocation::Fork(Box::new(ForkArgs {
                    source,
                    at,
                    session_dir: parsed.common.places.session_dir,
                    directory: parsed.common.places.directory,
                    output: parsed.common.output.unwrap_or(Output::Text),
                })))
            }
        },
        "compact" => match clap_or_answer::<CompactCommand>(tokens)? {
            Parsed::Answered(text) => Ok(Invocation::Answered(text)),
            Parsed::Command(parsed) => into_compact_invocation(parsed),
        },
        "export" => match clap_or_answer::<ExportCommand>(tokens)? {
            Parsed::Answered(text) => Ok(Invocation::Answered(text)),
            Parsed::Command(parsed) => into_export_invocation(parsed),
        },
        _ => unreachable!("only session commands reach this parser"),
    }
}

fn parse_management_command(
    command: &str,
    tokens: &[String],
    tail: Option<Vec<String>>,
) -> Result<Invocation, UsageError> {
    match command {
        "auth" => match clap_or_answer::<AuthCommand>(tokens)? {
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
        "roots" => match clap_or_answer::<RootsCommand>(tokens)? {
            Parsed::Answered(text) => Ok(Invocation::Answered(text)),
            Parsed::Command(parsed) => {
                let action = match parsed.action {
                    RootsSubcommand::Add {
                        profile,
                        path,
                        access,
                    } => ConfigAction::Root {
                        target: config_target(profile)?,
                        path,
                        access: Some(access.0),
                    },
                    RootsSubcommand::Rm { profile, path } => ConfigAction::Root {
                        target: config_target(profile)?,
                        path,
                        access: None,
                    },
                    RootsSubcommand::Ls { profile, json } => ConfigAction::RootsList {
                        target: config_target(profile)?,
                        json,
                    },
                };
                Ok(Invocation::Config(Box::new(ConfigArgs { action })))
            }
        },
        "config" => match config_clap_or_answer(tokens)? {
            Parsed::Answered(text) => Ok(Invocation::Answered(text)),
            Parsed::Command(parsed) => Ok(Invocation::Config(Box::new(into_config_args(parsed)?))),
        },
        "rules" => match rules_clap_or_answer(tokens)? {
            Parsed::Answered(text) => Ok(Invocation::Answered(text)),
            Parsed::Command(parsed) => Ok(Invocation::Rules(Box::new(into_rules_args(parsed)?))),
        },
        "mcp" => match clap_or_answer::<McpCommand>(tokens)? {
            Parsed::Answered(text) => Ok(Invocation::Answered(text)),
            Parsed::Command(parsed) => Ok(Invocation::Mcp(Box::new(into_mcp_args(parsed, tail)?))),
        },
        _ => unreachable!("only management commands reach this parser"),
    }
}

fn into_acp_invocation(parsed: AcpCommand) -> Result<Invocation, UsageError> {
    ego_core::config::validate_prompt_options(
        parsed.shape.bare,
        parsed.shape.instructions.is_some(),
    )
    .map_err(|message| UsageError(message.to_owned()))?;
    let defaults = ego_listen::ConnectionLimits::default();
    Ok(Invocation::Acp(Box::new(AcpArgs {
        session_dir: parsed.places.session_dir,
        directory: parsed.places.directory,
        sandbox: parsed.ceiling.sandbox,
        shape: parsed.shape.into(),
        listen: parsed.listen,
        allow_public_bind: parsed.allow_public_bind,
        max_connections: parsed
            .max_connections
            .map_or(defaults.max_connections(), NonZeroU32::get),
        connection_lifetime: parsed
            .connection_lifetime
            .unwrap_or(defaults.connection_lifetime()),
        max_ingress_bytes: parsed
            .max_ingress_bytes
            .map_or(defaults.max_ingress_bytes(), NonZeroU64::get),
    })))
}

fn into_rm_invocation(parsed: RmCommand) -> Result<Invocation, UsageError> {
    let source = SessionId::from_str(&parsed.source)
        .map_err(|error| UsageError(format!("invalid session id: {error}")))?;
    if source.uuid().is_nil() {
        return Err(UsageError("session id must not be nil".into()));
    }
    Ok(Invocation::Rm(Box::new(RmArgs {
        source,
        yes: parsed.yes,
        force: parsed.force,
        session_dir: parsed.places.session_dir,
        directory: parsed.places.directory,
    })))
}

fn into_compact_invocation(parsed: CompactCommand) -> Result<Invocation, UsageError> {
    let source = SessionId::from_str(&parsed.source)
        .map_err(|error| UsageError(format!("`{}` is not a session id: {error}", parsed.source)))?;
    if source.uuid().is_nil() {
        return Err(UsageError(
            "the compaction source must not be the nil session id".to_owned(),
        ));
    }
    let request_id = match parsed.request_id {
        Some(request_id) => RequestId::from_str(&request_id).map_err(|error| {
            UsageError(format!(
                "`{request_id}` is not a compaction request id: {error}"
            ))
        })?,
        None => RequestId::from_uuid(uuid::Uuid::now_v7()),
    };
    if request_id.uuid().is_nil() {
        return Err(UsageError(
            "the compaction request id must not be nil".to_owned(),
        ));
    }
    Ok(Invocation::Compact(Box::new(CompactArgs {
        source,
        request_id,
        budget: parsed.budget,
        max_output_tokens: parsed.max_output_tokens,
        session_dir: parsed.common.places.session_dir,
        directory: parsed.common.places.directory,
        output: parsed.common.output.unwrap_or(Output::Text),
    })))
}

fn into_export_invocation(parsed: ExportCommand) -> Result<Invocation, UsageError> {
    let source = SessionId::from_str(&parsed.source)
        .map_err(|error| UsageError(format!("`{}` is not a session id: {error}", parsed.source)))?;
    if parsed.format == ExportFormat::Jsonl && parsed.file.is_some() {
        return Err(UsageError(
            "ego export --format jsonl cannot be combined with --file".to_owned(),
        ));
    }
    if (parsed.follow || parsed.after.is_some()) && parsed.format != ExportFormat::Transcript {
        return Err(UsageError(
            "--follow and --after require --format transcript".to_owned(),
        ));
    }
    if parsed.format == ExportFormat::Transcript && (parsed.sanitize || parsed.file.is_some()) {
        return Err(UsageError(
            "transcript cannot be combined with --sanitize or --file".to_owned(),
        ));
    }
    Ok(Invocation::Export(Box::new(ExportArgs {
        source,
        sanitize: parsed.sanitize,
        format: parsed.format,
        follow: parsed.follow,
        after: parsed.after,
        file: parsed.file,
        session_dir: parsed.places.session_dir,
        directory: parsed.places.directory,
    })))
}

fn validate_keyboard_value(value: &ManagedConfigValue) -> Result<(), UsageError> {
    if let ManagedConfigValue::Keybindings(bindings) = value {
        crate::repl::actions::ActionRegistry::final_surface()
            .with_keybindings(bindings)
            .map_err(UsageError)?;
    }
    Ok(())
}

fn into_config_args(command: ConfigCommand) -> Result<ConfigArgs, UsageError> {
    let action = match command.action {
        ConfigSubcommand::Capabilities => ConfigAction::Capabilities,
        ConfigSubcommand::Snapshot { profile } => ConfigAction::Snapshot {
            target: config_target(profile)?,
        },
        ConfigSubcommand::Batch {
            profile,
            sets,
            unsets,
            if_revision,
        } => {
            let target = config_target(profile)?;
            if sets.len() + unsets.len() == 0 || sets.len() + unsets.len() > 64 {
                return Err(UsageError("managed batch requires 1..64 operations".into()));
            }
            let mut changes = Vec::new();
            let mut keys = std::collections::HashSet::new();
            for assignment in sets {
                let (key, raw) = assignment
                    .split_once('=')
                    .ok_or_else(|| UsageError("batch set requires KEY=TOML_VALUE".into()))?;
                let key = managed_config_key(&target, key)?;
                if !keys.insert(key.to_string()) {
                    return Err(UsageError("duplicate batch key".into()));
                }
                let value = ManagedConfigValue::parse(key.clone(), raw).map_err(|_| {
                    UsageError(format!("invalid TOML value or type for config key `{key}`"))
                })?;
                validate_keyboard_value(&value)?;
                changes.push(ego_core::config::UserConfigMutation::ManagedSet {
                    target: target.clone(),
                    key,
                    value,
                });
            }
            for key in unsets {
                let key = managed_config_key(&target, &key)?;
                if !keys.insert(key.to_string()) {
                    return Err(UsageError("duplicate batch key".into()));
                }
                changes.push(ego_core::config::UserConfigMutation::ManagedUnset {
                    target: target.clone(),
                    key,
                });
            }
            ConfigAction::Batch {
                changes,
                expected: if_revision,
            }
        }
        ConfigSubcommand::Get { profile, json, key } => {
            let target = config_target(profile)?;
            let key = managed_config_key(&target, &key)?;
            ConfigAction::Get { target, key, json }
        }
        ConfigSubcommand::Set {
            profile,
            assignment,
        } => {
            let target = config_target(profile)?;
            let Some((key, raw_value)) = assignment.split_once('=') else {
                return Err(UsageError(
                    "ego config set requires exactly one KEY=TOML_VALUE assignment".to_owned(),
                ));
            };
            let key = managed_config_key(&target, key)?;
            let value = ManagedConfigValue::parse(key.clone(), raw_value).map_err(|_| {
                UsageError(format!("invalid TOML value or type for config key `{key}`"))
            })?;
            validate_keyboard_value(&value)?;
            ConfigAction::Set { target, key, value }
        }
        ConfigSubcommand::Unset { profile, key } => {
            let target = config_target(profile)?;
            let key = managed_config_key(&target, &key)?;
            ConfigAction::Unset { target, key }
        }
        ConfigSubcommand::List {
            profile,
            json,
            effective,
        } => {
            if effective {
                ConfigAction::Effective { profile, json }
            } else {
                ConfigAction::List {
                    target: config_target(profile)?,
                    json,
                }
            }
        }
        ConfigSubcommand::Edit => ConfigAction::Edit,
    };
    Ok(ConfigArgs { action })
}

fn into_rules_args(command: RulesCommand) -> Result<RulesArgs, UsageError> {
    let action = match command.action {
        RulesSubcommand::List { profile, json } => RulesAction::List {
            target: config_target(profile)?,
            json,
        },
        RulesSubcommand::Allow { profile, rule } => RulesAction::Add {
            target: config_target(profile)?,
            effect: ManagedRuleEffect::Allow,
            rule: managed_rule(&rule)?,
        },
        RulesSubcommand::Deny { profile, rule } => RulesAction::Add {
            target: config_target(profile)?,
            effect: ManagedRuleEffect::Deny,
            rule: managed_rule(&rule)?,
        },
        RulesSubcommand::Revoke { profile, id } => {
            let id = managed_rule_id(id)?;
            RulesAction::Revoke {
                target: config_target(profile)?,
                id,
            }
        }
        RulesSubcommand::Remember {
            profile,
            directory,
            yes,
            effect,
            tool,
            arguments,
        } => RulesAction::Remember {
            target: config_target(profile)?,
            directory,
            yes,
            effect: managed_rule_effect(&effect)?,
            tool,
            arguments: rule_arguments(&arguments)?,
        },
        RulesSubcommand::Explain {
            profile,
            directory,
            json,
            tool,
            arguments,
        } => RulesAction::Explain {
            target: config_target(profile)?,
            directory,
            json,
            tool,
            arguments: rule_arguments(&arguments)?,
        },
    };
    Ok(RulesArgs { action })
}

pub(crate) fn managed_rule(raw: &str) -> Result<Rule, UsageError> {
    Rule::load(raw).map_err(|_| UsageError("invalid or unenforceable permission rule".to_owned()))
}

pub(crate) fn managed_rule_id(raw: impl AsRef<str>) -> Result<RuleId, UsageError> {
    let id = RuleId::from_str(raw.as_ref())
        .map_err(|_| UsageError("invalid permission rule ID".to_owned()))?;
    if !id.is_known() {
        return Err(UsageError(
            "permission rule ID uses an unsupported grammar generation".to_owned(),
        ));
    }
    Ok(id)
}

pub(crate) fn managed_rule_effect(raw: &str) -> Result<ManagedRuleEffect, UsageError> {
    match raw {
        "allow" => Ok(ManagedRuleEffect::Allow),
        "deny" => Ok(ManagedRuleEffect::Deny),
        _ => Err(UsageError(
            "rule effect must be exactly `allow` or `deny`".to_owned(),
        )),
    }
}

pub(crate) fn rule_arguments(
    raw: &str,
) -> Result<serde_json::Map<String, serde_json::Value>, UsageError> {
    match serde_json::from_str(raw) {
        Ok(serde_json::Value::Object(arguments)) => Ok(arguments),
        _ => Err(UsageError(
            "tool arguments must be exactly one JSON object".to_owned(),
        )),
    }
}

fn config_target(profile: Option<String>) -> Result<UserConfigTarget, UsageError> {
    match profile {
        Some(name) => ProfileName::parse(&name)
            .map(UserConfigTarget::Profile)
            .map_err(|error| UsageError(format!("invalid config profile: {error}"))),
        None => Ok(UserConfigTarget::Base),
    }
}

fn managed_config_key(
    target: &UserConfigTarget,
    raw: &str,
) -> Result<ManagedConfigKey, UsageError> {
    let key = ManagedConfigKey::parse(raw)
        .map_err(|_| UsageError(format!("unknown config key `{raw}`")))?;
    if !key.available_in(target) {
        return Err(UsageError(format!(
            "config key `{key}` is not available for this target"
        )));
    }
    Ok(key)
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
        name: _,
        goal_operation: _,
    } = command;

    reject_ephemeral(flags.ephemeral)?;
    if flags.worktree.is_some() {
        return Err(UsageError(
            "--worktree creates a new session; it cannot resume one".into(),
        ));
    }

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

    let retry_default = if prompt.is_none() && flags.file.is_none() && answers.is_empty() {
        INTERACTIVE_PROVIDER_RETRIES
    } else {
        0
    };
    let mut args = into_args(RunCommand { prompt, flags }, retry_default)?;
    args.session = SessionChoice::Resume(id);
    args.answers = answers;
    Ok(args)
}

fn into_session_metadata_args(command: ResumeCommand) -> Result<SessionMetadataArgs, UsageError> {
    let id = command
        .id
        .as_deref()
        .expect("clap requires a metadata session id");
    let session = SessionId::from_str(id).map_err(|error| {
        UsageError(format!(
            "`{}` is not a session id: {error}. Metadata operations require an explicit id",
            id
        ))
    })?;
    let operation = match (command.name, command.goal_operation) {
        (Some(operation), None) => SessionMetadataOperation::Name(operation),
        (None, Some(operation)) => SessionMetadataOperation::Goal(operation),
        _ => unreachable!("clap requires exactly one metadata operation"),
    };
    Ok(SessionMetadataArgs {
        session,
        operation,
        session_dir: command.flags.common.places.session_dir,
        directory: command.flags.common.places.directory,
        output: command.flags.common.output.unwrap_or(Output::Text),
        profile: None,
    })
}

/// Interactive and ACP seats recover from transient failures that showed nothing.
pub const INTERACTIVE_PROVIDER_RETRIES: u32 = 6;

fn into_args(command: RunCommand, retry_default: u32) -> Result<Args, UsageError> {
    let RunCommand { prompt, flags } = command;

    reject_ephemeral(flags.ephemeral)?;
    if flags.vim && retry_default == 0 {
        return Err(UsageError(
            "--vim applies only to an interactive session".to_owned(),
        ));
    }

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

    ego_core::config::validate_prompt_options(flags.shape.bare, flags.shape.instructions.is_some())
        .map_err(|message| UsageError(message.to_owned()))?;

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

    // One conversion, so `ego run` and a served protocol cannot start
    // disagreeing about what these seven flags mean.
    let shape = SessionShape::from(flags.shape);

    Ok(Args {
        worktree: flags.worktree,
        read_only_dir: shape.read_only_dir,
        prompt,
        agent_file,
        attach: flags.attach,
        stdin,
        model: flags.model,
        offline: flags.offline,
        sandbox: flags.sandbox,
        thinking: flags.thinking,
        system: shape.system,
        session_prompt: None,
        append_system: shape.append_system,
        output_style: shape.output_style,
        instructions: shape.instructions,
        bare: shape.bare,
        trust_workspace: flags.trust_workspace,
        mode,
        allow: shape.allow,
        deny: shape.deny,
        answers: Vec::new(),
        session,
        session_dir: flags.common.places.session_dir,
        artifact_dir: flags.artifact_dir,
        output: flags.common.output.unwrap_or(Output::Text),
        output_explicit: flags.common.output.is_some(),
        schema: flags.schema,
        schema_retries: flags.schema_retries,
        retry: flags.retry.unwrap_or(retry_default),
        provider_recovery: retry_default != 0 && flags.retry.is_none(),
        fallbacks: flags.fallbacks,
        budget: flags.budget,
        max_output_tokens: flags.max_output_tokens,
        goal: flags.goal,
        goal_turns: flags.goal_turns,
        no_context_tool: flags.no_context_tool,
        context_clear: flags.context_clear,
        context_interval: flags.context_interval,
        goal_check_command: flags.goal_check_command.map(|command| command.0),
        max_turns: flags.max_turns,
        timeout: flags.timeout,
        tools: shape.tools,
        mcp: flags.mcp,
        directory: flags.common.places.directory,
        set: flags.set,
        profile_source: shape.profile_source,
        ceiling_profile: None,
        profile: shape.profile,
        quiet: flags.quiet,
        no_color: flags.no_color,
        plain: flags.plain,
        vim: flags.vim,
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

fn positive_duration(spec: &str) -> Result<Duration, String> {
    let seconds = duration_seconds(spec).map_err(|error| error.to_string())?;
    if seconds == 0 {
        return Err(format!(
            "`{spec}` is not a connection lifetime: the duration must be positive"
        ));
    }
    Ok(Duration::from_secs(seconds))
}

/// Parse and admit the endpoint spelling shared by inbound CLI protocols.
pub(crate) fn admit_endpoint(listen: &str, allow_public_bind: bool) -> Result<Admitted, Refusal> {
    let requested = match listen.parse() {
        Ok(address) => Requested::Tcp(address),
        Err(_) => Requested::Local {
            name: listen.to_owned(),
        },
    };
    admit(
        requested,
        if allow_public_bind {
            PublicBind::RequestedByOperator
        } else {
            PublicBind::Refused
        },
    )
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

#[cfg(test)]
mod permission_red {
    use super::*;

    #[test]
    fn managed_inputs_are_canonical_and_strictly_object_shaped() {
        let parsed = managed_rule("read(src/**)").expect("canonical rule");
        assert_eq!(
            parsed,
            ego_core::permit::rule::Rule::load("read(src/**)").unwrap()
        );
        assert!(managed_rule("read(").is_err());
        let id = managed_rule_id(parsed.id().to_string()).expect("known rule id");
        assert_eq!(id, parsed.id());
        assert!(managed_rule_id("r9:000000000000").is_err());
        assert!(managed_rule_id("not-a-rule-id").is_err());
        assert!(rule_arguments(r#"{"path":"src/main.rs"}"#).is_ok());
        assert!(rule_arguments("[\"not\",\"an\",\"object\"]").is_err());
    }
}

#[cfg(test)]
mod overview_red {
    use super::*;

    /// Catches a command listed with its own name as the description and a
    /// name longer than the fixed column running into its description.
    #[test]
    fn every_built_command_has_a_description_separated_from_its_name() {
        let text = overview("ego");
        for name in BUILT {
            let row = text
                .lines()
                .find(|line| line.trim_start().starts_with(name) && line.starts_with("  "))
                .unwrap_or_else(|| panic!("no row for {name}"));
            let rest = row
                .trim_start()
                .strip_prefix(name)
                .expect("row starts with the name");
            assert!(
                rest.starts_with("  "),
                "`{name}` is not separated from its description: {row:?}"
            );
            assert_ne!(rest.trim(), *name, "`{name}` describes itself: {row:?}");
            assert!(
                !rest.trim().is_empty(),
                "`{name}` has no description: {row:?}"
            );
        }
    }
}

#[cfg(test)]
mod root_access_mapping_tests {
    use super::*;

    /// Catches canonical root spellings drifting from the accepted CLI values.
    #[test]
    fn root_access_spellings_and_clap_values_agree() {
        use ego_core::permit::RootAccess;
        for (access, spelling) in [
            (RootAccess::Read, "read"),
            (RootAccess::ReadWrite, "read-write"),
        ] {
            assert_eq!(access.as_str(), spelling);
            let parsed =
                RootsCommand::try_parse_from(["roots", "add", "--access", spelling, "/tmp/root"]);
            assert!(parsed.is_ok(), "{spelling} must be accepted");
        }
        assert!(
            RootsCommand::try_parse_from(["roots", "add", "--access", "write", "/tmp/root"])
                .is_err()
        );
    }
}

/// Explicit operator-only filesystem restore. This never becomes prompt text.
#[derive(Debug, Clone, PartialEq, Eq, Parser)]
#[command(
    name = "ego checkpoint",
    about = "Preview or selectively restore session-owned Git file checkpoints."
)]
pub struct CheckpointArgs {
    #[arg(short = 'C', long = "directory", default_value = ".")]
    pub directory: PathBuf,
    pub session: SessionId,
    #[command(subcommand)]
    pub action: CheckpointAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum CheckpointAction {
    /// List checkpoint IDs, paths, effects, and external-change conflicts as JSON.
    Preview,
    /// Restore one selected checkpoint; external changes are never forced.
    Restore { checkpoint: String },
    /// Close checkpoint ownership and delete the session's private Git ref.
    Close,
}

#[cfg(test)]
mod checkpoint_contract {
    use super::*;

    #[test]
    fn checkpoint_is_an_operator_action_not_model_prompt_text() {
        // Catches: root dispatch treating a destructive operator action as a prompt.
        let session = SessionId::new();
        let parsed = parse(vec![
            "ego".into(),
            "checkpoint".into(),
            session.to_string(),
            "preview".into(),
        ])
        .unwrap();
        assert!(matches!(
            parsed,
            Invocation::Checkpoint(CheckpointArgs {
                action: CheckpointAction::Preview,
                ..
            })
        ));
        assert!(
            parse(vec![
                "ego".into(),
                "checkpoint".into(),
                session.to_string(),
                "restore".into()
            ])
            .is_err()
        );
        assert!(
            parse(vec![
                "ego".into(),
                "checkpoint".into(),
                session.to_string(),
                "restore".into(),
                "abc".into(),
                "--force".into()
            ])
            .is_err()
        );
    }
}
