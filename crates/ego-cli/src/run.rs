//! One headless run, start to exit code.
//!
//! Everything this module decides, it decides once and hands to something
//! else: the exit code is `RunTermination::exit_code`, the projection is
//! `project_public`, the totals are `SessionState`'s. What is genuinely the
//! binary's own is the *order* — config, then instructions, then a model, then
//! a store, then turns — and the two things only a process can do: notice a
//! signal, and notice a deadline.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ego_core::config::{
    Effective, Limits, Mode, Posture, Selection, SystemPrompt, Trust, resolve, system_prompt,
};
use ego_core::cost::CurrencyCode;
use ego_core::event::domain::CancelCause;
use ego_core::event::id::SessionId;
use ego_core::generator::capabilities::ModelCapabilities;
use ego_core::permit::judge::{JudgeConfig, JudgeInput};
use ego_core::permit::{
    CanonPath, Change, Escalation, ExecCaps, ExecObservation, HumanApprover, Op, OpName,
    PermissionDecision, PermissionRequest, PolicyEngine, PreparedCall, RunConfinement, Tool,
};
use ego_core::public::{PUBLIC_EVENT_VERSION, PublicEvent};
use ego_core::run::{DeniedBy, RunTermination, Signal};
use ego_core::sink::EventSink;
use ego_core::state::{ItemKind, SessionState};
use ego_core::store::session_store::{SessionMeta, SessionStore};
use ego_core::turn::{
    CancelToken, JudgeModel, Model, Session, Tools, TurnConfig, TurnLoop, TurnOutcome,
};
use ego_tools::clock::SystemClock;
use ego_tools::exec::{ExecLimits, ExecRunConfig, GuardedExec, ProcessError};
use ego_tools::fs::OsFs;
use ego_tools::instructions::OsInstructionIo;
use ego_tools::store::{JsonlStore, SessionEvidenceStore};
use ego_tools::tools::builtins;
use serde_json::Value;

use crate::budget;
use crate::cli::{Args, Output, SessionChoice, UsageError};
use crate::output::{JsonlSink, Report, Stdout, TextSink, note};
use crate::render::caps::{Capabilities, Environment, Terminal};
use crate::render::osc::{Osc, Progress, State};
use crate::session::{self, Opened};
use crate::{config, facts, model, prompt, repl, schema};

/// The currency a session settles in.
///
/// One constant and not a flag, because `--currency` is not in `CLI.md` §3 and
/// a currency nobody can state is worse than a stated one: every quote this
/// build can produce is either zero (a local daemon) or supplied alongside its
/// own currency.
const SETTLEMENT: &str = "USD";

/// How long a cancelled turn is given to unwind before the run stops waiting.
///
/// `CancelToken` is checked before each generation and after each stream event,
/// so a provider that has gone silent delays cancellation until its transport
/// gives up. Waiting forever would make `--timeout` a suggestion; not waiting
/// at all would abandon an unsettled attempt, which is what `AttemptStream`
/// exists to prevent. A bounded wait is the only honest third answer.
const GRACE: Duration = Duration::from_secs(5);

/// Complete stdout or stderr beyond this per-stream bound fails the tool and
/// terminates its process tree. Preview memory is bounded separately by
/// [`ExecLimits`].
const EVIDENCE_CAP: u64 = 64 * 1024 * 1024;

/// What the run produced, beyond how it ended.
pub struct Ended {
    pub termination: RunTermination,
    pub session: SessionId,
    pub state: SessionState,
    /// The final answer: the validated document under `--schema`, the last
    /// assistant message otherwise, and the empty string when there was none.
    pub result: Value,
}

impl Ended {
    /// A run that ended before a session existed.
    fn refused(termination: RunTermination) -> Self {
        Ended {
            termination,
            session: SessionId::new(),
            state: SessionState::default(),
            result: Value::String(String::new()),
        }
    }
}

/// Run `args` to completion, publishing where `--output` says, and say how it
/// ended.
pub async fn run(args: Args, out: &Stdout, cancel: CancelToken) -> Ended {
    let output = args.output;
    let quiet = args.quiet;
    // The one place the terminal is asked what it is. §4.5's "stdout is
    // protocol-clean" is why the machine-readable modes are not asked at all
    // rather than asked and ignored: there is no environment in which a JSON
    // document acquires an escape sequence.
    let environment = Environment::read();
    let capabilities = match output {
        Output::Text => Capabilities::of(&Terminal::from_process(args.no_color, &environment)),
        Output::Json | Output::Jsonl => Capabilities::plain(),
    };
    let text_sink = TextSink::new(out, quiet, capabilities);
    let jsonl_sink = JsonlSink::new(out);
    let sink: &dyn EventSink = match output {
        Output::Text => &text_sink,
        Output::Jsonl => &jsonl_sink,
        // §4.1 is a single object at completion, so nothing is published as it
        // happens — but the run still needs a sink, and one that writes
        // nothing is the honest one rather than a collector nobody reads.
        Output::Json => &ego_core::sink::NullSink,
    };

    let announce = Announce::new(out, capabilities, quiet);
    announce.opening(args.prompt.as_deref());

    let ended = run_with(args, |_| sink, cancel).await;

    // Draws the last block and closes the last line. A no-op unless text mode
    // has something outstanding, which is why it can run on the refusal path
    // too: a run that refused before the first token held nothing back.
    if output == Output::Text {
        text_sink.finish();
    }
    announce.closing(&ended.termination);
    ended
}

/// Talk to the model until the person stops, then say how it ended.
///
/// The mirror of [`run`] for a session driven from a keyboard: one resolution,
/// one session, one lease, many turns.
pub async fn interactive(args: Args, out: &Stdout, cancel: CancelToken) -> Ended {
    // A machine-readable stream has no reader here. `--output json` is a
    // single document at completion and a conversation has no completion, so
    // the flag is refused by name rather than quietly producing a stream
    // nobody can consume — `CLI.md` §4.5.
    if args.output != Output::Text {
        return Ended::refused(RunTermination::UsageError {
            message: format!(
                "`--output {}` has no meaning for an interactive session: it is \
                 one document at completion, and a conversation does not have \
                 one. Use `ego run --output {}` for a headless one-shot",
                args.output, args.output
            ),
        });
    }

    let quiet = args.quiet;
    let environment = Environment::read();
    let capabilities = Capabilities::of(&Terminal::from_process(args.no_color, &environment));
    let text_sink = TextSink::new(out, quiet, capabilities);

    let seat: Arc<dyn InteractiveSeat> = Arc::new(repl::Seat::new());
    let announce = Announce::new(out, capabilities, quiet);

    let ended = converse(
        args,
        |_| &text_sink as &dyn EventSink,
        cancel,
        seat,
        // Between turns: draw the block the renderer was holding back, and
        // close the line, so the next prompt starts on a row of its own.
        &|| text_sink.finish(),
    )
    .await;

    announce.closing(&ended.termination);
    ended
}

/// Everything ego tells the host that is *not* the answer: the marks, the
/// progress indicator, the window title, the notification.
///
/// It lives beside the run rather than inside the sink because none of it is a
/// projection of an event. "This process started producing output" and "it
/// exited 3" are facts about the *process*, and the process is what this module
/// is. The sink publishes what happened in the session; this publishes what
/// happened to the program.
///
/// Everything here is silent unless [`Capabilities`] found a screen, so no arm
/// needs to ask again — see [`crate::render::osc`].
///
/// ## What is deliberately not here
///
/// **OSC 7 (working directory)** and **OSC 133 A (prompt start)**. Both are
/// correct for a process that *occupies* the pane and wrong for one that
/// borrows it: OSC 7 tells the terminal where the shell is, and `ego -C sub`
/// announcing `sub` would leave the pane pointing at a directory the shell
/// returns from the moment ego exits. They belong to the REPL, which is also
/// where a prompt to mark exists at all.
struct Announce<'a> {
    out: &'a Stdout,
    osc: Osc,
    quiet: bool,
    started: std::time::Instant,
}

/// How long a run has to take before finishing it is worth a desktop
/// notification.
///
/// A notification for a run the operator watched finish is an interruption
/// reporting something they already know. A minute is the point past which
/// they have plausibly looked at something else — and `-q`, which `CLI.md` §1.3
/// calls quiet progress, suppresses it at any duration.
const NOTIFY_AFTER: Duration = Duration::from_secs(60);

impl<'a> Announce<'a> {
    fn new(out: &'a Stdout, capabilities: Capabilities, quiet: bool) -> Self {
        Announce {
            out,
            osc: Osc::new(capabilities),
            quiet,
            started: std::time::Instant::now(),
        }
    }

    fn opening(&self, prompt: Option<&str>) {
        self.emit(self.preamble(prompt));
    }

    fn closing(&self, termination: &RunTermination) {
        self.emit(self.postamble(termination, self.started.elapsed()));
    }

    /// Write a sequence, or nothing at all.
    ///
    /// The empty check is not an optimisation: [`Stdout::write`] flushes, and a
    /// run in `--output json` would otherwise flush for the privilege of
    /// writing zero bytes.
    fn emit(&self, sequence: String) {
        if !sequence.is_empty() {
            self.out.write(&sequence);
        }
    }

    /// One string rather than four writes, so a pipe that breaks mid-preamble
    /// cannot leave half of an escape sequence in the stream — and so the whole
    /// announcement is a value a test can read.
    fn preamble(&self, prompt: Option<&str>) -> String {
        [
            self.osc.title(&title(prompt)),
            self.osc.state(State::Busy),
            // Indeterminate rather than a percentage: the number of turns is
            // not known in advance, and a bar that advances on a guess is a lie
            // with a progress bar's authority. `--budget` would make a fraction
            // meaningful; that is the story that gives `Progress::At` a caller.
            self.osc.progress(Progress::Indeterminate),
            self.osc.output_start(),
        ]
        .concat()
    }

    fn postamble(&self, termination: &RunTermination, elapsed: Duration) -> String {
        let code = termination.exit_code();
        let notification = if self.quiet || elapsed < NOTIFY_AFTER {
            String::new()
        } else {
            self.osc.notify(
                crate::DISPLAY_NAME,
                &format!("{} after {}", termination.exit_reason(), duration(elapsed)),
            )
        };
        [
            // Cleared on the way out on *every* path, including failure: an
            // indicator left set is a tab that stays orange until the terminal
            // is restarted, and the exit status is already carried by 133 D.
            self.osc.progress(if code == 0 {
                Progress::Cleared
            } else {
                Progress::Failed
            }),
            notification,
            self.osc.state(State::Idle),
            // Reduced to the fixed brand prefix rather than restored. Restoring
            // needs the previous title, which is only readable through a query
            // (OSC 21) that half the matrix answers and the other half echoes
            // onto the screen. `Osc::title` owns the invariant that even this
            // empty dynamic title still emits the fixed prefix; the shell
            // overwrites it at its next prompt.
            self.osc.title(""),
            // Last, because it is the mark that says everything above it
            // belongs to this command.
            self.osc.output_end(i32::from(code)),
        ]
        .concat()
    }
}

/// The window title for a run: what it was asked, short enough to read in a
/// tab.
///
/// Measured in cells rather than bytes, like everything else that reaches a
/// terminal — a title cut mid-grapheme is how a tab ends up with a replacement
/// character in it.
fn title(prompt: Option<&str>) -> String {
    const CELLS: usize = 48;
    let Some(prompt) = prompt.map(str::trim).filter(|text| !text.is_empty()) else {
        return crate::DISPLAY_NAME.to_string();
    };
    // `-` is the spelling for "read the prompt from stdin", so it is the one
    // prompt that says nothing about the run.
    if prompt == "-" {
        return crate::DISPLAY_NAME.to_string();
    }
    let first = prompt.lines().next().unwrap_or_default();
    let short = crate::render::Line::plain(first).truncate_to(CELLS);
    short.text().trim_end().to_string()
}

/// A duration a person reads, not one a machine parses.
fn duration(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds < 60 {
        return format!("{seconds}s");
    }
    format!("{}m {}s", seconds / 60, seconds % 60)
}

/// Run `args` to completion, publishing through the sink `destination` builds.
///
/// The destination of the event stream is a *parameter* of a run, not a
/// consequence of `--output`. [`run`] derives one from the flag because that is
/// what the flag is for; `ego acp` supplies one that turns each event into a
/// `session/update` notification, and it is the same run underneath — one
/// implementation of a turn, two places its events go.
///
/// **A function of the resolved model, not a value.** ACP's `UsageUpdate`
/// reports occupancy as a fraction of the context window, and the window is a
/// fact about the model — which is resolved *inside* this run, after the store
/// is chosen and before the first event. A caller that had to supply a ready
/// sink would have to resolve the model itself first, which is the same
/// resolution done twice with two chances to disagree. `destination` is
/// therefore called once, at the point the answer exists. A caller that does
/// not care ignores the argument: `|_| sink`.
pub async fn run_with<S: EventSink>(
    args: Args,
    destination: impl FnOnce(&ModelCapabilities) -> S,
    cancel: CancelToken,
) -> Ended {
    match execute(args, destination, cancel, Driver::Once).await {
        Ok(ended) => ended,
        Err(refusal) => Ended::refused(refusal),
    }
}

/// The same run, driven by a person instead of by argv.
///
/// Everything before the first turn is identical — one resolution, one
/// session, one lease — which is why this is a driver and not a second
/// pipeline. `session::open` replays the log from disk, so opening one per
/// typed line would make a conversation get slower the longer it went on.
pub async fn converse<S: EventSink>(
    args: Args,
    destination: impl FnOnce(&ModelCapabilities) -> S,
    cancel: CancelToken,
    seat: Arc<dyn InteractiveSeat>,
    between: &(dyn Fn() + Send + Sync),
) -> Ended {
    match execute(
        args,
        destination,
        cancel,
        Driver::Interactive { seat, between },
    )
    .await
    {
        Ok(ended) => ended,
        Err(refusal) => Ended::refused(refusal),
    }
}

/// The deadline, if `--timeout` set one.
///
/// Parsed here rather than by a duration crate: `CLI.md` gives two spellings
/// ('90s', '10m') and a permissive parser would accept a third nobody
/// documented.
fn deadline(spec: &str) -> Result<Duration, UsageError> {
    let refuse = || {
        UsageError(format!(
            "`{spec}` is not a duration: write it as '90s', '10m' or '2h'"
        ))
    };
    let spec = spec.trim();
    let (digits, unit) = spec.split_at(spec.len().saturating_sub(1));
    let scale = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        _ => return Err(refuse()),
    };
    let count: u64 = digits.parse().map_err(|_| refuse())?;
    count
        .checked_mul(scale)
        .map(Duration::from_secs)
        .ok_or_else(refuse)
}

/// The workspace this run is rooted at.
///
/// `-C DIR` is applied to the process before anything else reads a path, so
/// there is exactly one notion of "here" and every relative path in every
/// later step agrees with it.
pub(crate) fn workspace(directory: Option<&PathBuf>) -> Result<PathBuf, UsageError> {
    if let Some(dir) = directory {
        std::env::set_current_dir(dir)
            .map_err(|err| UsageError(format!("-C {}: {err}", dir.display())))?;
    }
    std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .map_err(|err| {
            UsageError(format!(
                "the working directory could not be resolved: {err}"
            ))
        })
}

/// `--instructions FILE`, resolved against this workspace.
///
/// Two things happen here and neither is cosmetic.
///
/// **The directory is resolved and the file name deliberately is not.**
/// `Selection::Explicit` reaches `config::plan` verbatim, and that module
/// states its inputs must already be canonical — `root` and `cwd` are, because
/// [`workspace`] canonicalises them, and this path was not. A relative
/// `AGENTS.md` therefore reached `scope_of` with an empty parent and published
/// `scope: ""`, while an absolute one published the host's own directory into
/// a stream `InstructionDescriptor`'s doc says a CI job archives. `"."` is the
/// documented answer for both. Resolving only the parent leaves the final
/// component untouched, so a symlinked file still arrives at the loader as a
/// symlink and is still refused by name instead of quietly followed.
///
/// **A file that is not there refuses the run.** `CLI.md` §4.4: invalid config
/// is never skipped silently. The operator named this file; proceeding without
/// it ignores an instruction that was given, which is the silently vacuous
/// flag `--budget` was fixed for one flag over — and `--system @file` already
/// exits 2 for the same mistake. `symlink_metadata` rather than `exists`,
/// because a *dangling* symlink must reach the loader and be refused as the
/// symlink it is, not reported here as an absence.
fn explicit(path: &Path, cwd: &Path) -> Result<PathBuf, UsageError> {
    let refuse =
        |err: std::io::Error| UsageError(format!("--instructions {}: {err}", path.display()));
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let name = absolute.file_name().ok_or_else(|| {
        UsageError(format!(
            "--instructions {}: that is a directory, not an instruction file",
            path.display()
        ))
    })?;
    let resolved = std::fs::canonicalize(absolute.parent().unwrap_or(cwd))
        .map_err(refuse)?
        .join(name);
    std::fs::symlink_metadata(&resolved).map_err(refuse)?;
    Ok(resolved)
}

/// The instructions and system prompt this run generates under.
///
/// **`--bare`, and a headless run in an untrusted workspace, call
/// `InstructionIo` zero times.** Not "read nothing" — make no call at all, so
/// nothing is stat'd, opened or read, and there is nothing for a dialog to be
/// raised about. That is the mechanism behind §4.4, and it is
/// `Selection::Disabled` doing it, one layer down.
fn instructions(
    effective: &Effective,
    args: &Args,
    root: &Path,
    cwd: &Path,
) -> Result<Option<SystemPrompt>, UsageError> {
    if let Some(text) = &args.system {
        // `--system` replaces the prompt outright (`CLI.md` §3), so no file
        // contributed and none is published. `--append-system` still applies:
        // "append to it" means to whatever `--system` left.
        let mut text = prompt::at_file(text)?;
        if let Some(extra) = &args.append_system {
            text.push_str("\n\n");
            text.push_str(&prompt::at_file(extra)?);
        }
        return Ok(Some(SystemPrompt::verbatim(text)));
    }

    let named = match &args.instructions {
        Some(path) => Some(explicit(path, cwd)?),
        None => None,
    };
    let selection = Selection::of(effective.posture, named);
    let set = ego_core::config::load(&OsInstructionIo, root, cwd, &selection, Limits::default())
        .map_err(|err| UsageError(err.to_string()))?;

    for skipped in &set.skipped {
        note(&format!(
            "{} was not used: {}",
            skipped.path.display(),
            skipped.reason
        ));
    }

    let mut assembled = system_prompt(&set, root, &selection);
    if let Some(extra) = &args.append_system {
        let extra = prompt::at_file(extra)?;
        assembled = Some(match assembled {
            Some(mut prompt) => {
                prompt.text.push_str("\n\n");
                prompt.text.push_str(&extra);
                prompt
            }
            None => SystemPrompt::verbatim(extra),
        });
    }
    Ok(assembled)
}

/// The tools this run offers the model.
///
/// `--tools` names a subset and `--no-tools` names the empty one; a name that
/// is not a builtin is refused rather than ignored, because a run that
/// silently offered fewer tools than it was told to is a run whose failure has
/// no visible cause.
fn tools(selected: Option<&Vec<String>>) -> Result<Vec<Arc<dyn Tool>>, UsageError> {
    let all = builtins();
    let Some(names) = selected else {
        return Ok(all);
    };
    for name in names {
        if !all
            .iter()
            .any(|tool| tool.descriptor().name == name.as_str())
        {
            let known: Vec<String> = all.iter().map(|tool| tool.descriptor().name).collect();
            return Err(UsageError(format!(
                "--tools {name}: there is no such builtin; this build has {}",
                known.join(", ")
            )));
        }
    }
    Ok(all
        .into_iter()
        .filter(|tool| names.contains(&tool.descriptor().name))
        .collect())
}

/// The output ceiling the request carries.
///
/// `BudgetGate::reserve` needs one from somewhere: with neither the model nor
/// the request stating a cap it refuses with `UnstatedCeiling { Output }`,
/// which is right — a generation with no bound on its output has no bound on
/// its price. Ollama states none (`ollama.rs`: "the daemon states no output
/// ceiling, so the request must cap itself"), so this is the ordinary path for
/// every local run rather than an edge case, and there is no `CLI.md` §3 flag
/// to supply it with.
///
/// The served context window is the answer, because it is a bound rather than
/// a guess: no generation can emit more output tokens than fit in the window it
/// is generating into. It over-reserves — the prompt already occupies part of
/// that window — and over-reserving is the safe direction for a gate whose only
/// failure that matters is authorising too much.
fn output_ceiling(caps: &ModelCapabilities) -> Option<u64> {
    caps.max_output_tokens.or(caps.max_context_tokens)
}

/// The capability names `session.started` publishes.
///
/// Feature detection by name (`CLI.md` §4.2), so a consumer never parses a
/// version — and by *observation*, not by a hand-written list: a capability
/// this build does not have must not be advertised, and the cheapest way to
/// keep that true is to derive each one from the thing that provides it.
fn capabilities(tools: &[Arc<dyn Tool>], instructions: bool, schema: bool) -> Vec<String> {
    let mut names = Vec::new();
    if !tools.is_empty() {
        names.push("tools".to_string());
    }
    if instructions {
        names.push("instructions".to_string());
    }
    if schema {
        names.push("schema".to_string());
    }
    names.push("jsonl".to_string());
    names
}

/// Everything the command line and the config layers settle before this run
/// touches a store, a provider, or a file of its own.
struct Settings {
    /// When the run began. `--timeout` is measured from here rather than from
    /// the first turn, because the time a model took to resolve is time the
    /// operator spent waiting for this command.
    started: std::time::Instant,
    timeout: Option<Duration>,
    /// The workspace root, and the directory relative paths resolve against.
    /// The same path today — [`workspace`] makes the root *be* the working
    /// directory — and two fields because they answer two questions.
    root: PathBuf,
    cwd: PathBuf,
    /// The effective runtime boundary, resolved independently of permission
    /// mode and fixed for the whole run.
    confinement: RunConfinement,
    effective: Effective,
}

/// The command line and the config layers, resolved into one answer.
fn settings(args: &Args, trust_for: impl FnOnce(&Path) -> Trust) -> Result<Settings, UsageError> {
    let started = std::time::Instant::now();
    let timeout = match args.timeout.as_deref() {
        Some(spec) => Some(deadline(spec)?),
        None => None,
    };

    let cwd = workspace(args.directory.as_ref())?;
    // The workspace root is the working directory. Discovering a repository
    // root is `git`'s question and would make the answer depend on a tool that
    // may not be installed; `-C` is how a caller says otherwise.
    let root = cwd.clone();

    // The caller can return `Trusted` only when it owns a human seat. A
    // headless caller passes a closure that always returns `Untrusted`, so no
    // terminal or stdin read is reachable from that path.
    let trust = trust_for(&root);
    let layers = config::layers(&root, args, trust)?;
    let effective = resolve(&layers, trust, config::profile(args).as_deref())
        .map_err(|err| UsageError(err.to_string()))?;
    if trust == Trust::Untrusted || args.bare {
        debug_assert_eq!(effective.posture, Posture::Bare);
    }

    // §4 requires the clamp be stated, not silent: a `--budget 20$` reduced to
    // an administrator's $5 must not read as binding at $20. The money half
    // travels into the gate, which publishes it on every `MeteringSelected`;
    // the token half has no such carrier, so it is said here.
    if let Some(asked) = effective.clamped.tokens {
        note(&format!(
            "the token ceiling of {asked} was reduced to {} by a system ceiling",
            effective
                .ceilings
                .tokens
                .expect("a clamp leaves a ceiling in force")
        ));
    }

    let confinement = RunConfinement::new(
        effective.sandbox.unwrap_or_default(),
        args.offline,
        CanonPath::new(&root),
    )
    .map_err(|err| UsageError(err.to_string()))?;

    Ok(Settings {
        started,
        timeout,
        root,
        cwd,
        confinement,
        effective,
    })
}

/// What a turn is made of: the prompt, its system half, the tools on offer,
/// the schema the answer must satisfy, and the name of the model to ask.
struct Materials {
    system: Option<SystemPrompt>,
    registry: Vec<Arc<dyn Tool>>,
    compiled: Option<schema::Schema>,
    text: String,
    slug: String,
}

/// Everything a turn needs that comes from a file, a flag or stdin.
///
/// Gathered before a store is chosen and before a model is resolved, so a run
/// naming a schema that will not compile or a tool this build does not have
/// refuses without creating a session or reaching a network.
fn materials(settings: &Settings, args: &Args, seeded: bool) -> Result<Materials, UsageError> {
    let compiled = match args.schema.as_deref() {
        Some(spec) => Some(schema::Schema::compile(&prompt::at_file(spec)?)?),
        None => None,
    };
    let system = instructions(&settings.effective, args, &settings.root, &settings.cwd)?;
    let registry = tools(args.tools.as_ref())?;
    // An interactive session may start with nothing to ask; a one-shot may
    // not. Same table otherwise.
    let text = if seeded {
        prompt::seed(args.prompt.as_deref(), args.stdin, &prompt::ProcessStdin)?
    } else {
        prompt::assemble(args.prompt.as_deref(), args.stdin, &prompt::ProcessStdin)?
    };
    let slug = args
        .model
        .clone()
        .or_else(|| settings.effective.model.clone().map(|model| model.0))
        .ok_or_else(|| {
            UsageError(
                "no model: pass --model provider/model, set EGO_MODEL, or put \
                 `model = \"…\"` in a config file"
                    .to_string(),
            )
        })?;

    Ok(Materials {
        system,
        registry,
        compiled,
        text,
        slug,
    })
}

/// The store this run logs through, and which session in it.
struct Chosen {
    /// `None` under `--ephemeral`: a run that keeps nothing needs no store.
    store: Option<JsonlStore>,
    id: SessionId,
}

/// *Which* session this run continues or creates — not the opening of it.
///
/// The two halves are separated on purpose, and this is the half that happens
/// before the model resolves. Choosing is a read-only catalogue query and it
/// can refuse: `ego resume` in a directory that owns no session is a usage
/// error about the command as typed, and it would be true whatever the
/// provider said. Answering it *after* the model resolved told an operator
/// standing in the wrong directory that the network was down. Opening stays in
/// [`execute`], because that takes a lease and can create, and a session must
/// not be created for a run whose model does not resolve.
fn chosen(args: &Args, root: &Path) -> Result<Chosen, UsageError> {
    let store_root = match args.session {
        SessionChoice::Ephemeral => None,
        _ => Some(session::store_root(args.session_dir.as_ref())?),
    };
    // The same `root` that derives a named session's id below, so "the session
    // called build in this workspace" and "the sessions belonging to this
    // workspace" cannot disagree about which workspace that is.
    let store = store_root.map(|dir| session::jsonl(dir, root.to_path_buf()));
    let id = session::id_for(
        &args.session,
        root,
        store.as_ref().map(|store| store as &dyn SessionStore),
    )?;

    Ok(Chosen { store, id })
}

/// The provider this run generates through.
struct Provider {
    http: Arc<dyn ego_ai::http::Http>,
    resolved: ego_ai::catalogue::ResolvedModel,
    credential: ego_ai::provider::ResolvedCredential,
}

struct ResolvedJudge {
    generator: ego_ai::generator::WireGenerator,
    resolved: ego_ai::catalogue::ResolvedModel,
    config: JudgeConfig,
}

/// Prove the fixed judge generation envelope before a session exists.
///
/// Dynamic authority facts occupy one user-text block and cannot change the
/// request's tool, thinking, parallelism or output-ceiling requirements. A
/// canonical write-shaped input therefore exercises the same provider-facing
/// shape without sending a generation or pretending that the provider was
/// contacted. The exact in-turn request is checked again by `TurnLoop`.
fn preflight_judge(
    judge: &ResolvedJudge,
    confinement: &RunConfinement,
    exec_caps: ExecCaps,
) -> Result<(), RunTermination> {
    let path = confinement.workspace().clone();
    let call = PreparedCall {
        tool: "write".to_string(),
        ops: vec![Op::Write {
            path: path.clone(),
            change: Change::Whole(Vec::new()),
        }],
    };
    let escalation = Escalation::ModeAsks {
        mode: Mode::Auto,
        op: OpName::Write,
        path,
    };
    let request =
        JudgeInput::new(&call, confinement, exec_caps, &escalation).request(&judge.config);
    let gaps = judge.resolved.capabilities.request_gaps(&request);
    if !gaps.is_empty() {
        return Err(RunTermination::UsageError {
            message: format!(
                "judge model `{}` is incompatible with the fixed judge request: {}",
                judge.config.identity.0.as_str(),
                gaps.iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        });
    }
    judge
        .generator
        .ensure_expressible(&request)
        .map_err(|error| RunTermination::UsageError {
            message: format!(
                "judge model `{}` cannot encode the fixed judge request: {error}",
                judge.config.identity.0.as_str()
            ),
        })
}

/// A transport, the catalogue row `--model` names, and the credential that
/// will sign for it.
///
/// The two ways this fails are two exit codes, which is what
/// `is_provider_failure` is asked: a slug nobody has heard of is the operator's
/// mistake (exit 2) and a daemon that is not listening is not (exit 4), and one
/// answer covering both would tell half of them the wrong thing.
async fn provider(
    slug: &str,
    effective: &Effective,
    offline: bool,
) -> Result<Provider, RunTermination> {
    let provider_id = slug
        .split_once('/')
        .map(|(provider, _)| provider)
        .unwrap_or(slug);
    model::ensure_provider_mapping(provider_id).map_err(|err| RunTermination::UsageError {
        message: err.to_string(),
    })?;
    let http = model::transport(offline).map_err(|err| RunTermination::Provider {
        message: err.to_string(),
    })?;
    let hosted =
        facts::supplied(&effective.ceilings).map_err(|err| RunTermination::UsageError {
            message: err.to_string(),
        })?;
    // One credential source for the whole request: refresh, resolution and
    // header assembly share the locked auth store selected by `ego auth`.
    let creds = crate::auth::request_credentials(
        provider_id,
        effective
            .auth
            .get(provider_id)
            .and_then(|auth| auth.credential_command.as_deref()),
        offline,
    )
    .await
    .map_err(|err| RunTermination::UsageError {
        message: err.to_string(),
    })?;
    let (resolved, credential) =
        model::resolve(slug, hosted, &http, creds.as_ref(), &effective.auth)
            .await
            .map_err(|err| {
                let message = err.to_string();
                if model::is_provider_failure(&err) {
                    RunTermination::Provider { message }
                } else {
                    RunTermination::UsageError { message }
                }
            })?;

    Ok(Provider {
        http,
        resolved,
        credential,
    })
}

/// Build the process boundary for one cancellation scope.
///
/// A backend that cannot demonstrate a requested guarantee is a startup usage
/// refusal (exit 2), not an internal error and never a silent downgrade.
async fn guarded_exec(
    confinement: &RunConfinement,
    evidence: &SessionEvidenceStore,
    cancel: &CancelToken,
) -> Result<(Arc<GuardedExec>, ExecObservation), RunTermination> {
    let config = ExecRunConfig::new(
        confinement.clone(),
        evidence.clone(),
        cancel.clone(),
        ExecLimits::default(),
    );
    GuardedExec::for_run(config)
        .await
        .map_err(|err| match &err {
            ProcessError::MissingCapabilities(_) | ProcessError::Backend(_) => {
                RunTermination::UsageError {
                    message: err.to_string(),
                }
            }
            ProcessError::Cancelled => cancelled_termination(cancel),
            _ => RunTermination::Failed {
                message: err.to_string(),
            },
        })
}

fn cancelled_termination(cancel: &CancelToken) -> RunTermination {
    match cancel.cause() {
        Some(CancelCause::Timeout) => RunTermination::TimedOut,
        Some(CancelCause::User) => RunTermination::Signalled(Signal::Int),
        _ => RunTermination::Signalled(Signal::Term),
    }
}

/// Everything before the first turn.
///
/// What is left here after the four steps above is the *order* they happen in,
/// which is the part that is genuinely this function's: settings before
/// session choice before materials because a missing `resume` target is true
/// independently of model configuration, materials before the provider because
/// a bad `--tools` must not reach a network, and the provider before the session
/// is opened because a session must not be created for a run whose model does
/// not resolve.
async fn execute<S: EventSink>(
    args: Args,
    destination: impl FnOnce(&ModelCapabilities) -> S,
    cancel: CancelToken,
    driver: Driver<'_>,
) -> Result<Ended, RunTermination> {
    let usage = |err: UsageError| RunTermination::UsageError {
        message: err.to_string(),
    };

    let settings = settings(&args, |root| run_trust(&args, &driver, root)).map_err(usage)?;
    let Chosen {
        store: jsonl_store,
        id: session_id,
    } = chosen(&args, &settings.root).map_err(usage)?;
    let Materials {
        system,
        registry,
        compiled,
        text,
        slug,
    } = materials(
        &settings,
        &args,
        matches!(&driver, Driver::Interactive { .. }),
    )
    .map_err(usage)?;

    let Settings {
        started,
        timeout,
        root,
        cwd,
        confinement,
        effective,
    } = settings;

    // Capability proof is a startup gate, so it runs before provider
    // resolution and before the durable session is opened. Its evidence lives
    // in a disposable session-bound store. The actual turn executor below is
    // rebuilt with the turn's latching cancellation token and the real
    // session evidence facade.
    let proof_directory = tempfile::tempdir().map_err(|err| RunTermination::Failed {
        message: format!("could not create the sandbox proof store: {err}"),
    })?;
    let proof_evidence =
        SessionEvidenceStore::open(proof_directory.path(), session_id, EVIDENCE_CAP).map_err(
            |err| RunTermination::Failed {
                message: format!("could not open the sandbox proof store: {err}"),
            },
        )?;
    let (proof, proof_observation) = guarded_exec(&confinement, &proof_evidence, &cancel).await?;
    drop(proof);

    let Provider {
        http,
        resolved,
        credential,
    } = provider(&slug, &effective, args.offline).await?;

    let mode = effective.mode.unwrap_or(Mode::Default);
    let judge = if mode == Mode::Auto {
        let identity = effective
            .judge
            .clone()
            .ok_or_else(|| RunTermination::UsageError {
                message: "`auto` mode requires `[permissions] judge = \"provider/model\"`"
                    .to_string(),
            })?;
        let Provider {
            http,
            resolved,
            credential,
        } = provider(&identity.0, &effective, args.offline).await?;
        let config = JudgeConfig {
            identity,
            request_model: resolved.model.clone(),
            max_output_tokens: output_ceiling(&resolved.capabilities),
        };
        let judge = ResolvedJudge {
            generator: ego_ai::generator::WireGenerator::new(
                resolved.row.clone(),
                http,
                credential,
            ),
            resolved,
            config,
        };
        preflight_judge(&judge, &confinement, proof_observation.capabilities())?;
        Some(judge)
    } else {
        None
    };

    let currency = CurrencyCode::new(SETTLEMENT).expect("a constant this file controls");
    let clock = SystemClock;
    let opened = match &jsonl_store {
        Some(store) => Opened::Durable(Box::new(
            session::open(store, &session_id, &clock).map_err(|err| RunTermination::Failed {
                message: err.to_string(),
            })?,
        )),
        None => Opened::Ephemeral(session::NullStore),
    };
    if let Some(note_text) = opened.repair_note() {
        note(&note_text);
    }

    // Declared before `evidence`, and therefore dropped after it. The
    // ephemeral store names files inside this owner; durable stores instead
    // retain the session writer's lease and need no temporary owner.
    let _ephemeral_evidence_owner;
    let evidence = match &opened {
        Opened::Durable(open) => {
            drop(proof_directory);
            _ephemeral_evidence_owner = None;
            open.writer
                .evidence_store(EVIDENCE_CAP)
                .map_err(|err| RunTermination::Failed {
                    message: format!("could not open this session's tool evidence: {err}"),
                })?
        }
        Opened::Ephemeral(_) => {
            _ephemeral_evidence_owner = Some(proof_directory);
            proof_evidence
        }
    };

    // Everything that can refuse has refused. From here the run has a session,
    // and every ending is a `RunTermination` with a log behind it.
    let sink = destination(&resolved.capabilities);
    let sink = &sink;
    //
    // The opening reduction, taken once: it is what `session.started` is
    // published against and what the loop then folds into, so reading it twice
    // would let the announcement and the turn disagree about where the session
    // began.
    let opening = opened.state();
    sink.publish(
        &PublicEvent::SessionStarted {
            v: PUBLIC_EVENT_VERSION,
            session: session_id,
            capabilities: capabilities(&registry, system.is_some(), compiled.is_some()),
            mcp_errors: Vec::new(),
            hook_errors: Vec::new(),
            instructions: system
                .as_ref()
                .map(|prompt| prompt.sources.clone())
                .unwrap_or_default(),
        },
        &opening,
    );

    let generator =
        ego_ai::generator::WireGenerator::new(resolved.row.clone(), Arc::clone(&http), credential);
    // An untrusted workspace resolves to `Mode::Default`; a trusted
    // interactive run may obtain its mode from the admitted config layers.
    let policy = match &driver {
        Driver::Once => {
            PolicyEngine::new(CanonPath::new(&root), mode, effective.permissions.clone())
        }
        Driver::Interactive { seat, .. } if seat.has_human() => PolicyEngine::with_human(
            CanonPath::new(&root),
            mode,
            effective.permissions.clone(),
            Arc::new(SeatApprover {
                seat: Arc::clone(seat),
            }),
        ),
        Driver::Interactive { .. } => {
            PolicyEngine::new(CanonPath::new(&root), mode, effective.permissions.clone())
        }
    };
    let fs = OsFs;
    let mut gate = budget::gate(&effective.ceilings, effective.clamped.money, currency);

    let config = TurnConfig {
        model: resolved.model.clone(),
        system,
        thinking: args.thinking,
        max_output_tokens: output_ceiling(&resolved.capabilities),
        parallel_tool_calls: resolved.capabilities.supports_parallel_tool_calls,
    };

    let turns = Turns {
        schema: compiled.as_ref(),
        retries: args.schema_retries,
        max_turns: args.max_turns,
        timeout,
        started,
    };

    // One turn's worth of loop, built from the state the last one left behind.
    //
    // Rebuilt per turn rather than held across the session, and that is
    // `CancelToken`'s doing rather than a preference: the token is latching
    // (`ego-core/src/turn/mod.rs:89`, "the first cause wins") and lives inside
    // `Session`, so a `Ctrl+C` on turn one would leave every later turn to
    // abort before its first check. A fresh loop takes a fresh token. It costs
    // nothing — every field below is a borrow or a clone of something already
    // resolved, and no I/O is repeated.
    // A macro and not a closure: the loop borrows from this function's locals,
    // and a closure cannot name the lifetime of a `TurnLoop<'_>` it returns.
    macro_rules! build {
        ($state:expr, $cancel:expr, $process:expr, $observation:expr) => {{
            let session = Session {
                id: session_id,
                state: $state,
                store: opened.store(),
                clock: &clock,
                sink,
                cancel: $cancel.clone(),
            };
            let model = Model {
                generator: &generator,
                gate: &mut gate,
                metering: resolved.metering.clone(),
                rates: resolved.rates.clone(),
                token_ceiling: effective.ceilings.tokens,
                caps: resolved.capabilities.clone(),
                config: config.clone(),
            };
            let tools = Tools::new(
                &registry,
                &policy,
                &fs,
                $process.as_ref(),
                &evidence,
                CanonPath::new(&cwd),
            );
            let loop_ = if let Some(judge) = judge.as_ref() {
                TurnLoop::with_judge(
                    session,
                    model,
                    tools,
                    JudgeModel {
                        generator: &judge.generator,
                        metering: judge.resolved.metering.clone(),
                        rates: judge.resolved.rates.clone(),
                        caps: judge.resolved.capabilities.clone(),
                        config: judge.config.clone(),
                        confinement: confinement.clone(),
                        exec_caps: $observation.capabilities(),
                    },
                )
            } else {
                TurnLoop::new(session, model, tools)
            };
            loop_.with_execution_observation($observation)
        }};
    }

    match driver {
        Driver::Once => {
            let (process, observation) = guarded_exec(&confinement, &evidence, &cancel).await?;
            let mut loop_ = build!(opening, &cancel, process, observation);
            let (termination, result) = turns.drive(&mut loop_, &text, &cancel).await;

            Ok(Ended {
                termination,
                session: session_id,
                state: loop_.into_state(),
                result,
            })
        }
        Driver::Interactive { seat, between } => {
            let mut state = opening;
            let mut last = Ended {
                termination: RunTermination::Done,
                session: session_id,
                state: state.clone(),
                result: Value::String(String::new()),
            };

            // `ego "explain this failing test"` seeds the first turn; `ego`
            // alone starts at the prompt. Both are one session — the argument
            // is a first thing to say, not a different mode.
            let mut seeded = (!text.trim().is_empty()).then(|| text.clone());

            loop {
                // The editor is the only writer while it reads, and no read
                // remains active while the turn renders. R12, held by
                // construction rather than by care.
                let line = match seeded.take() {
                    Some(first) => repl::Line::Submitted(first),
                    None => seat.read(),
                };

                let asked = match line {
                    repl::Line::Submitted(text) => text,
                    repl::Line::Blank => continue,
                    // Ctrl+C at an idle prompt drops the line and nothing
                    // else. Someone who pressed it to stop a turn must not
                    // lose the session by pressing it once more.
                    repl::Line::Interrupted => continue,
                    repl::Line::Ended => break,
                    repl::Line::Refused(why) => {
                        note(why);
                        continue;
                    }
                };

                // Per turn, for the reason `build` documents.
                let turn_cancel = CancelToken::new();
                let (process, observation) =
                    guarded_exec(&confinement, &evidence, &turn_cancel).await?;
                let mut loop_ = build!(state, &turn_cancel, process, observation);
                let (termination, result) = turns.drive(&mut loop_, &asked, &turn_cancel).await;
                state = loop_.into_state();
                between();

                last = Ended {
                    termination,
                    session: session_id,
                    state: state.clone(),
                    result,
                };

                // The person's own cancellations end a turn; the process's end
                // the process. A broken stdout or a SIGTERM is not a turn to
                // return to a prompt from.
                if cancel.is_cancelled() {
                    break;
                }
            }

            Ok(last)
        }
    }
}

/// What drives the turns once the session is open.
///
/// The setup above is identical either way — the same resolution, the same
/// session, the same policy — so it is the tail that branches rather than the
/// function that is duplicated.
enum Driver<'a> {
    /// One prompt from argv or stdin, and the run is over.
    Once,
    /// Lines from a person, until they stop.
    Interactive {
        seat: Arc<dyn InteractiveSeat>,
        /// Run after each turn: the screen holds the last block back until it
        /// knows no more is coming, and between turns is when it knows.
        between: &'a (dyn Fn() + Send + Sync),
    },
}

fn run_trust(args: &Args, driver: &Driver<'_>, root: &Path) -> Trust {
    match driver {
        Driver::Once => Trust::Untrusted,
        Driver::Interactive { seat, .. } if !args.bare && seat.has_human() => seat.trust(root),
        Driver::Interactive { .. } => Trust::Untrusted,
    }
}

/// The seat, as the run loop needs it.
///
/// A trait so a test can drive the whole interactive path from a script of
/// lines, with no terminal and no editor — the real [`repl::Seat`] needs a tty
/// and would make every one of those tests unrunnable in CI.
pub trait Reads {
    /// Read one line, blocking until there is one.
    fn read(&self) -> repl::Line;
}

/// The two explicit decisions only a human-driven run may make.
pub trait InteractiveSeat: Reads + std::fmt::Debug + Send + Sync {
    fn has_human(&self) -> bool;
    fn trust(&self, workspace: &Path) -> Trust;
    fn permission(&self, request: &PermissionRequest) -> PermissionDecision;
}

#[derive(Debug)]
struct SeatApprover {
    seat: Arc<dyn InteractiveSeat>,
}

impl HumanApprover for SeatApprover {
    fn decide<'a>(
        &'a self,
        request: &'a PermissionRequest,
    ) -> futures::future::BoxFuture<'a, PermissionDecision> {
        Box::pin(futures::future::ready(self.seat.permission(request)))
    }
}

/// The run-level limits, and the loop that applies them between turns.
///
/// Between turns is where they belong: `max_turns` fires *between* two
/// completed turns and is not honestly a cause of the preceding one, and a
/// token budget cannot interrupt a generation that is already paid for.
struct Turns<'a> {
    schema: Option<&'a schema::Schema>,
    retries: u32,
    max_turns: Option<u32>,
    timeout: Option<Duration>,
    started: std::time::Instant,
}

impl Turns<'_> {
    async fn drive(
        &self,
        loop_: &mut TurnLoop<'_>,
        first: &str,
        cancel: &CancelToken,
    ) -> (RunTermination, Value) {
        let mut prompt = first.to_string();
        let mut taken: u32 = 0;
        let mut left = self.retries;

        loop {
            if self.max_turns.is_some_and(|max| taken >= max) {
                return (RunTermination::MaxTurns, self.answer(loop_.state()));
            }
            let outcome = match self.one(loop_, &prompt, cancel).await {
                Ok(outcome) => outcome,
                Err(termination) => return (termination, self.answer(loop_.state())),
            };
            taken += 1;

            if let Some(termination) =
                RunTermination::after_turn(outcome.exit_reason, outcome.denied)
            {
                return (termination, self.answer(loop_.state()));
            }

            let answer = last_assistant_message(loop_.state());
            let Some(schema) = self.schema else {
                return (RunTermination::Done, Value::String(answer));
            };
            match schema.check(&answer) {
                Ok(document) => return (RunTermination::Done, document),
                Err(invalid) => {
                    note(&format!("the answer did not validate: {invalid}"));
                    if left == 0 {
                        return (RunTermination::TaskFailed, Value::String(answer));
                    }
                    left -= 1;
                    prompt = schema::repair_prompt(&invalid);
                }
            }
        }
    }

    /// One turn, watched by the two things only a process can see: a signal,
    /// and a deadline.
    ///
    /// The cancellation is cooperative first and bounded second. The token is
    /// set so the loop unwinds cleanly — settling its attempt, writing
    /// `TurnCompleted` — and only if it has not finished within [`GRACE`] does
    /// the run stop waiting and report from the cause. Dropping the future
    /// immediately would abandon an unsettled reservation; waiting forever
    /// would make `--timeout` advisory.
    async fn one(
        &self,
        loop_: &mut TurnLoop<'_>,
        prompt: &str,
        cancel: &CancelToken,
    ) -> Result<TurnOutcome, RunTermination> {
        let remaining = self
            .timeout
            .map(|budget| budget.saturating_sub(self.started.elapsed()));

        let outcome = {
            let mut running = Box::pin(loop_.run(prompt));
            let mut stopped = false;
            loop {
                if stopped {
                    tokio::select! {
                        done = &mut running => break Some(done),
                        () = tokio::time::sleep(GRACE) => break None,
                    }
                }
                tokio::select! {
                    done = &mut running => break Some(done),
                    cause = stop_signal(remaining) => {
                        cancel.cancel(cause);
                        stopped = true;
                    }
                }
            }
        };

        match outcome {
            Some(Ok(outcome)) => Ok(outcome),
            Some(Err(abandoned)) => Err(RunTermination::Failed {
                message: format!("the session log could not be written: {}", abandoned.cause),
            }),
            // The turn did not unwind in time. The cause is still the honest
            // account of why the run ended, and it is the same mapping a
            // cancelled turn would have produced.
            None => Err(match cancel.cause() {
                Some(CancelCause::Timeout) => RunTermination::TimedOut,
                Some(CancelCause::User) => RunTermination::Signalled(Signal::Int),
                _ => RunTermination::Signalled(Signal::Term),
            }),
        }
    }

    /// The last assistant message, as `CLI.md` §4.1's `result`.
    fn answer(&self, state: &SessionState) -> Value {
        Value::String(last_assistant_message(state))
    }
}

/// Whichever of a signal or the deadline arrives first, and who it was.
///
/// A `SIGINT` and an expired `--timeout` are different exits (130 and 124), so
/// this returns the cause rather than the fact — and `CancelToken` keeps the
/// first one, so a deadline that expires a moment after the user pressed
/// Ctrl-C does not rewrite what happened.
async fn stop_signal(remaining: Option<Duration>) -> CancelCause {
    let deadline = async {
        match remaining {
            Some(left) => tokio::time::sleep(left).await,
            // Nothing to wait for; never resolves, so `select!` ignores it.
            None => std::future::pending().await,
        }
    };
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("a process may always listen for SIGTERM");

    tokio::select! {
        _ = tokio::signal::ctrl_c() => CancelCause::User,
        _ = term.recv() => CancelCause::Host,
        () = deadline => CancelCause::Timeout,
    }
}

/// The text of the last assistant message in the session.
///
/// Read from `SessionState` rather than accumulated while streaming: the state
/// is the fold of the durable log, so what is reported is what was written
/// down. An empty string when the run produced no assistant message — §4.1's
/// `result` is a string, and a run that said nothing said the empty thing.
fn last_assistant_message(state: &SessionState) -> String {
    state
        .items
        .values()
        .filter(|item| item.kind == ItemKind::Assistant)
        .max_by_key(|item| item.seq)
        .map(|item| item.text.clone())
        .unwrap_or_default()
}

/// `ego ls`: the catalogue, printed (`CLI.md` §4.6).
///
/// A `RunTermination` like everything else, so one exit-code table answers for
/// the whole binary: a store that cannot be listed is `Failed` (exit 1) and a
/// store root that cannot be located is a `UsageError` (exit 2), exactly as
/// they are for a run. There is no session and no turn, so nothing here can
/// produce the endings that are about one.
pub fn ls(args: &crate::cli::LsArgs, out: &Stdout) -> RunTermination {
    match listed(args, out) {
        Ok(()) => RunTermination::Done,
        Err(termination) => termination,
    }
}

/// [`ls`], with the endings as an error so each step can be one line.
///
/// The `Ok(())` case is `RunTermination::Done` and nothing else, so there is
/// no ending decided here that [`ls`] does not name.
fn listed(args: &crate::cli::LsArgs, out: &Stdout) -> Result<(), RunTermination> {
    let usage = |err: UsageError| RunTermination::UsageError {
        message: err.to_string(),
    };

    let root = workspace(args.directory.as_ref()).map_err(usage)?;
    let store_root = session::store_root(args.session_dir.as_ref()).map_err(usage)?;
    // The workspace is handed over for the same reason `run` hands it over —
    // a store writes it into the header of any session it creates — even
    // though listing creates none. One constructor, one meaning.
    let store = session::jsonl(store_root, root.clone());

    let metas = crate::catalogue::listing(&store, &root, args.all).map_err(|err| {
        RunTermination::Failed {
            message: format!("the session store could not be listed: {err}"),
        }
    })?;

    match args.output {
        // §4.6's single object. Rendered even when there is nothing to list:
        // a script that gets no output cannot tell an empty store from a
        // command that failed to run.
        Output::Json => render(out, &crate::catalogue::Listing::of(&metas), "the listing")?,
        // One session per line, which is what §4.2's spelling means for a
        // list: the unit of a catalogue is a session, as the unit of a run is
        // an event.
        Output::Jsonl => {
            for meta in &metas {
                render(out, &crate::catalogue::Row::of(meta), "a session")?;
            }
        }
        Output::Text => tabulate(out, &metas, &root, args.all),
    }
    Ok(())
}

/// The catalogue as a table, and the sentence an empty one owes.
///
/// The header row is printed even for an empty store, so the person who typed
/// `ego ls` sees that the command ran. The empty case says so on stderr rather
/// than on stdout, which stays the listing — which is also why this cannot
/// fail: nothing is serialised, so there is no rendering to refuse.
fn tabulate(out: &Stdout, metas: &[SessionMeta], root: &Path, all: bool) {
    for line in crate::catalogue::table(metas, all) {
        out.line(&line);
    }
    if metas.is_empty() {
        note(&if all {
            "no sessions in this store".to_string()
        } else {
            format!(
                "no session belongs to {}; `ego ls --all` lists every one",
                root.display()
            )
        });
    }
}

/// One JSON value on one line, or the ending a value that will not serialise
/// produces.
///
/// `what` names the thing rather than the error: a `Listing` and a `Row` fail
/// for the same reason and the operator needs to know which of the two the
/// command was in the middle of.
fn render<T: serde::Serialize>(out: &Stdout, value: &T, what: &str) -> Result<(), RunTermination> {
    match serde_json::to_string(value) {
        Ok(line) => {
            out.line(&line);
            Ok(())
        }
        Err(err) => Err(RunTermination::Failed {
            message: format!("{what} could not be rendered: {err}"),
        }),
    }
}

/// The result of a run, rendered for `--output json` (`CLI.md` §4.1).
pub fn report(ended: &Ended) -> Report {
    Report::of(
        ended.session,
        &ended.state,
        &ended.termination,
        ended.result.clone(),
    )
}

/// The one line text mode owes at the end when something went wrong.
///
/// A run that exits non-zero with nothing on stderr has told a person only a
/// number. `--output json` says it in the object instead, and saying it twice
/// would put diagnostics on stdout.
///
/// `broken` is what [`Stdout`] recorded if a write failed. A run stopped by a
/// closed pipe ends as `Signalled(Term)`, which on its own reads as "somebody
/// sent SIGTERM"; naming the real cause is the difference between a report and
/// a guess.
pub fn diagnose(termination: &RunTermination, broken: Option<String>) {
    let message = match termination {
        RunTermination::Done => return,
        RunTermination::TaskFailed => "the answer never satisfied the schema".to_string(),
        RunTermination::UsageError { message } | RunTermination::Provider { message } => {
            message.clone()
        }
        RunTermination::Failed { message } => message.clone(),
        // Two sentences, because one of them was about to be false. "Nobody
        // could grant it" describes a question the evaluation order exhausted;
        // said of a refusal somebody made, it denies that anybody decided,
        // which is the one thing that did happen. Both still lead with the
        // class — a person who reads only this line has to see that exit 3 is
        // about permission — and both name the action, whose own `Display`
        // carries the rule, the mode or the boundary that produced it.
        RunTermination::PermissionRequired {
            action,
            by: DeniedBy::Decided(_),
        } => format!("stopped: permission refused — {action}"),
        RunTermination::PermissionRequired {
            action,
            by: DeniedBy::Nobody,
        } => format!("stopped: permission unresolved and nobody could be asked — {action}"),
        RunTermination::BudgetExceeded => "stopped: the budget was reached".to_string(),
        RunTermination::MaxTurns => "stopped: --max-turns was reached".to_string(),
        RunTermination::TimedOut => "stopped: --timeout expired".to_string(),
        RunTermination::Signalled(Signal::Int) => "interrupted".to_string(),
        RunTermination::Signalled(Signal::Term) => match broken {
            Some(why) => format!("stopped: {why}"),
            None => "terminated".to_string(),
        },
    };
    note(&message);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::caps::{Depth, OscProfile};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug)]
    struct TrustingSeat {
        asked: AtomicUsize,
    }

    impl Reads for TrustingSeat {
        fn read(&self) -> repl::Line {
            repl::Line::Ended
        }
    }

    impl InteractiveSeat for TrustingSeat {
        fn has_human(&self) -> bool {
            true
        }

        fn trust(&self, _workspace: &Path) -> Trust {
            self.asked.fetch_add(1, Ordering::SeqCst);
            Trust::Trusted
        }

        fn permission(&self, _request: &PermissionRequest) -> PermissionDecision {
            PermissionDecision::Deny
        }
    }

    fn interactive_args(extra: &[&str]) -> Args {
        let argv = std::iter::once("ego".to_string())
            .chain(extra.iter().map(|arg| arg.to_string()))
            .collect();
        let crate::cli::Invocation::Interactive(args) = crate::cli::parse(argv).unwrap() else {
            panic!("the test invocation is interactive");
        };
        *args
    }

    #[test]
    fn only_an_interactive_non_bare_run_can_ask_for_trust() {
        let seat = Arc::new(TrustingSeat {
            asked: AtomicUsize::new(0),
        });
        let dyn_seat: Arc<dyn InteractiveSeat> = seat.clone();
        let between = || {};
        let interactive = Driver::Interactive {
            seat: dyn_seat,
            between: &between,
        };
        assert_eq!(
            run_trust(&interactive_args(&[]), &interactive, Path::new("/ws")),
            Trust::Trusted
        );
        assert_eq!(seat.asked.load(Ordering::SeqCst), 1);

        assert_eq!(
            run_trust(
                &interactive_args(&["--bare"]),
                &interactive,
                Path::new("/ws")
            ),
            Trust::Untrusted
        );
        assert_eq!(
            seat.asked.load(Ordering::SeqCst),
            1,
            "--bare must skip the seat"
        );

        assert_eq!(
            run_trust(&interactive_args(&[]), &Driver::Once, Path::new("/ws")),
            Trust::Untrusted,
            "headless has no seat to consult"
        );
        assert_eq!(seat.asked.load(Ordering::SeqCst), 1);
    }

    fn capabilities(osc: OscProfile) -> Capabilities {
        Capabilities {
            depth: Depth::TrueColor,
            width: 80,
            hyperlinks: true,
            progress: true,
            osc,
        }
    }

    fn announce(osc: OscProfile, quiet: bool) -> (Stdout, Capabilities, bool) {
        (Stdout::new(CancelToken::new()), capabilities(osc), quiet)
    }

    /// The reason any of this exists: TUICommander segments a pane into command
    /// blocks from 133 A/C/D and otherwise falls back to pattern-matching
    /// tool-call headers (`tuicommander/docs/FEATURES.md:187`). C opens the
    /// block, D closes it carrying the exit status, and nothing may come after
    /// D — anything that did would belong to the next command.
    #[test]
    fn the_block_opens_with_c_and_closes_with_d_carrying_the_status() {
        let (out, capabilities, quiet) = announce(OscProfile::Standard, false);
        let announce = Announce::new(&out, capabilities, quiet);

        assert!(announce.preamble(Some("hello")).ends_with("\x1b]133;C\x07"));

        let failed = announce.postamble(&RunTermination::TaskFailed, Duration::ZERO);
        assert!(failed.ends_with("\x1b]133;D;1\x07"), "{failed:?}");
        let done = announce.postamble(&RunTermination::Done, Duration::ZERO);
        assert!(done.ends_with("\x1b]133;D;0\x07"), "{done:?}");
    }

    #[test]
    fn a_title_is_branded_once_and_closing_keeps_only_the_fixed_prefix() {
        let (out, capabilities, quiet) = announce(OscProfile::Standard, false);
        let announce = Announce::new(&out, capabilities, quiet);

        assert_eq!(
            announce.preamble(Some("explain the diff")),
            "\x1b]0;ɛ । explain the diff\x07\x1b]9;4;3;0\x07\x1b]133;C\x07"
        );
        assert_eq!(
            announce.preamble(None),
            "\x1b]0;ɛ । εgo\x07\x1b]9;4;3;0\x07\x1b]133;C\x07"
        );
        assert_eq!(
            announce.postamble(&RunTermination::Done, Duration::ZERO),
            "\x1b]9;4;0;0\x07\x1b]0;ɛ । \x07\x1b]133;D;0\x07"
        );
    }

    /// A progress indicator left set outlives the process that set it: the tab
    /// stays marked until the terminal is restarted. So it is cleared on the
    /// failure path too, and the failure is reported by 133 D instead.
    #[test]
    fn the_indicator_is_always_taken_down_however_the_run_ended() {
        let (out, capabilities, quiet) = announce(OscProfile::Standard, false);
        let announce = Announce::new(&out, capabilities, quiet);

        assert!(
            announce.preamble(None).contains("\x1b]9;4;3;0\x07"),
            "a run of unknown length is indeterminate"
        );
        for termination in [
            RunTermination::Done,
            RunTermination::TaskFailed,
            RunTermination::Signalled(Signal::Int),
        ] {
            let closing = announce.postamble(&termination, Duration::ZERO);
            let state = if termination.exit_code() == 0 { 0 } else { 2 };
            assert!(
                closing.contains(&format!("\x1b]9;4;{state};0\x07")),
                "{termination:?} left the indicator: {closing:?}"
            );
        }
    }

    /// §4.5, applied to the out-of-band channel. A JSON document with an OSC in
    /// it is not JSON, and the gate is what prevents that rather than a rule
    /// each call site remembers.
    #[test]
    fn a_machine_readable_run_announces_nothing_at_all() {
        let out = Stdout::new(CancelToken::new());
        let announce = Announce::new(&out, Capabilities::plain(), false);
        assert_eq!(announce.preamble(Some("hello")), "");
        assert_eq!(
            announce.postamble(&RunTermination::Done, Duration::from_secs(3_600)),
            ""
        );
    }

    /// A notification is an interruption. It is worth it for a run the operator
    /// stopped watching, and it is noise for one they saw finish — so the
    /// duration decides, and `-q` overrules the duration.
    #[test]
    fn only_a_long_run_is_worth_interrupting_someone_over() {
        let (out, capabilities, _) = announce(OscProfile::Standard, false);
        let loud = Announce::new(&out, capabilities, false);

        // `\x1b]9;εgo`, not `\x1b]9;` — OSC 9;4 is the progress indicator and
        // shares the prefix.
        assert!(
            !loud
                .postamble(&RunTermination::Done, NOTIFY_AFTER - Duration::from_secs(1))
                .contains("\x1b]9;εgo"),
            "a run somebody watched finish"
        );
        let long = loud.postamble(&RunTermination::Done, Duration::from_secs(252));
        assert!(
            long.contains("\x1b]9;εgo: done after 4m 12s\x07"),
            "{long:?}"
        );

        let quiet = Announce::new(&out, capabilities, true);
        assert!(
            !quiet
                .postamble(&RunTermination::Done, Duration::from_secs(252))
                .contains("\x1b]9;εgo"),
            "-q suppresses it at any duration"
        );
    }

    /// The private protocol reaches TUIC and nowhere else, and it is the
    /// capability gate that decides — not a call site that checks a variable.
    #[test]
    fn only_tuic_is_told_what_ego_is_doing() {
        let (out, standard, _) = announce(OscProfile::Standard, false);
        let elsewhere = Announce::new(&out, standard, false);
        assert!(!elsewhere.preamble(None).contains("7770"));

        let inside = Announce::new(&out, capabilities(OscProfile::Tuic), false);
        assert!(inside.preamble(None).contains("\x1b]7770;state=busy\x07"));
        assert!(
            inside
                .postamble(&RunTermination::Done, Duration::ZERO)
                .contains("\x1b]7770;state=idle\x07")
        );
    }

    #[test]
    fn the_title_says_what_was_asked_and_never_says_a_dash() {
        assert_eq!(title(Some("explain the diff")), "explain the diff");
        // `-` is the spelling for "the prompt is on stdin".
        assert_eq!(title(Some("-")), "εgo");
        assert_eq!(title(None), "εgo");
        assert_eq!(title(Some("   ")), "εgo");
        // A prompt is often a paragraph; a tab is one line.
        assert_eq!(title(Some("first line\nsecond line")), "first line");
    }

    /// Cut in cells, like everything else that reaches a terminal. A title cut
    /// by bytes puts a replacement character in the tab.
    #[test]
    fn a_long_title_is_cut_where_a_grapheme_ends() {
        let flag = "\u{1f1ee}\u{1f1f9}";
        let title = title(Some(&flag.repeat(40)));
        assert!(!title.contains('\u{fffd}'));
        assert_eq!(crate::render::line::cells(&title), 48);
    }

    #[test]
    fn a_duration_is_written_for_a_person() {
        assert_eq!(duration(Duration::from_secs(9)), "9s");
        assert_eq!(duration(Duration::from_secs(60)), "1m 0s");
        assert_eq!(duration(Duration::from_secs(252)), "4m 12s");
    }
}
