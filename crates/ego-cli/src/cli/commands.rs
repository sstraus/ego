//! Clap command declarations shared by parsing, help, and completions.

use super::*;

/// The clap definition. Kept separate from [`Args`] so that the shape the
/// rest of the binary reads is the shape §4.5 describes — a mutually
/// exclusive group is one field — rather than clap's flat set of options.
#[derive(Debug, Parser)]
#[command(
    name = "ego",
    about = "Run an agent from a prompt, headless.",
    disable_help_subcommand = true
)]
pub(super) struct RunCommand {
    /// The prompt. `-` reads it from stdin.
    pub(super) prompt: Option<String>,

    #[command(flatten)]
    pub(super) flags: Flags,
}

/// `ego resume [ID] [PROMPT]` — the same [`Flags`], against a session that
/// already exists.
#[derive(Debug, Parser)]
#[command(
    name = "ego resume",
    about = "Continue a session; no ID = latest in this workspace.",
    disable_help_subcommand = true
)]
pub(super) struct ResumeCommand {
    /// The session to continue. Omit it for the latest in this workspace.
    pub(super) id: Option<String>,

    /// The prompt. `-` reads it from stdin.
    pub(super) prompt: Option<String>,

    /// Settle a pending question. Repeat for multiple sibling questions.
    #[arg(
        long = "answer",
        value_name = "QUESTION_ID=JSON",
        action = ArgAction::Append,
        value_parser = parse_resume_answer
    )]
    pub(super) answers: Vec<ResumeAnswer>,

    #[command(flatten)]
    pub(super) flags: Flags,
}

/// The metadata-only `resume` grammar. No run flag is present by construction.
#[derive(Debug, Parser)]
#[command(
    name = "ego resume",
    about = "Read or change durable session metadata.",
    disable_help_subcommand = true
)]
pub(super) struct SessionMetadataCommand {
    pub(super) id: String,

    #[arg(long, value_parser = parse_name_operation, conflicts_with = "goal", required_unless_present = "goal")]
    pub(super) name: Option<NameOperation>,

    #[arg(long, value_parser = parse_goal_operation, conflicts_with = "name", required_unless_present = "name")]
    pub(super) goal: Option<GoalOperation>,

    #[command(flatten)]
    pub(super) common: Common,
}

/// `ego ls` — the catalogue.
#[derive(Debug, Parser)]
#[command(
    name = "ego ls",
    about = "List sessions (this workspace; --all for every one).",
    disable_help_subcommand = true
)]
pub(super) struct LsCommand {
    /// Every session in the store, not just this workspace's.
    #[arg(long)]
    pub(super) all: bool,

    #[command(flatten)]
    pub(super) common: Common,
}

/// `ego usage` — deterministic accounting for this workspace's local logs.
#[derive(Debug, Parser)]
#[command(
    name = "ego usage",
    about = "Report usage, context and cost for this workspace.",
    disable_help_subcommand = true
)]
pub(super) struct UsageCommand {
    /// all|Ns|Nm|Nh
    #[arg(
        long,
        default_value = "all",
        value_parser = parse_usage_period,
        allow_hyphen_values = true
    )]
    pub(super) period: UsagePeriod,

    /// Emit the exact versioned usage-report object.
    #[arg(long)]
    pub(super) json: bool,

    /// Keep role rows that recorded no activity in the text table.
    #[arg(long)]
    pub(super) all_roles: bool,

    #[command(flatten)]
    pub(super) places: Places,
}

/// `ego fork ID [--at ITEM]` — branch after a completed turn.
#[derive(Debug, Parser)]
#[command(
    name = "ego fork",
    about = "Fork a session at its tip or after the completed turn containing a message.",
    disable_help_subcommand = true
)]
pub(super) struct ForkCommand {
    pub(super) source: String,

    #[arg(long, value_name = "ITEM_ID")]
    pub(super) at: Option<String>,

    #[command(flatten)]
    pub(super) common: Common,
}

/// `ego compact ID [--request-id UUID]` — the ordinary command mints its
/// idempotency identity; an advanced caller may retain a known identity.
#[derive(Debug, Parser)]
#[command(
    name = "ego compact",
    about = "Compact a session into its deterministic successor.",
    disable_help_subcommand = true
)]
pub(super) struct CompactCommand {
    pub(super) source: String,

    #[arg(long = "request-id")]
    pub(super) request_id: Option<String>,

    #[command(flatten)]
    pub(super) common: Common,
}

/// `ego export ID` — no writer, provider, prompt, or host runtime.
#[derive(Debug, Parser)]
#[command(
    name = "ego export",
    about = "Export a session locally or as a structurally sanitized bundle.",
    disable_help_subcommand = true
)]
pub(super) struct ExportCommand {
    pub(super) source: String,

    #[arg(long)]
    pub(super) sanitize: bool,

    #[arg(long, value_enum, default_value_t = ExportFormat::Md)]
    pub(super) format: ExportFormat,

    #[arg(long, value_name = "PATH")]
    pub(super) file: Option<PathBuf>,

    #[command(flatten)]
    pub(super) places: Places,
}

#[derive(Debug, Parser)]
#[command(
    name = "ego models",
    about = "List the shared model catalogue.",
    disable_help_subcommand = true
)]
pub(super) struct ModelsCommand {
    /// Restrict enumeration and output to one admitted provider.
    #[arg(long)]
    pub(super) provider: Option<String>,

    /// Refresh the selected discoverable provider slices.
    #[arg(long)]
    pub(super) refresh: bool,

    /// Emit the exact versioned snapshot object.
    #[arg(long)]
    pub(super) json: bool,
}

#[derive(Debug, Parser)]
#[command(
    name = "ego hooks",
    about = "Inspect the admitted system and user hook catalogue.",
    disable_help_subcommand = true
)]
pub(super) struct HooksCommand {
    #[command(subcommand)]
    pub(super) action: HooksSubcommand,
}

#[derive(Debug, Parser)]
#[command(
    name = "ego doctor",
    about = "Report a read-only redacted health snapshot of this installation.",
    disable_help_subcommand = true
)]
pub(super) struct DoctorCommand {
    /// Emit the exact versioned health snapshot.
    #[arg(long)]
    pub(super) json: bool,
}

#[derive(Debug, Subcommand)]
pub(super) enum HooksSubcommand {
    /// List the admitted hook catalogue without changing configuration.
    Ls {
        /// Emit the exact versioned hook-listing object.
        #[arg(long)]
        json: bool,
    },
}

/// `ego acp` — the protocol, on stdio or one explicitly selected endpoint.
#[derive(Debug, Parser)]
#[command(
    name = "ego acp",
    about = "Serve the Agent Client Protocol on stdio or an endpoint.",
    disable_help_subcommand = true
)]
pub(super) struct AcpCommand {
    #[command(flatten)]
    pub(super) places: Places,

    #[command(flatten)]
    pub(super) ceiling: Ceiling,

    #[command(flatten)]
    pub(super) shape: Shape,

    /// Serve on a socket path or address instead of stdio.
    #[arg(long)]
    pub(super) listen: Option<String>,

    /// Serve on an address reachable from outside this machine.
    #[arg(long, requires = "listen")]
    pub(super) allow_public_bind: bool,

    /// Maximum simultaneous listener connections.
    #[arg(long, requires = "listen")]
    pub(super) max_connections: Option<NonZeroU32>,

    /// Maximum lifetime of one listener connection: 90s, 10m or 2h.
    #[arg(long, requires = "listen", value_parser = positive_duration)]
    pub(super) connection_lifetime: Option<Duration>,

    /// Maximum cumulative bytes read from one listener connection.
    #[arg(long, requires = "listen")]
    pub(super) max_ingress_bytes: Option<NonZeroU64>,
}

/// The general shape of a session: what the model may use, what it is allowed
/// to do, and what it is told about where it is working.
///
/// Its own struct rather than seven fields on `Flags`, because these questions
/// are not about `ego run`. They are about a *session*, and every surface that
/// opens one — a terminal run, a resumed run, a served protocol — answers them
/// the same way or answers them wrongly. Declaring them once is what stops
/// `ego acp` from growing a `--tools` that means something else.
#[derive(Debug, ClapArgs)]
pub(super) struct Shape {
    /// Append to the system prompt. `@file` reads it from a file.
    #[arg(long)]
    pub(super) append_system: Option<String>,

    /// Use this instruction file instead of discovering one.
    #[arg(long)]
    pub(super) instructions: Option<PathBuf>,

    /// Restrict the builtin tools to this comma-separated list.
    #[arg(long, value_delimiter = ',')]
    pub(super) tools: Option<Vec<String>>,

    /// Offer no tools at all.
    #[arg(long, conflicts_with = "tools")]
    pub(super) no_tools: bool,

    /// Allow rule, e.g. 'read(src/**)'. Repeatable.
    #[arg(long, value_name = "RULE", action = ArgAction::Append)]
    pub(super) allow: Vec<String>,

    /// Deny rule, e.g. 'write(**)'. Repeatable.
    #[arg(long, value_name = "RULE", action = ArgAction::Append)]
    pub(super) deny: Vec<String>,

    /// Layer [profile.NAME] from config.
    #[arg(long)]
    pub(super) profile: Option<String>,
}

/// The weakest confinement a served protocol's sessions may run at.
///
/// Its own struct rather than a field, because the ceiling belongs to the
/// process that serves runs and not to the protocol it happens to speak:
/// `ego a2a` is the same question and flattens the same two lines.
///
/// `docs/decisions/2026-09-07-a-host-picks-confinement-under-a-ceiling-it-cannot-raise.md`
#[derive(Debug, ClapArgs)]
pub(super) struct Ceiling {
    /// The weakest confinement a session may select: ro|workspace|off.
    #[arg(long)]
    pub(super) sandbox: Option<Confinement>,
}

/// `ego a2a` — the Agent2Agent protocol, on an endpoint.
///
/// `--listen` is required and has no default. Every other surface ego serves
/// speaks over stdio to a parent that already had the right to run it; this one
/// opens an endpoint, and an endpoint that appears because a default said so is
/// one nobody decided to open.
#[derive(Debug, Parser)]
#[command(
    name = "ego a2a",
    about = "Serve the Agent2Agent protocol on a local or loopback endpoint.",
    disable_help_subcommand = true
)]
pub(super) struct A2aCommand {
    #[command(flatten)]
    pub(super) places: Places,

    #[command(flatten)]
    pub(super) ceiling: Ceiling,

    /// Where to serve: a socket path, or an address such as `127.0.0.1:8080`.
    ///
    /// Anything that parses as an address is one; anything else is a local
    /// endpoint, which the operating system authenticates and which therefore
    /// needs no credential. A path is preferred for that reason.
    #[arg(long)]
    pub(super) listen: String,

    /// Serve on an address reachable from outside this machine.
    #[arg(long)]
    pub(super) allow_public_bind: bool,
}

/// `ego mcp-server` — the scoped run protocol, on stdio.
#[derive(Debug, Parser)]
#[command(
    name = "ego mcp-server",
    about = "Serve scoped ego runs over MCP on stdio.",
    disable_help_subcommand = true
)]
pub(super) struct McpServerCommand {
    #[command(flatten)]
    pub(super) places: Places,

    #[arg(long)]
    pub(super) max_concurrent_runs: NonZeroUsize,

    #[arg(long)]
    pub(super) retained_requests: NonZeroUsize,

    #[arg(long)]
    pub(super) max_resource_bytes: NonZeroUsize,
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
pub(super) struct AuthCommand {
    #[command(subcommand)]
    pub(super) action: AuthSubcommand,
}

#[derive(Debug, Subcommand)]
pub(super) enum AuthSubcommand {
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
pub(super) struct McpCommand {
    #[command(subcommand)]
    pub(super) action: McpSubcommand,
}

#[derive(Debug, Parser)]
#[command(
    name = "ego config",
    about = "Manage stored user configuration.",
    disable_help_subcommand = true
)]
pub(super) struct ConfigCommand {
    #[command(subcommand)]
    pub(super) action: ConfigSubcommand,
}

#[derive(Debug, Parser)]
#[command(
    name = "ego rules",
    about = "Inspect and manage persistent permission rules.",
    disable_help_subcommand = true
)]
pub(super) struct RulesCommand {
    #[command(subcommand)]
    pub(super) action: RulesSubcommand,
}

#[derive(Debug, Subcommand)]
pub(super) enum RulesSubcommand {
    /// List the effective system and selected user rules.
    #[command(name = "ls")]
    List {
        #[arg(long, value_name = "NAME")]
        profile: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Persist one allow predicate.
    Allow {
        #[arg(long, value_name = "NAME")]
        profile: Option<String>,
        rule: String,
    },
    /// Persist one deny predicate.
    Deny {
        #[arg(long, value_name = "NAME")]
        profile: Option<String>,
        rule: String,
    },
    /// Remove one predicate ID from both selected user lists.
    Revoke {
        #[arg(long, value_name = "NAME")]
        profile: Option<String>,
        id: String,
    },
    /// Derive persistent rules from one prepared builtin call.
    Remember {
        #[arg(long, value_name = "NAME")]
        profile: Option<String>,
        #[arg(short = 'C', value_name = "DIR")]
        directory: Option<PathBuf>,
        #[arg(long)]
        yes: bool,
        effect: String,
        tool: String,
        #[arg(value_name = "ARGS_JSON")]
        arguments: String,
    },
    /// Explain one prepared builtin call without running it.
    Explain {
        #[arg(long, value_name = "NAME")]
        profile: Option<String>,
        #[arg(short = 'C', value_name = "DIR")]
        directory: Option<PathBuf>,
        #[arg(long)]
        json: bool,
        tool: String,
        #[arg(value_name = "ARGS_JSON")]
        arguments: String,
    },
}

#[derive(Debug, Subcommand)]
pub(super) enum ConfigSubcommand {
    /// Read one exact stored value.
    Get {
        #[arg(long, value_name = "NAME")]
        profile: Option<String>,
        #[arg(long)]
        json: bool,
        key: String,
    },
    /// Store one strictly typed TOML value.
    Set {
        #[arg(long, value_name = "NAME")]
        profile: Option<String>,
        #[arg(value_name = "KEY=TOML_VALUE")]
        assignment: String,
    },
    /// Remove one exact stored value.
    Unset {
        #[arg(long, value_name = "NAME")]
        profile: Option<String>,
        key: String,
    },
    /// List the exact stored managed projection.
    #[command(name = "ls")]
    List {
        #[arg(long, value_name = "NAME")]
        profile: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Edit the complete user document in the selected external editor.
    Edit,
}

#[derive(Debug, Subcommand)]
pub(super) enum McpSubcommand {
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
pub(super) struct McpAddCommand {
    pub(super) name: String,

    /// Configure a subprocess transport. Its command follows literal `--`.
    #[arg(long, required_unless_present = "http", conflicts_with = "http")]
    pub(super) stdio: bool,

    /// Configure a Streamable HTTP transport.
    #[arg(
        long,
        value_name = "URL",
        required_unless_present = "stdio",
        conflicts_with = "stdio"
    )]
    pub(super) http: Option<String>,

    /// Working directory for the stdio child.
    #[arg(long, value_name = "DIR", requires = "stdio", conflicts_with = "http")]
    pub(super) cwd: Option<PathBuf>,

    /// Environment variable inherited by the stdio child. Repeatable.
    #[arg(
        long,
        value_name = "NAME",
        action = ArgAction::Append,
        requires = "stdio",
        conflicts_with = "http"
    )]
    pub(super) inherit_env: Vec<String>,

    /// HTTP authentication mode.
    #[arg(
        long,
        value_parser = ["none", "oauth"],
        requires = "http",
        conflicts_with = "stdio"
    )]
    pub(super) auth: Option<String>,

    /// Consent to plaintext HTTP for an exact loopback host.
    #[arg(long, requires = "http", conflicts_with = "stdio")]
    pub(super) allow_insecure_loopback: bool,
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
pub(super) struct Common {
    #[command(flatten)]
    pub(super) places: Places,

    /// text|json|jsonl
    #[arg(long, value_enum, default_value_t = Output::Text)]
    pub(super) output: Output,
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
pub(super) struct Places {
    /// Override the store location.
    #[arg(long)]
    pub(super) session_dir: Option<PathBuf>,

    /// Run as if started in DIR.
    #[arg(short = 'C', value_name = "DIR")]
    pub(super) directory: Option<PathBuf>,
}

#[derive(Debug, ClapArgs)]
pub(super) struct Flags {
    /// Read the instruction and its invocation configuration from an agent file.
    #[arg(short = 'f', long = "file", value_name = "FILE")]
    pub(super) file: Option<PathBuf>,

    /// Supply one declared agent-file template variable.
    #[arg(long = "var", value_name = "KEY=VALUE", action = ArgAction::Append)]
    pub(super) vars: Vec<String>,

    /// Add exact image bytes as ordered context before the prompt text.
    #[arg(
        long = "attach",
        value_name = "IMAGE_PATH|HTTPS_IMAGE_URL",
        action = ArgAction::Append
    )]
    pub(super) attach: Vec<String>,

    /// provider/model slug, e.g. ollama/qwen3
    #[arg(short = 'm', long)]
    pub(super) model: Option<String>,

    /// No non-loopback egress from ego's own HTTP transport.
    #[arg(long)]
    pub(super) offline: bool,

    /// Filesystem confinement: ro|workspace|off.
    #[arg(long)]
    pub(super) sandbox: Option<Confinement>,

    /// off|minimal|low|medium|high|xhigh|max
    #[arg(long, value_parser = parse_thinking)]
    pub(super) thinking: Option<ThinkingLevel>,

    /// Replace the system prompt. `@file` reads it from a file.
    #[arg(long)]
    pub(super) system: Option<String>,

    #[command(flatten)]
    pub(super) shape: Shape,

    /// No ambient discovery, even in a trusted workspace.
    #[arg(long)]
    pub(super) bare: bool,

    /// Named session: created or attached.
    #[arg(long, group = "session-choice")]
    pub(super) session: Option<String>,

    /// A session id supplied by the caller.
    #[arg(long, group = "session-choice")]
    pub(super) session_id: Option<String>,

    /// Refused: every run uses a durable session.
    #[arg(long, group = "session-choice")]
    pub(super) ephemeral: bool,

    #[command(flatten)]
    pub(super) common: Common,

    /// Override the sanitized run-artifact root for profile `ci`.
    #[arg(long)]
    pub(super) artifact_dir: Option<PathBuf>,

    /// JSON Schema the final answer must validate against; `@file` or inline.
    #[arg(long)]
    pub(super) schema: Option<String>,

    /// How many times to re-prompt on a schema mismatch.
    #[arg(long, default_value_t = 2)]
    pub(super) schema_retries: u32,

    /// Additional delivery-safe attempts for each candidate.
    #[arg(long, default_value_t = 0)]
    pub(super) retry: u32,

    /// Ordered provider/model candidate used after a delivery-safe failure.
    #[arg(
        long = "fallback",
        value_name = "P/M",
        action = ArgAction::Append,
        value_parser = parse_model_ref
    )]
    pub(super) fallbacks: Vec<ModelRef>,

    /// Hard cap: '2.5$' or '400k' tokens.
    #[arg(long)]
    pub(super) budget: Option<String>,

    /// Cap the turns this run may take.
    #[arg(long)]
    pub(super) max_turns: Option<u32>,

    /// Wall clock, e.g. '90s' or '10m'.
    #[arg(long)]
    pub(super) timeout: Option<String>,

    /// Attach a named effective or strict invocation-scoped JSON MCP server.
    #[arg(
        long,
        value_name = "NAME|@FILE",
        action = ArgAction::Append,
        value_parser = parse_mcp_selector
    )]
    pub(super) mcp: Vec<McpSelector>,

    /// Permission mode: plan|default|auto|edits|yolo.
    #[arg(long, value_parser = parse_mode, group = "mode-choice")]
    pub(super) mode: Option<Mode>,

    /// Shorthand for --mode plan.
    #[arg(long, group = "mode-choice")]
    pub(super) plan: bool,

    /// Shorthand for --mode yolo.
    #[arg(long, group = "mode-choice")]
    pub(super) yolo: bool,

    /// Dotted-path config override, value parsed as TOML.
    #[arg(short = 'c', long = "set", value_name = "K=V", action = ArgAction::Append)]
    pub(super) set: Vec<String>,

    /// Suppress progress and diagnostics on stderr; stdout is unaffected.
    #[arg(short = 'q', long)]
    pub(super) quiet: bool,

    /// Never emit ANSI colour, whatever the terminal claims to support.
    #[arg(long)]
    pub(super) no_color: bool,
}
