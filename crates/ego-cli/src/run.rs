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
    Effective, Limits, Posture, Selection, SystemPrompt, Trust, resolve, system_prompt,
};
use ego_core::cost::CurrencyCode;
use ego_core::event::domain::CancelCause;
use ego_core::event::id::SessionId;
use ego_core::permit::{CanonPath, PolicyEngine, Tool};
use ego_core::public::{PUBLIC_EVENT_VERSION, PublicEvent};
use ego_core::run::{RunTermination, Signal};
use ego_core::sink::EventSink;
use ego_core::state::{ItemKind, SessionState};
use ego_core::turn::{CancelToken, Model, Session, Tools, TurnConfig, TurnLoop, TurnOutcome};
use ego_tools::clock::SystemClock;
use ego_tools::fs::OsFs;
use ego_tools::instructions::OsInstructionIo;
use ego_tools::tools::builtins;
use serde_json::Value;

use crate::budget;
use crate::cli::{Args, Output, SessionChoice, UsageError};
use crate::output::{JsonlSink, Report, Stdout, TextSink, note};
use crate::session::{self, Opened};
use crate::{config, facts, model, prompt, schema};

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

/// Run `args` to completion and say how it ended.
pub async fn run(args: Args, out: &Stdout, cancel: CancelToken) -> Ended {
    match execute(args, out, cancel).await {
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
fn workspace(directory: Option<&PathBuf>) -> Result<PathBuf, UsageError> {
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
fn output_ceiling(caps: &ego_core::generator::capabilities::ModelCapabilities) -> Option<u64> {
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

/// Everything before the first turn.
async fn execute(args: Args, out: &Stdout, cancel: CancelToken) -> Result<Ended, RunTermination> {
    let usage = |err: UsageError| RunTermination::UsageError {
        message: err.to_string(),
    };

    let started = std::time::Instant::now();
    let timeout = match args.timeout.as_deref() {
        Some(spec) => Some(deadline(spec).map_err(usage)?),
        None => None,
    };

    let cwd = workspace(args.directory.as_ref()).map_err(usage)?;
    // The workspace root is the working directory. Discovering a repository
    // root is `git`'s question and would make the answer depend on a tool that
    // may not be installed; `-C` is how a caller says otherwise.
    let root = cwd.clone();

    // §4.5: "headless in an untrusted workspace runs bare automatically (no
    // dialog, no ambient code execution)". There is no trust store, and a
    // headless run has nobody to ask, so this is not a default that a later
    // milestone relaxes — it is what the section says to do.
    let trust = Trust::Untrusted;
    let layers = config::layers(&root, &args, trust).map_err(usage)?;
    let effective = resolve(&layers, trust, config::profile(&args).as_deref())
        .map_err(|err| usage(UsageError(err.to_string())))?;
    debug_assert_eq!(effective.posture, Posture::Bare);

    let currency = CurrencyCode::new(SETTLEMENT).expect("a constant this file controls");
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

    let compiled = match args.schema.as_deref() {
        Some(spec) => {
            let text = prompt::at_file(spec).map_err(usage)?;
            Some(schema::Schema::compile(&text).map_err(usage)?)
        }
        None => None,
    };

    let system = instructions(&effective, &args, &root, &cwd).map_err(usage)?;
    let registry = tools(args.tools.as_ref()).map_err(usage)?;

    let text = prompt::assemble(args.prompt.as_deref(), args.stdin, &prompt::ProcessStdin)
        .map_err(usage)?;

    let slug = args
        .model
        .clone()
        .or_else(|| effective.model.clone().map(|model| model.0))
        .ok_or_else(|| {
            usage(UsageError(
                "no model: pass --model provider/model, set EGO_MODEL, or put \
                 `model = \"…\"` in a config file"
                    .to_string(),
            ))
        })?;

    let http = model::transport(args.offline).map_err(|err| RunTermination::Provider {
        message: err.to_string(),
    })?;
    let hosted = facts::supplied(&effective.ceilings).map_err(usage)?;
    let resolved = model::resolve(&slug, hosted, &http).await.map_err(|err| {
        if model::is_provider_failure(&err) {
            RunTermination::Provider {
                message: err.to_string(),
            }
        } else {
            RunTermination::UsageError {
                message: err.to_string(),
            }
        }
    })?;

    let session_id = session::id_for(&args.session, &root).map_err(usage)?;
    let store_root = match args.session {
        SessionChoice::Ephemeral => None,
        _ => Some(session::store_root(args.session_dir.as_ref()).map_err(usage)?),
    };
    let clock = SystemClock;
    let jsonl_store = store_root.map(session::jsonl);
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

    // Everything that can refuse has refused. From here the run has a session,
    // and every ending is a `RunTermination` with a log behind it.
    let text_sink = TextSink::new(out, args.quiet);
    let jsonl_sink = JsonlSink::new(out);
    let sink: &dyn EventSink = match args.output {
        Output::Text => &text_sink,
        Output::Jsonl => &jsonl_sink,
        // §4.1 is a single object at completion, so nothing is published as it
        // happens — but the run still needs a sink, and one that writes
        // nothing is the honest one rather than a collector nobody reads.
        Output::Json => &ego_core::sink::NullSink,
    };

    sink.publish(&PublicEvent::SessionStarted {
        v: PUBLIC_EVENT_VERSION,
        session: session_id,
        capabilities: capabilities(&registry, system.is_some(), compiled.is_some()),
        mcp_errors: Vec::new(),
        hook_errors: Vec::new(),
        instructions: system
            .as_ref()
            .map(|prompt| prompt.sources.clone())
            .unwrap_or_default(),
    });

    let generator = ego_ai::generator::WireGenerator::new(resolved.row.clone(), Arc::clone(&http));
    let policy = PolicyEngine::new(CanonPath::new(&root));
    let fs = OsFs;
    let mut gate = budget::gate(&effective.ceilings, effective.clamped.money, currency);

    let config = TurnConfig {
        model: resolved.model.clone(),
        system,
        thinking: args.thinking,
        max_output_tokens: output_ceiling(&resolved.capabilities),
        parallel_tool_calls: resolved.capabilities.supports_parallel_tool_calls,
    };

    let mut loop_ = TurnLoop::new(
        Session {
            id: session_id,
            state: opened.state(),
            store: opened.store(),
            clock: &clock,
            sink,
            cancel: cancel.clone(),
        },
        Model {
            generator: &generator,
            gate: &mut gate,
            metering: resolved.metering.clone(),
            rates: resolved.rates.clone(),
            token_ceiling: effective.ceilings.tokens,
            caps: resolved.capabilities.clone(),
            config,
        },
        Tools::new(&registry, &policy, &fs, CanonPath::new(&cwd)),
    );

    let turns = Turns {
        schema: compiled.as_ref(),
        retries: args.schema_retries,
        max_turns: args.max_turns,
        timeout,
        started,
    };
    let (termination, result) = turns.drive(&mut loop_, &text, &cancel).await;

    if args.output == Output::Text {
        text_sink.finish();
    }

    Ok(Ended {
        termination,
        session: session_id,
        state: loop_.into_state(),
        result,
    })
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
                RunTermination::after_turn(outcome.exit_reason, outcome.denied.as_deref())
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
        RunTermination::PermissionRequired { action } => {
            format!("stopped: {action} needed a permission nobody could grant")
        }
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
