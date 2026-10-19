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
    about = "Continue a stored session.",
    after_help = "With terminal input and output, no ID opens the saved-session picker across roots. Otherwise no ID selects the latest session in this workspace. Explicit --output, --answer, --file, stdin prompt '-' or profile ci select headless resumption.\n\nA single non-UUID positional is a prompt-only form, not a session ID. For example: ego resume \"next step\" (or ego continue \"next step\"). Supply a UUID plus a prompt to name a specific session.",
    disable_help_subcommand = true
)]
pub(super) struct ResumeCommand {
    /// Session UUID; omit it for the terminal picker or headless latest in this workspace.
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

    /// Read or change the session title without starting a turn: show, set=TEXT, clear.
    #[arg(long, value_parser = parse_name_operation, requires = "id", conflicts_with_all = ["goal_operation", "prompt", "answers", "resume-run-options"])]
    pub(super) name: Option<NameOperation>,

    /// Read or change goal metadata without starting a turn: show, set=TEXT, achieve, block=REASON.
    #[arg(long, value_parser = parse_goal_operation, requires = "id", conflicts_with_all = ["name", "prompt", "answers", "resume-run-options"])]
    pub(super) goal_operation: Option<GoalOperation>,

    #[command(flatten)]
    pub(super) flags: Flags,
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
    /// Hard spend ceiling, as for run.
    #[arg(long)]
    pub(super) budget: Option<String>,
    /// Maximum generated tokens per request.
    #[arg(long)]
    pub(super) max_output_tokens: Option<std::num::NonZeroU64>,
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
    /// Stored session ID to export.
    pub(super) source: String,

    /// Remove private content structurally; exports are unsanitized by default.
    #[arg(long)]
    pub(super) sanitize: bool,

    /// Transcript format; JSONL is stdout-only and cannot be combined with --file.
    #[arg(long, value_enum, default_value_t = ExportFormat::Md)]
    pub(super) format: ExportFormat,

    /// Follow committed local transcript events until stopped.
    #[arg(long)]
    pub(super) follow: bool,

    /// Only emit transcript events after this session-local sequence.
    #[arg(long, value_name = "SEQ")]
    pub(super) after: Option<u64>,

    /// Write to PATH instead of stdout; its parent must exist. Refuse symlinks,
    /// non-regular targets and existing files with different bytes; identical bytes are accepted.
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

    /// Maximum simultaneous listener connections (runtime default: 32).
    #[arg(long, requires = "listen")]
    pub(super) max_connections: Option<NonZeroU32>,

    /// Maximum lifetime of one listener connection: 90s, 10m or 2h (runtime default: 24h).
    #[arg(long, requires = "listen", value_parser = positive_duration)]
    pub(super) connection_lifetime: Option<Duration>,

    /// Maximum cumulative bytes read from one listener connection (runtime default: 134217728 bytes).
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
    /// Grant reads of one existing canonical absolute upload directory.
    #[arg(long, value_name = "DIR")]
    pub(super) read_only_dir: Option<PathBuf>,

    /// Replace the system prompt. `@file` reads it from a file.
    #[arg(long)]
    pub(super) system: Option<String>,

    /// No ambient discovery, even in a trusted workspace.
    #[arg(long, conflicts_with = "instructions")]
    pub(super) bare: bool,

    /// Append to the system prompt. `@file` reads it from a file.
    #[arg(long)]
    pub(super) append_system: Option<String>,

    /// Select a trusted prompt-only style: concise, explanatory, or NAME from EGO_HOME/output-styles/NAME.md.
    #[arg(long, value_name = "NAME")]
    pub(super) output_style: Option<String>,

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
/// `ARCHITECTURE.md`
#[derive(Debug, ClapArgs)]
pub(super) struct Ceiling {
    /// The weakest confinement a session may select: ro|workspace|adaptive|off; off = no confinement, credentials included.
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
    /// Inspect managed owner transaction capabilities as JSON.
    Capabilities,
    /// Read stored values and their opaque owner revision as JSON.
    Snapshot {
        #[arg(long, value_name = "NAME")]
        profile: Option<String>,
    },
    /// Atomically publish a bounded set of typed managed edits.
    Batch {
        #[arg(long, value_name = "NAME")]
        profile: Option<String>,
        #[arg(long = "set", value_name = "KEY=TOML_VALUE")]
        sets: Vec<String>,
        #[arg(long, value_name = "REVISION")]
        if_revision: Option<String>,
        #[arg(long = "unset", value_name = "KEY")]
        unsets: Vec<String>,
    },

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
        /// Show the effective run perimeter instead of stored values.
        #[arg(long)]
        effective: bool,
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
    /// Authorize a public OAuth client with an explicit registration strategy.
    Login {
        name: String,
        /// Operator-issued public client ID.
        #[arg(long, conflicts_with_all = ["cimd", "dcr"])]
        client_id: Option<String>,
        /// Published HTTPS client metadata document URL.
        #[arg(long, conflicts_with_all = ["client_id", "dcr"])]
        cimd: Option<String>,
        /// Explicitly use issuer-advertised dynamic registration.
        #[arg(long, conflicts_with_all = ["client_id", "cimd"])]
        dcr: bool,
        /// Loopback port registered with the authorization server (0 selects a free port).
        #[arg(long, default_value_t = 0)]
        callback_port: u16,
        /// Refresh an existing issuer/resource/account-bound credential without interaction.
        #[arg(long, requires = "issuer")]
        refresh: bool,
        #[arg(long, requires = "refresh")]
        issuer: Option<String>,
        #[arg(long, requires = "refresh")]
        account: Option<String>,
    },
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

    /// Explicit HTTP wire profile; never selected by fallback.
    #[arg(long, value_parser = ["current", "legacy-sse"], requires = "http", conflicts_with = "stdio")]
    pub(super) profile: Option<String>,

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
    #[arg(long, value_enum)]
    pub(super) output: Option<Output>,
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
    /// Create a retained managed worktree on this explicitly approved new branch.
    #[arg(long, value_name = "BRANCH", conflicts_with_all = ["session", "session_id"])]
    pub(super) worktree: Option<String>,
    /// Read the instruction and its invocation configuration from an agent file.
    #[arg(
        short = 'f',
        long = "file",
        value_name = "FILE",
        conflicts_with = "prompt"
    )]
    pub(super) file: Option<PathBuf>,

    /// Supply one declared agent-file template variable. Requires --file.
    #[arg(long = "var", value_name = "KEY=VALUE", action = ArgAction::Append, requires = "file", conflicts_with = "prompt")]
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

    /// Filesystem confinement: ro|workspace|adaptive|off; off = no confinement, credentials included.
    #[arg(long)]
    pub(super) sandbox: Option<Confinement>,

    /// off|minimal|low|medium|high|xhigh|max
    #[arg(long, value_parser = parse_thinking)]
    pub(super) thinking: Option<ThinkingLevel>,

    #[command(flatten)]
    pub(super) shape: Shape,

    /// Trust this launch's workspace without asking; remember the answer.
    #[arg(long, conflicts_with = "bare")]
    pub(super) trust_workspace: bool,

    /// Named session: created or attached.
    #[arg(long, group = "session-choice")]
    pub(super) session: Option<String>,

    /// A session id supplied by the caller.
    #[arg(long, group = "session-choice")]
    pub(super) session_id: Option<String>,

    /// Refused: every run uses a durable session.
    #[arg(long, group = "session-choice", hide = true)]
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
    #[arg(long)]
    pub(super) retry: Option<u32>,

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

    /// Maximum generated tokens per request.
    #[arg(long)]
    pub(super) max_output_tokens: Option<std::num::NonZeroU64>,
    /// Set a checked session objective.
    #[arg(long)]
    pub(super) goal: Option<String>,

    /// Disable model-requested context transitions.
    #[arg(long)]
    pub(super) no_context_tool: bool,

    /// Permit the model to discard conversation history using clear.
    #[arg(long)]
    pub(super) context_clear: bool,

    /// Minimum completed turns between model-requested transitions.
    #[arg(long, default_value = "10", value_parser = clap::value_parser!(u32).range(1..))]
    pub(super) context_interval: u32,

    /// Maximum automatic goal continuation turns.
    #[arg(long, default_value_t = 20)]
    pub(super) goal_turns: u32,

    /// Evaluator argv as a JSON string array; reads evidence JSON on stdin.
    #[arg(long, value_parser = parse_goal_check_command)]
    pub(super) goal_check_command: Option<GoalCheckCommand>,

    /// Cap the turns this run may take.
    #[arg(long)]
    pub(super) max_turns: Option<u32>,

    /// Wall clock, e.g. '90s' or '10m'.
    #[arg(long)]
    pub(super) timeout: Option<String>,

    /// Select an effective server by name, even one marked enabled = false, or attach a strict invocation-scoped JSON MCP server.
    #[arg(
        long,
        value_name = "NAME|@FILE",
        action = ArgAction::Append,
        value_parser = parse_mcp_selector
    )]
    pub(super) mcp: Vec<McpSelector>,

    /// Permission mode: plan|manual|auto|accept-edits|yolo (legacy: default, edits).
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

    /// Append plain output without terminal styling or live retry updates.
    #[arg(long)]
    pub(super) plain: bool,

    /// Use Vim normal/insert editing in the interactive composer.
    #[arg(long)]
    pub(super) vim: bool,
}

#[derive(Debug, Parser)]
pub(super) struct RootsCommand {
    #[command(subcommand)]
    pub(super) action: RootsSubcommand,
}

#[derive(Debug, Subcommand)]
pub(super) enum RootsSubcommand {
    /// Add or update one user perimeter root.
    Add {
        #[arg(long)]
        profile: Option<String>,
        #[arg(long, default_value = "read", value_enum)]
        access: RootAccessValue,
        path: PathBuf,
    },
    /// Remove a root by its stored spelling.
    Rm {
        #[arg(long)]
        profile: Option<String>,
        path: PathBuf,
    },
    /// List roots stored in the selected user target.
    Ls {
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

/// Clap adapter keeps the domain type independent of the CLI framework.
#[derive(Debug, Clone, Copy)]
pub(super) struct RootAccessValue(pub(super) ego_core::permit::RootAccess);

impl clap::ValueEnum for RootAccessValue {
    fn value_variants<'a>() -> &'a [Self] {
        &[
            Self(ego_core::permit::RootAccess::Read),
            Self(ego_core::permit::RootAccess::ReadWrite),
        ]
    }

    fn to_possible_value(&self) -> Option<clap::builder::PossibleValue> {
        Some(clap::builder::PossibleValue::new(self.0.as_str()))
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "ego setup",
    about = "Inspect expected work without running effects.",
    disable_help_subcommand = true
)]
pub(super) struct SetupCommand {
    /// Select proposal item IDs as one comma-separated list; repeat to add IDs.
    #[arg(long, value_name = "ID[,ID...]", value_delimiter = ',', num_args = 1, action = ArgAction::Append)]
    pub(super) items: Option<Vec<usize>>,
    /// Offer a validated judge for unresolved policy; never authorizes an action.
    #[arg(long)]
    pub(super) judge: Option<String>,
    pub(super) prompt: Option<String>,
    #[arg(long)]
    pub(super) profile: Option<String>,
    #[arg(long)]
    pub(super) directory: Option<PathBuf>,
}

/// One clap value containing the evaluator's entire argv, not repeated values.
#[derive(Debug, Clone)]
pub(super) struct GoalCheckCommand(pub(super) Vec<String>);

fn parse_goal_check_command(value: &str) -> Result<GoalCheckCommand, String> {
    let argv: Vec<String> = serde_json::from_str(value).map_err(|error| error.to_string())?;
    if argv.is_empty() || argv[0].trim().is_empty() {
        return Err("goal check command must name an executable".into());
    }
    Ok(GoalCheckCommand(argv))
}

/// Delete one stored session without taking a model turn.
#[derive(Debug, Parser)]
#[command(
    name = "ego rm",
    about = "Delete a stored session (history is retained for descendants).",
    disable_help_subcommand = true
)]
pub(super) struct RmCommand {
    /// Stored session ID to delete; history needed by descendants is retained.
    pub(super) source: String,
    /// Supply deletion confirmation without prompting; required without terminal stdin.
    #[arg(long)]
    pub(super) yes: bool,
    /// Acknowledge an uncertain prior run; does not replace deletion confirmation
    /// and is not a general override.
    #[arg(long)]
    pub(super) force: bool,
    #[command(flatten)]
    pub(super) places: Places,
}
