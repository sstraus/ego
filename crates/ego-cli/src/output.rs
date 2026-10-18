//! Everything that reaches a terminal or a pipe: the three `--output` modes
//! of `CLI.md` §4, and the rule that stdout is protocol-clean.
//!
//! §4.5: "stdout is protocol-clean in json/jsonl/acp modes, all diagnostics on
//! stderr". That is enforced here by there being exactly one type that can
//! write to stdout at all — [`Stdout`] — and by the diagnostic path
//! ([`note`]) not being able to reach it.

use std::collections::{HashMap, VecDeque};
use std::io::{ErrorKind, IsTerminal, Write};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use ego_core::cost::{CumulativeCost, RateSource};
use ego_core::event::domain::{CancelCause, OccupancyProvenance};
use ego_core::event::id::{ItemId, SessionId, TurnId};
use ego_core::permit::Authority;
use ego_core::public::{CeilingEffect, ItemKind, MeteringKind, PUBLIC_EVENT_VERSION, PublicEvent};
use ego_core::run::{DeniedBy, RunTermination};
use ego_core::sink::EventSink;
use ego_core::state::SessionState;
use ego_core::turn::CancelToken;
use serde::Serialize;

use crate::DISPLAY_NAME;
use crate::render::tool_row::{self, Finished};
use crate::render::{Answer, Capabilities};
use crate::repl::SurfaceOutput;

/// The CLI-owned output boundary used by text rendering and diagnostics.
/// Headless output keeps stdout/stderr split; an interactive implementation
/// frames both through Reedline's sole terminal writer.
pub trait Diagnostics: Send + Sync {
    fn note(&self, message: &str);

    /// A finished tool call: its rows, and the full detail the expand key
    /// shows later. A headless run has no expand key, so the rows are a
    /// diagnostic and the detail is dropped; a terminal surface commits the
    /// rows to the transcript and keeps the detail.
    fn tool_row(&self, rows: &str, _detail: String) {
        self.note(rows);
    }

    /// Print the detail of the most recent tool call, if this surface kept
    /// one.
    fn expand_last_tool(&self) {}
}

pub trait TextOutput: Diagnostics {
    fn write(&self, text: &str);
    fn answer(&self, text: &str) {
        self.write(text);
    }
    fn line(&self, text: &str);
    /// Publish nonprinting host control bytes without committing a text row.
    fn signal(&self, sequence: &str) {
        self.write(sequence);
    }
    fn live_retry(&self) -> bool {
        false
    }
    /// Replace a live retry row; ordinary outputs remain append-only.
    fn retry_status(&self, _message: Option<(&str, u64)>) {}
    /// Refresh the active goal counter without writing a transcript row.
    fn goal_status(&self, _goal: Option<(u32, Option<u32>)>) {}
    fn begin_block(&self) {}
    fn finish(&self) {}
}

/// The unchanged diagnostic adapter for non-interactive callers.
pub struct Stderr;

impl Diagnostics for Stderr {
    fn note(&self, message: &str) {
        note(message);
    }
}

/// The exclusive owner of the process's standard output.
///
/// One type, holding one lock, so "stdout is protocol-clean" is a property of
/// the program rather than a rule every call site has to remember. Diagnostics
/// cannot get here — [`note`] writes to stderr and takes no `Stdout`.
///
/// ## What happens when the reader walks away
///
/// `ego … --output jsonl | head -1` closes the read end of the pipe. Rust
/// ignores `SIGPIPE`, so the write comes back `EPIPE` rather than killing the
/// process, and [`EventSink::publish`] is infallible by design — `sink.rs`
/// says the CLI owns stdout and owns `RunTermination`, so this is the layer
/// that has to have an opinion.
///
/// The opinion is: **stop the run**. The first error is recorded, nothing
/// further is written (a broken pipe is not retried once per event for the
/// rest of the session), the turn is cancelled with [`CancelCause::Host`], and
/// the reason goes to stderr. The run ends as `Signalled(Term)` → exit 143.
///
/// The two alternatives were both worse. Restoring the default `SIGPIPE`
/// disposition — so `ego | head -1` dies the way `cat | head -1` does — kills
/// the process *between two writes*: the attempt in flight never settles, the
/// turn never records how it ended, and the session on disk stops mid-sentence
/// for a reason no reader can recover. And absorbing the error to keep running
/// bills somebody for a generation nobody will read, while being precisely the
/// "swallows both the transport error and the event" that `sink.rs` names as
/// the failure it avoids.
pub struct Stdout {
    inner: Mutex<Option<std::io::Stdout>>,
    /// The first write that failed, kept so the run can report *why* it
    /// stopped rather than merely that it did.
    broken: Mutex<Option<String>>,
    cancel: CancelToken,
}

impl Stdout {
    pub fn new(cancel: CancelToken) -> Self {
        Stdout {
            inner: Mutex::new(Some(std::io::stdout())),
            broken: Mutex::new(None),
            cancel,
        }
    }

    /// Write one line, or notice that the far end is gone.
    pub fn line(&self, text: &str) {
        self.emit(|out| writeln!(out, "{text}"));
    }

    /// Write without a newline, for streamed text — and for text that already
    /// ends in one, which is what clap's rendered help is.
    pub fn write(&self, text: &str) {
        self.emit(|out| write!(out, "{text}"));
    }

    /// The one place a write to stdout happens, and therefore the one place
    /// the reader walking away is noticed. Written once rather than at each
    /// call site: two copies of this would be two chances for one of them to
    /// keep writing after the pipe broke.
    fn emit(&self, write: impl FnOnce(&mut std::io::Stdout) -> std::io::Result<()>) {
        let mut held = self
            .inner
            .lock()
            .expect("a poisoned lock means a writer panicked mid-line");
        let Some(out) = held.as_mut() else {
            return;
        };
        let Err(err) = write(out).and_then(|()| out.flush()) else {
            return;
        };
        // Dropped, so no later write can be attempted: "stopped writing" is
        // enforced by no longer holding the handle.
        *held = None;
        let message = if err.kind() == ErrorKind::BrokenPipe {
            "stdout was closed by the reader".to_string()
        } else {
            format!("stdout could not be written: {err}")
        };
        note(&message);
        *self
            .broken
            .lock()
            .expect("a poisoned lock means a writer panicked mid-line") = Some(message);
        self.cancel.cancel(CancelCause::Host);
    }

    /// Why stdout stopped accepting writes, if it did.
    pub fn broken(&self) -> Option<String> {
        self.broken
            .lock()
            .expect("a poisoned lock means a writer panicked mid-line")
            .clone()
    }
}

impl Diagnostics for Stdout {
    fn note(&self, message: &str) {
        note(message);
    }
}

impl TextOutput for Stdout {
    fn write(&self, text: &str) {
        Stdout::write(self, text);
    }

    fn line(&self, text: &str) {
        Stdout::line(self, text);
    }
}

/// How many tool calls and collapsed prompts the expand key retains.
const EXPANDABLE_CALLS: usize = 16;

/// One history shared by submission receipts and finished tool output.
#[derive(Default)]
pub(crate) struct ExpansionHistory(Mutex<VecDeque<String>>);

impl ExpansionHistory {
    pub(crate) fn remember(&self, detail: String) {
        let mut entries = self.0.lock().expect("expansion detail lock");
        if entries.len() == EXPANDABLE_CALLS {
            entries.pop_front();
        }
        entries.push_back(detail);
    }

    pub(crate) fn latest(&self) -> Option<String> {
        self.0
            .lock()
            .expect("expansion detail lock")
            .back()
            .cloned()
    }
}

/// Interactive text and diagnostics serialized into complete logical rows.
pub struct InteractiveOutput {
    inner: Mutex<SurfaceOutput>,
    /// Shared with the terminal owner's prompt submission renderer.
    details: Arc<ExpansionHistory>,
}

impl InteractiveOutput {
    pub fn new(output: SurfaceOutput) -> Self {
        Self {
            details: Arc::clone(&output.details),
            inner: Mutex::new(output),
        }
    }
}

impl Diagnostics for InteractiveOutput {
    fn note(&self, message: &str) {
        let text = format!("{DISPLAY_NAME}: {message}");
        let width = terminal_size::terminal_size().map(|(width, _)| usize::from(width.0));
        self.line(&wrap_notice(&text, width));
    }

    fn tool_row(&self, rows: &str, detail: String) {
        {
            let mut output = self.inner.lock().expect("surface output lock");
            let _ = output.write_tool(rows);
        }
        self.details.remember(detail);
    }

    fn expand_last_tool(&self) {
        let detail = self.details.latest();
        match detail {
            Some(detail) => self.line(&detail),
            None => self.note("no tool call to expand yet"),
        }
    }
}

impl TextOutput for InteractiveOutput {
    fn write(&self, text: &str) {
        let _ = self.inner.lock().expect("surface output lock").write(text);
    }

    fn answer(&self, text: &str) {
        let mut output = self.inner.lock().expect("surface output lock");
        let _ = output.write_answer(text);
    }

    fn line(&self, text: &str) {
        let mut output = self.inner.lock().expect("surface output lock");
        let _ = output.begin_block();
        let _ = output.write(text);
        let _ = output.write("\n");
    }

    fn signal(&self, sequence: &str) {
        let _ = self
            .inner
            .lock()
            .expect("surface output lock")
            .signal(sequence);
    }

    fn live_retry(&self) -> bool {
        true
    }

    fn retry_status(&self, message: Option<(&str, u64)>) {
        self.inner
            .lock()
            .expect("surface output lock")
            .retry_status(message);
    }

    fn goal_status(&self, goal: Option<(u32, Option<u32>)>) {
        self.inner
            .lock()
            .expect("surface output lock")
            .goal_status(goal);
    }

    fn begin_block(&self) {
        let _ = self
            .inner
            .lock()
            .expect("surface output lock")
            .begin_block();
    }

    fn finish(&self) {
        let _ = self.inner.lock().expect("surface output lock").finish();
    }
}

/// A diagnostic. Always stderr, in every mode, with no exception — which is
/// why it does not take a [`Stdout`] and could not use one if it wanted to.
pub fn note(message: &str) {
    let mut err = std::io::stderr();
    let width = err
        .is_terminal()
        .then(terminal_size::terminal_size)
        .flatten()
        .map(|(width, _)| usize::from(width.0));
    write_note(&mut err, message, width);
}

/// Plain human notices share the answer renderer's word and cell layout.
/// A missing terminal width keeps redirected diagnostics byte-for-byte intact.
pub(crate) fn wrap_notice(text: &str, width: Option<usize>) -> String {
    // Zero is an unmeasured surface width, not a one-cell terminal.
    let Some(width) = width.filter(|width| *width > 0) else {
        return text.to_owned();
    };
    text.split('\n')
        .flat_map(|line| {
            let indent = line.bytes().take_while(|byte| *byte == b' ').count();
            let prefix = " ".repeat(indent.min(width.saturating_sub(1)));
            crate::render::Line::plain(&line[indent..])
                .wrap(width.saturating_sub(prefix.len()).max(1))
                .into_iter()
                .map(move |row| format!("{prefix}{}", row.text()))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn write_note(mut err: impl Write, message: &str, width: Option<usize>) {
    // Nothing to be done if the diagnostic channel itself is gone, and a
    // panic here would replace a report with a crash.
    let text = wrap_notice(&format!("{DISPLAY_NAME}: {message}"), width);
    let _ = writeln!(err, "{text}");
}

/// The one sentence every empty listing says: what is missing, and the command
/// that would add the first entry.
///
/// A listing that exits 0 having written nothing at all is indistinguishable,
/// to the person who ran it, from one that is broken. It is a *diagnostic*
/// rather than a row because stdout belongs to the row contracts — `config ls`
/// and `rules ls` have their empty text output fixed by their ADRs, and a
/// prose line on stdout would corrupt every reader of the tab-separated shape.
pub fn empty_listing(missing: &str, add_with: &str) -> String {
    format!("{missing}; add the first with `{add_with}`")
}

/// The live view of a run: one `PublicEvent` per line, as it happens.
///
/// `CLI.md` §4.2 says *live*, and the only way a sink can be live is to write
/// on `publish` — which is what this does, flushing every line, so a consumer
/// reading a pipe sees an event before the turn that produced it has ended.
pub struct JsonlSink<'a> {
    out: &'a Stdout,
}

impl<'a> JsonlSink<'a> {
    pub fn new(out: &'a Stdout) -> Self {
        JsonlSink { out }
    }
}

impl EventSink for JsonlSink<'_> {
    fn publish(&self, event: &PublicEvent, _state: &SessionState) {
        match serde_json::to_string(event) {
            Ok(line) => self.out.line(&line),
            // An event that cannot be serialised is a defect in the schema,
            // not in the pipe, and it is reported rather than dropped —
            // `sink.rs`: absorbing a transport error is not the same as
            // discarding the event that hit it.
            Err(err) => note(&format!("an event could not be serialised: {err}")),
        }
    }
}

/// The default text mode: line-oriented, closer to a Unix tool than a TUI
/// (`SPEC.md` §2.3, `ARCHITECTURE.md` §6).
///
/// Assistant text is written as it streams, with no line of its own, so
/// `ego run "…" > answer.txt` contains the answer and nothing else. Everything
/// that is *about* the run rather than *of* it — tool calls, errors — goes to
/// stderr, for the same reason a progress bar does.
pub struct TextSink<'a> {
    out: &'a dyn TextOutput,
    quiet: bool,
    /// The answer being written, and whether it has left a partial line
    /// outstanding so the run can close it before anything else is printed.
    ///
    /// One mutex over both because they are one fact: what was written last
    /// decides whether a newline is owed, and two locks would let a tool note
    /// interleave between the write and the bookkeeping about it.
    text: Mutex<Streaming>,
    /// The last ceiling and rate source this sink considered, so a
    /// per-generation event does not repeat one warning per generation while
    /// a change in the evidence behind the ceiling remains visible. See
    /// [`ceiling_note`].
    ///
    /// [`ceiling_note`]: TextSink::ceiling_note
    ceiling: Mutex<Option<(CeilingEffect, RateSource)>>,
    /// Tool calls that have started and not finished: what the row needs from
    /// the start event, kept until the completion event arrives.
    calls: Mutex<HashMap<ItemId, StartedCall>>,
    capabilities: Capabilities,
    retry_cause: Mutex<Option<String>>,
    goal: Mutex<Option<ego_core::goal::GoalRecord>>,
    turn_started: Mutex<HashMap<TurnId, Instant>>,
    edited_lines: Mutex<HashMap<TurnId, ego_core::permit::LineChange>>,
    last_cache: Mutex<Option<(ego_core::generator::attempt::CachePrefix, bool)>>,
}

struct StartedCall {
    name: String,
    args: String,
}

fn compact_tokens(count: u64) -> String {
    if count < 1_000 {
        count.to_string()
    } else if count < 100_000 {
        format!("{:.1}K", count as f64 / 1_000.0)
    } else {
        format!("{}K", count / 1_000)
    }
}

/// The state of the answer currently being written.
struct Streaming {
    answer: Answer,
    tools: tool_row::ToolRows,
    open: bool,
}

impl<'a> TextSink<'a> {
    pub fn new(out: &'a dyn TextOutput, quiet: bool, capabilities: Capabilities) -> Self {
        TextSink {
            out,
            quiet,
            text: Mutex::new(Streaming {
                answer: Answer::new(capabilities),
                tools: tool_row::ToolRows::default(),
                open: false,
            }),
            ceiling: Mutex::new(None),
            calls: Mutex::new(HashMap::new()),
            capabilities,
            retry_cause: Mutex::new(None),
            goal: Mutex::new(None),
            turn_started: Mutex::new(HashMap::new()),
            edited_lines: Mutex::new(HashMap::new()),
            last_cache: Mutex::new(None),
        }
    }

    /// Compare reduced goal facts, not provider prose, at the live output boundary.
    /// One path renders set/update, admitted continuation and stop receipts.
    fn goal_receipts(&self, event: &PublicEvent, state: &SessionState) {
        use ego_core::goal::{GoalOwner, GoalState};
        let mut previous = self.goal.lock().expect("goal receipt lock");
        let current = state.goal.as_ref();
        let updated_by_tool = matches!(event, PublicEvent::ItemCompleted {
            item, kind: ItemKind::Tool, ok: true, ..
        } if state.items.get(item)
            .is_some_and(|item| item.tool_name.as_deref() == Some("goal")));
        if previous.as_ref() == current && !updated_by_tool {
            return;
        }
        self.out.goal_status(
            current
                .filter(|goal| goal.state == GoalState::Active)
                .map(|goal| (goal.continuation_turns, goal.continuation_budget)),
        );
        if let Some(goal) = current {
            let prior = previous.as_ref().filter(|prior| prior.id == goal.id);
            let mut receipts = Vec::new();
            if prior
                .is_none_or(|prior| prior.objective != goal.objective || prior.owner != goal.owner)
                || updated_by_tool
            {
                let owner = match goal.owner {
                    GoalOwner::Operator => "operator",
                    GoalOwner::Automated => "automated",
                    GoalOwner::Check => "check",
                };
                receipts.push(format!("Goal ({owner}): {}", goal.objective));
            }
            let allowance = goal
                .continuation_budget
                .map_or_else(|| "pending".into(), |n| n.to_string());
            if prior.is_some_and(|prior| prior.continuation_turns < goal.continuation_turns) {
                receipts.push(format!(
                    "Goal turn {} of {allowance}: {}",
                    goal.continuation_turns,
                    goal.blocker_reason
                        .as_deref()
                        .unwrap_or("condition unavailable")
                ));
            }
            if let Some(reason) = &goal.check_stop
                && prior.is_none_or(|prior| prior.check_stop.as_ref() != Some(reason))
            {
                let next = if goal.state == GoalState::Blocked {
                    "Next: /goal show; after resolving the blocker, /goal set TEXT starts a new goal. /goal achieve and /goal block require an active goal"
                } else {
                    "Next: /goal show, /goal achieve, /goal block"
                };
                receipts.push(format!(
                    "Goal stopped: {} — turns {}/{allowance}; {reason}; last unmet condition: {}. {next}; --goal-turns N sets the allowance when checks first start.",
                    goal.objective, goal.continuation_turns,
                    goal.blocker_reason.as_deref().unwrap_or("none recorded")
                ));
            }
            for receipt in receipts {
                self.finish();
                self.out.note(&receipt);
            }
        }
        *previous = current.cloned();
    }

    /// What an operator has to be told about the ceiling they asked for, and
    /// `None` when the flag does exactly what its spelling says.
    ///
    /// Two of [`CeilingEffect`]'s three values are a flag that behaves
    /// differently from how it reads, and `CLI.md` §5 spells `--budget` as a
    /// **hard cap**: `Inapplicable` is not a cap at all, and a clamped `Binds`
    /// caps at a figure nobody typed. Both are already on
    /// [`PublicEvent::MeteringSelected`] for `--output json/jsonl`; this is the
    /// same fact for the operator who did not ask for a machine-readable
    /// stream, and without it the default mode is exactly the "silently vacuous
    /// flag" that event's own doc says is not acceptable.
    ///
    /// Money is rendered `{amount}{currency}` — the spelling `--budget` itself
    /// accepts — so the figure quoted back can be retyped.
    fn ceiling_note(effect: &CeilingEffect, rates: &RateSource) -> Option<String> {
        let table_risk = match (effect, rates) {
            (CeilingEffect::Binds { .. }, RateSource::Table { revision }) => Some(format!(
                "this budget is enforced using a cached third-party pricing table ({revision}); \
                 stale rates can make the ceiling too low or let the real provider charge exceed it",
                revision = revision.0
            )),
            _ => None,
        };
        match effect {
            CeilingEffect::None => None,
            CeilingEffect::Binds {
                clamped_from: None, ..
            } => table_risk,
            CeilingEffect::Binds {
                limit,
                clamped_from: Some(asked),
            } => {
                let clamp = format!(
                    "the budget of {}{} was reduced to {}{} by a system ceiling",
                    asked.amount, asked.currency, limit.amount, limit.currency
                );
                Some(match table_risk {
                    Some(risk) => format!("{clamp}; {risk}"),
                    None => clamp,
                })
            }
            // A token budget is unaffected and constrains such a row exactly as
            // it constrains any other, which is what the operator should reach
            // for — so the note says so rather than only reporting the loss.
            CeilingEffect::Inapplicable { limit, metering } => Some(format!(
                "the budget of {}{} does not bind this model: it is {}, so no \
                 monetary ceiling can reach it and the run continues until the \
                 provider refuses. A token budget such as `--budget 400k` still \
                 applies",
                limit.amount,
                limit.currency,
                match metering {
                    MeteringKind::Subscription => "paid for by a subscription",
                    MeteringKind::Unmetered => "not metered — a daemon serving its own weights",
                    // Unreachable as the gate builds it, and phrased rather
                    // than panicked: a diagnostic that aborts the run it was
                    // meant to explain is worse than a vague diagnostic.
                    MeteringKind::Currency => "metered in a currency this ceiling cannot bind",
                }
            )),
        }
    }

    /// Draw whatever is still held back, and end the line if one is open.
    ///
    /// Called at the end of a run and before every diagnostic. On a screen the
    /// first half matters as much as the second: a tool note is about to appear
    /// on stderr, and prose the model finished before calling that tool should
    /// be on the screen *above* the note rather than after it.
    pub fn finish(&self) {
        let mut text = self
            .text
            .lock()
            .expect("a poisoned lock means a writer panicked mid-line");
        self.finish_text(&mut text);
        text.tools.separate();
    }

    fn finish_text(&self, text: &mut Streaming) {
        text.answer
            .resize(self.capabilities.with_current_width().width);
        let rest = text.answer.finish();
        if !rest.is_empty() {
            text.tools.separate();
            self.out.answer(&rest);
            text.open = !rest.ends_with('\n');
        }
        if text.open {
            self.out.line("");
            text.open = false;
        }
    }

    fn stream(&self, delta: &str) {
        let mut text = self
            .text
            .lock()
            .expect("a poisoned lock means a writer panicked mid-line");
        text.answer
            .resize(self.capabilities.with_current_width().width);
        let ready = text.answer.push(delta);
        if ready.is_empty() {
            return;
        }
        text.tools.separate();
        self.out.answer(&ready);
        text.open = !ready.ends_with('\n');
    }
}

impl EventSink for TextSink<'_> {
    fn cache_observed(
        &self,
        prefix: &ego_core::generator::attempt::CachePrefix,
        usage: Option<ego_core::state::Usage>,
    ) {
        if !self.capabilities.on_screen() || self.quiet {
            return;
        }
        let mut last = self.last_cache.lock().expect("cache observation lock");
        let Some(usage) = usage.filter(|usage| {
            usage.cache_read_tokens > 0
                || usage.uncached_input_tokens > 0
                || usage.cache_write_tokens > 0
                || usage.cache_write_extended_tokens > 0
        }) else {
            if last
                .as_ref()
                .is_some_and(|(previous, _)| previous != prefix)
            {
                *last = None;
            }
            return;
        };
        let hit = usage.cache_read_tokens > 0;
        let probable_loss = last
            .as_ref()
            .is_some_and(|(previous, hit_before)| previous == prefix && *hit_before && !hit);
        *last = Some((prefix.clone(), hit));
        drop(last);
        if probable_loss {
            self.finish();
            self.out.note("cache reuse was reported on an earlier request with the same marked input, but not this one; cached entries may have expired or changed at the provider");
        }
    }

    fn publish(&self, event: &PublicEvent, state: &SessionState) {
        self.goal_receipts(event, state);
        if self.capabilities.on_screen()
            && self.out.live_retry()
            && matches!(
                event,
                PublicEvent::ItemStarted { .. }
                    | PublicEvent::ItemDelta { .. }
                    | PublicEvent::TurnCompleted { .. }
            )
        {
            self.out.retry_status(None);
            if let Some(cause) = self.retry_cause.lock().expect("retry cause lock").take() {
                self.finish();
                self.out.note(&format!("{cause} — retry finished"));
            }
        }
        match event {
            PublicEvent::TurnStarted { turn } if self.capabilities.on_screen() && !self.quiet => {
                self.turn_started
                    .lock()
                    .expect("turn clock lock")
                    .insert(*turn, Instant::now());
            }
            PublicEvent::UsageUpdated { usage_delta, .. }
                if self.capabilities.on_screen()
                    && !self.quiet
                    && usage_delta.cache_read_tokens > 0 =>
            {
                self.finish();
                self.out.note(&format!(
                    "cache hit: {} tokens",
                    compact_tokens(usage_delta.cache_read_tokens)
                ));
            }
            PublicEvent::TurnCompleted { turn, ok, .. }
                if self.capabilities.on_screen() && !self.quiet =>
            {
                let started = self
                    .turn_started
                    .lock()
                    .expect("turn clock lock")
                    .remove(turn);
                let lines = self
                    .edited_lines
                    .lock()
                    .expect("edit count lock")
                    .remove(turn);
                if let Some(started) = started {
                    self.finish();
                    let elapsed = tool_row::duration(started.elapsed());
                    let clock = chrono::Local::now().format("%-I:%M %p");
                    let outcome = if *ok { "done" } else { "stopped" };
                    let mut receipt = format!("✻ Turn elapsed {elapsed} · {outcome} {clock}");
                    if let Some(lines) = lines {
                        let (green, red, reset) = if self.capabilities.coloured() {
                            ("\x1b[32m", "\x1b[31m", "\x1b[0m")
                        } else {
                            ("", "", "")
                        };
                        receipt.push_str(&format!(
                            " · tracked edits {green}+{}{reset} {red}-{}{reset}",
                            lines.added, lines.removed
                        ));
                    }
                    self.out.note(&receipt);
                }
            }
            PublicEvent::PlanUpdated { .. } if !self.quiet => {
                self.finish();
                for step in &state.plan.steps {
                    self.out
                        .note(&format!("[{:?}] {}: {}", step.status, step.id, step.text));
                }
                if state.plan.steps.is_empty() {
                    self.out.note("Plan cleared");
                }
            }
            PublicEvent::SessionStarted { mcp_errors, .. } => {
                for error in mcp_errors {
                    self.finish();
                    self.out.note(&format!("{error}. Inspect with ego mcp ls."));
                }
            }
            // `AgentMessage` and not "either message kind": stdout is the
            // answer, and a user prompt is what the caller already typed. It
            // never arrives as a delta anyway — `UserPrompt` projects to one
            // `ItemCompleted` — so this narrowing describes the stream rather
            // than filtering it.
            PublicEvent::ItemDelta {
                kind: ItemKind::AgentMessage,
                text,
                ..
            } => self.stream(text),
            // One row per call, drawn when the call finishes: a row needs the
            // outcome and the duration, and the start event has neither.
            // `ask_question` gets none: the selector already shows the
            // question as prompt and options, and a row would repeat the call's
            // JSON after the answer.
            PublicEvent::ItemStarted {
                kind: ItemKind::Tool,
                item,
                name: Some(name),
                text,
                ..
            } if !self.quiet && name != ego_core::question::ASK_QUESTION_TOOL => {
                self.calls
                    .lock()
                    .expect("a poisoned lock means a writer panicked mid-line")
                    .insert(
                        *item,
                        StartedCall {
                            name: name.clone(),
                            args: text.clone().unwrap_or_default(),
                        },
                    );
            }
            PublicEvent::ItemCompleted {
                kind: ItemKind::Tool,
                turn,
                item,
                ok,
                text,
            } if !self.quiet => {
                if *ok
                    && self.capabilities.on_screen()
                    && let Some(change) = state
                        .items
                        .get(item)
                        .and_then(|item| item.tool_result.as_ref())
                        .and_then(|result| result.line_change())
                {
                    let mut lines = self.edited_lines.lock().expect("edit count lock");
                    let total = lines.entry(*turn).or_insert(ego_core::permit::LineChange {
                        added: 0,
                        removed: 0,
                    });
                    total.added = total.added.saturating_add(change.added);
                    total.removed = total.removed.saturating_add(change.removed);
                }
                let started = self
                    .calls
                    .lock()
                    .expect("a poisoned lock means a writer panicked mid-line")
                    .remove(item);
                if let Some(started) = started {
                    let mut streaming = self.text.lock().expect("streaming output lock");
                    self.finish_text(&mut streaming);
                    let record = streaming.tools.record(
                        &Finished {
                            name: &started.name,
                            args: &started.args,
                            ok: *ok,
                            result: text.as_deref().unwrap_or(""),
                            timing: state.items.get(item).and_then(|item| item.tool_timing),
                        },
                        self.capabilities.with_current_width(),
                    );
                    self.out.tool_row(&record.rows, record.detail);
                }
            }
            // Deliberately **not** gated on `quiet`. `-q` is presentation —
            // CLI.md §1.3 pairs it with `--no-color` and calls it "quiet
            // progress" — and a guarantee the operator asked for and did not
            // get is not progress. Suppressing it would put the flag's silence
            // back exactly where this arm removes it from.
            PublicEvent::MeteringSelected { ceiling, rates, .. } => {
                let mut last = self
                    .ceiling
                    .lock()
                    .expect("a poisoned lock means a writer panicked mid-line");
                // Announced on *change*, not once: the event is per-generation
                // because a fallback chain can switch rows mid-turn, and a
                // switch from a metered row to an unmetered one changes whether
                // the budget binds. Repeating the identical warning for every
                // generation of a long run would be noise; dropping the second
                // one when it says something new would be the original bug.
                if last.as_ref() == Some(&(ceiling.clone(), rates.clone())) {
                    return;
                }
                let message = Self::ceiling_note(ceiling, rates);
                *last = Some((ceiling.clone(), rates.clone()));
                drop(last);
                if let Some(message) = message {
                    self.finish();
                    self.out.note(&message);
                }
            }
            PublicEvent::ProviderRetry {
                attempt,
                limit,
                delay_ms,
                cause,
                recovery_remaining_ms,
                ..
            } if !self.quiet || recovery_remaining_ms.is_some() => {
                self.finish();
                let counter = if let Some(remaining) = recovery_remaining_ms {
                    format!(
                        "attempt {attempt}, recovery horizon {:.0}s; retry now: /retry or Meta-r",
                        *remaining as f64 / 1_000.0
                    )
                } else {
                    format!("attempt {attempt}/{limit}")
                };
                let message = format!(
                    "{cause} — connection problem, retrying in {:.1}s ({counter})",
                    *delay_ms as f64 / 1_000.0
                );
                if self.capabilities.on_screen() && self.out.live_retry() {
                    *self.retry_cause.lock().expect("retry cause lock") = Some(cause.clone());
                    self.out.retry_status(Some((&message, *delay_ms)));
                } else {
                    self.out.note(&message);
                }
            }
            PublicEvent::Error { message, .. }
                if state
                    .goal
                    .as_ref()
                    .and_then(|goal| goal.check_stop.as_deref())
                    == Some(message.as_str()) => {}
            PublicEvent::Error { message, kind, .. } => {
                self.finish();
                if self.capabilities.on_screen()
                    && self.out.live_retry()
                    && *kind == ego_core::generator::types::ErrorKind::Transient
                {
                    *self.retry_cause.lock().expect("retry cause lock") = Some(message.clone());
                    self.out.retry_status(Some((message, 0)));
                } else {
                    self.out.note(message);
                }
            }
            _ => {}
        }
    }
}

/// The occupancy half of §4.1's result, present only when something reported
/// it. An absent key says "nobody has told us"; a zero would say the context
/// is empty.
#[derive(Debug, Serialize)]
pub struct ContextReport {
    pub used_tokens: u64,
    pub provenance: OccupancyProvenance,
}

/// The single object `--output json` writes at completion (`CLI.md` §4.1).
///
/// `ok` and `exit_reason` are both read off one [`RunTermination`], so they
/// cannot disagree — §4.2's "one error field" rule, and the Claude
/// `subtype:"success"` + `is_error:true` contradiction it was written against.
/// `usage` and `cost` are `SessionState`'s own values, serialised rather than
/// rebuilt: the four priced categories and the `unstarted|known|unknown` state
/// are contracts core already owns.
#[derive(Debug, Serialize)]
pub struct Report {
    pub v: u32,
    pub session: SessionId,
    pub result: serde_json::Value,
    pub ok: bool,
    pub exit_reason: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goal: Option<GoalReport>,
    /// Stable sanitized MCP startup diagnostics, absent when every selected
    /// server was admitted.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mcp_errors: Vec<String>,
    /// The action a permission-refused run was denied. §4.4 requires it: a
    /// script handed exit 3 and no subject has been told nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub denied: Option<String>,
    /// Who refused it, when somebody did.
    ///
    /// Beside [`denied`](Self::denied) rather than replacing it: the action
    /// and the authority are two facts, and folding one into the other would
    /// break every consumer of a documented §4.4 field for no gain.
    ///
    /// Absent means **nobody answered** — §6's evaluation order was exhausted
    /// — which is unambiguous alongside `exit_reason: "permission"`. That is
    /// the one place [`DeniedBy`](ego_core::run::DeniedBy)'s explicit `Nobody`
    /// arm becomes an `Option`, and the one place the `Option` earns its keep.
    ///
    /// A new key and no version bump. §4.2 already ratifies presence as this
    /// format's detection mechanism — error arrays "present only when
    /// non-empty — a CI gate is a presence check" — so a key that appears only
    /// on exit 3 is additive under a rule that was written down rather than
    /// under one asserted here. See
    /// `ARCHITECTURE.md` §4.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub denied_by: Option<Authority>,
    pub turns: usize,
    pub usage: ego_core::state::Usage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<ContextReport>,
    pub cost: CumulativeCost,
}

/// Goal outcome is separate from whether the invocation completed successfully.
#[derive(Debug, Serialize)]
pub struct GoalReport {
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl GoalReport {
    fn of(goal: &ego_core::goal::GoalRecord) -> Self {
        use ego_core::goal::GoalState;
        let reason = goal
            .check_stop
            .clone()
            .or_else(|| goal.blocker_reason.clone());
        // The durable stop event carries a reason, not a typed stop category.
        let exhausted = goal.check_stop.as_deref().is_some_and(|reason| {
            reason == "goal check token budget reached"
                || reason
                    == format!(
                        "goal continuation budget exhausted ({})",
                        goal.continuation_turns
                    )
        });
        let status = match goal.state {
            GoalState::Achieved => "achieved",
            _ if exhausted => "exhausted",
            GoalState::Blocked => "stopped",
            GoalState::Active if goal.check_stop.is_some() => "stopped",
            GoalState::Active => "active",
        };
        Self {
            status,
            reason: if goal.state == GoalState::Achieved {
                None
            } else {
                reason
            },
        }
    }
}

impl Report {
    pub fn of(
        session: SessionId,
        state: &SessionState,
        termination: &RunTermination,
        result: serde_json::Value,
    ) -> Self {
        // Both read off the one `RunTermination`, in one match, for the same
        // reason `ok` and `exit_reason` are: two keys about one ending that
        // were derived separately are two keys that can disagree.
        let (denied, denied_by) = match termination {
            RunTermination::PermissionRequired { action, by } => (
                Some(action.clone()),
                match by {
                    DeniedBy::Decided(authority) => Some(authority.clone()),
                    DeniedBy::Nobody => None,
                },
            ),
            _ => (None, None),
        };
        Report {
            v: PUBLIC_EVENT_VERSION,
            session,
            result,
            ok: matches!(termination, RunTermination::Done),
            exit_reason: termination.exit_reason(),
            goal: state.goal.as_ref().map(GoalReport::of),
            mcp_errors: Vec::new(),
            denied,
            denied_by,
            turns: state.turns.len(),
            usage: state.usage,
            context: state.context.used_tokens.zip(state.context.provenance).map(
                |(used_tokens, provenance)| ContextReport {
                    used_tokens,
                    provenance,
                },
            ),
            cost: state.cost.clone(),
        }
    }
}

/// The one arm of §4.1's denial mapping the binary cannot reach.
///
/// `DeniedBy::Nobody` needs a call the mode declines to auto-approve, and
/// `ModeApprover` declines only mutating operations (`approver.rs`: "a read
/// inside the workspace that stopped the run to ask would be the prompt
/// fatigue `ARCHITECTURE.md:238` counts as a security bug"). This build's
/// `builtins()` are `read`, `glob` and `grep` — every one of them
/// non-mutating — so no argv reaches it, and a test that spawned `ego` could
/// not produce one. It arrives with the write tool.
///
/// That is exactly why the arm is asserted here rather than left to the
/// process tests: a mapping nothing exercises is an assumption with a match
/// arm's syntax, and the change that first exercises it — the write tool — is
/// the one that must not discover it was wrong.
#[cfg(test)]
mod tests {
    use ego_core::event::id::SessionId;
    use ego_core::state::SessionState;

    use super::*;

    fn report(termination: &RunTermination) -> serde_json::Value {
        let report = Report::of(
            SessionId::new(),
            &SessionState::default(),
            termination,
            serde_json::Value::String(String::new()),
        );
        serde_json::to_value(report).expect("§4.1's object serialises")
    }

    #[test]
    fn a_diagnostic_uses_the_human_facing_product_name_byte_for_byte() {
        let mut err = Vec::new();
        write_note(&mut err, "something failed", None);

        assert_eq!(err, "εgo: something failed\n".as_bytes());
    }

    // Catches: a terminal width leaks into redirected diagnostic bytes.
    #[test]
    fn redirected_diagnostics_keep_long_lines_and_whitespace() {
        let message =
            "a long diagnostic with instructions that must stay on one logical line\n  next line";
        let mut err = Vec::new();
        write_note(&mut err, message, None);
        assert_eq!(String::from_utf8(err).unwrap(), format!("εgo: {message}\n"));
    }

    // Catches: unmeasured surface geometry splits a permission receipt into characters.
    #[test]
    fn unmeasured_notice_width_preserves_receipt_bytes() {
        let receipt = "Allowed run seq 1 3\n  next receipt";
        assert_eq!(wrap_notice(receipt, Some(0)), receipt);
        assert_eq!(wrap_notice(receipt, None), receipt);
    }

    // Catches: multiline notice wrapping drops blank rows or measures Unicode in bytes.
    #[test]
    fn notices_preserve_blank_rows_and_wrap_unicode_in_cells() {
        assert_eq!(
            wrap_notice("\n  hint\nεgo: 日本語 café text continues\n", Some(20)),
            "\n  hint\nεgo: 日本語 café\ntext continues\n"
        );
    }

    // Catches: wrapping long key hints strips their header-alignment indentation.
    #[test]
    fn wrapped_notices_keep_leading_indent_on_every_row() {
        assert_eq!(
            wrap_notice("     Ctrl+B backgrounds · Esc interrupts", Some(25)),
            "     Ctrl+B backgrounds ·\n     Esc interrupts"
        );
    }

    // Catches: an oversized diagnostic token bypasses the shared bounded fallback.
    #[test]
    fn diagnostic_tokens_wider_than_the_terminal_still_fit() {
        let mut err = Vec::new();
        write_note(&mut err, "abcdefghijklmnopqrstuvwxyz", Some(20));
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "εgo:\nabcdefghijklmnopqrst\nuvwxyz\n"
        );
    }

    #[test]
    fn a_question_nobody_answered_names_the_action_and_no_authority() {
        let object = report(&RunTermination::PermissionRequired {
            action: "edit may not write /ws/x".to_string(),
            by: DeniedBy::Nobody,
        });
        let object = object.as_object().expect("§4.1 publishes an object");

        assert!(
            !object.contains_key("goal"),
            "no goal must not acquire an outcome"
        );
        assert_eq!(object["exit_reason"], "permission");
        assert_eq!(object["denied"], "edit may not write /ws/x");
        // Absent, not null. §4.2 ratifies presence as this format's detection
        // mechanism — error arrays "present only when non-empty — a CI gate is
        // a presence check" — and a null would be a value every consumer has
        // to special-case in order to learn the same thing.
        assert!(
            !object.contains_key("denied_by"),
            "nobody decided, so there is no authority to name"
        );
    }

    /// The other half, at the same level: absence means *nobody answered* and
    /// nothing else. If a decided refusal could also drop the key, absence
    /// would be ambiguous and the whole field would say nothing.
    #[test]
    fn a_refusal_somebody_made_always_names_them() {
        let object = report(&RunTermination::PermissionRequired {
            action: "read may not read /etc/hosts".to_string(),
            by: DeniedBy::Decided(Authority::Boundary),
        });

        assert_eq!(object["denied_by"], serde_json::json!({ "by": "boundary" }));
    }

    /// Everything a sink asked its output to draw.
    #[derive(Default)]
    struct Recorded {
        notes: Mutex<Vec<String>>,
        rows: Mutex<Vec<(String, String)>>,
    }

    impl Diagnostics for Recorded {
        fn note(&self, message: &str) {
            self.notes.lock().unwrap().push(message.to_owned());
        }

        fn tool_row(&self, rows: &str, detail: String) {
            self.rows.lock().unwrap().push((rows.to_owned(), detail));
        }
    }

    impl TextOutput for Recorded {
        fn write(&self, _text: &str) {}
        fn line(&self, _text: &str) {}
    }

    fn started(item: ItemId, name: &str, args: &str) -> PublicEvent {
        PublicEvent::ItemStarted {
            turn: ego_core::event::id::TurnId::new(),
            item,
            kind: ItemKind::Tool,
            name: Some(name.to_owned()),
            text: Some(args.to_owned()),
        }
    }

    /// Catches: reporting a cache hit as a raw token count or dropping it from
    /// the interactive transcript after a provider reports reused input.
    #[test]
    fn interactive_usage_reports_compact_cache_hits_only_when_read_tokens_exist() {
        let out = Recorded::default();
        let mut caps = Capabilities::plain();
        caps.osc = crate::render::caps::OscProfile::Standard;
        let sink = TextSink::new(&out, false, caps);
        let state = SessionState::default();
        let turn = ego_core::event::id::TurnId::new();
        for cached in [0, 18_400] {
            sink.publish(
                &PublicEvent::UsageUpdated {
                    turn,
                    item: None,
                    usage_delta: ego_core::state::Usage {
                        cache_read_tokens: cached,
                        ..ego_core::state::Usage::IDENTITY
                    },
                    cost_delta: ego_core::cost::CostDelta::Unknown(
                        ego_core::cost::UnknownCost::NoPricingQuote,
                    ),
                },
                &state,
            );
        }
        assert_eq!(*out.notes.lock().unwrap(), ["cache hit: 18.4K tokens"]);
    }

    /// Catches: treating a first miss or a different cache prefix as a reset,
    /// and repeating the warning for every miss after one prior hit.
    #[test]
    fn cache_warning_requires_a_prior_hit_on_the_same_marked_prefix() {
        use ego_core::generator::attempt::CachePrefix;
        let out = Recorded::default();
        let mut caps = Capabilities::plain();
        caps.osc = crate::render::caps::OscProfile::Standard;
        let sink = TextSink::new(&out, false, caps);
        let original = CachePrefix {
            session: SessionId::new(),
            provider: "anthropic".into(),
            model: "model-a".into(),
            digest: "marked-a".into(),
        };
        let different = CachePrefix {
            model: "model-b".into(),
            ..original.clone()
        };
        let usage = |read, uncached| ego_core::state::Usage {
            cache_read_tokens: read,
            uncached_input_tokens: uncached,
            ..ego_core::state::Usage::IDENTITY
        };
        sink.cache_observed(&original, Some(usage(0, 800)));
        sink.cache_observed(&original, Some(usage(400, 0)));
        sink.cache_observed(&different, Some(usage(0, 800)));
        sink.cache_observed(&original, Some(usage(0, 800)));
        assert!(out.notes.lock().unwrap().is_empty());
        sink.cache_observed(&original, Some(usage(400, 0)));
        sink.cache_observed(&original, Some(usage(0, 800)));
        sink.cache_observed(&original, Some(usage(0, 800)));
        sink.cache_observed(&original, Some(usage(400, 0)));
        sink.cache_observed(
            &original,
            Some(ego_core::state::Usage {
                cache_write_tokens: 800,
                ..ego_core::state::Usage::IDENTITY
            }),
        );
        let notes = out.notes.lock().unwrap();
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(notes[0].contains("may have expired"), "{notes:?}");
    }

    /// Catches: a missing usage report being read as a zero-token miss, or a
    /// cache warning entering a headless or quiet transcript.
    #[test]
    fn cache_warning_needs_reported_input_and_an_interactive_loud_surface() {
        use ego_core::generator::attempt::CachePrefix;
        let prefix = CachePrefix {
            session: SessionId::new(),
            provider: "anthropic".into(),
            model: "model-a".into(),
            digest: "same".into(),
        };
        let hit = ego_core::state::Usage {
            cache_read_tokens: 500,
            ..ego_core::state::Usage::IDENTITY
        };
        let miss = ego_core::state::Usage {
            uncached_input_tokens: 500,
            ..ego_core::state::Usage::IDENTITY
        };
        for (quiet, interactive) in [(false, true), (true, true), (false, false)] {
            let out = Recorded::default();
            let mut caps = Capabilities::plain();
            if interactive {
                caps.osc = crate::render::caps::OscProfile::Standard;
            }
            let sink = TextSink::new(&out, quiet, caps);
            sink.cache_observed(&prefix, Some(hit));
            sink.cache_observed(&prefix, None);
            sink.cache_observed(&prefix, Some(ego_core::state::Usage::IDENTITY));
            assert!(out.notes.lock().unwrap().is_empty());
            sink.cache_observed(&prefix, Some(miss));
            sink.cache_observed(&prefix, Some(hit));
            sink.cache_observed(&prefix, Some(miss));
            assert_eq!(
                out.notes.lock().unwrap().len(),
                2 * usize::from(!quiet && interactive)
            );
        }
    }

    /// Catches: a finished turn disappearing without a visible receipt, or
    /// a headless answer receiving terminal-only status text.
    #[test]
    fn interactive_turn_completion_prints_elapsed_receipt_without_changing_headless_output() {
        let out = Recorded::default();
        let mut caps = Capabilities::plain();
        caps.osc = crate::render::caps::OscProfile::Standard;
        let interactive = TextSink::new(&out, false, caps);
        let headless = TextSink::new(&out, false, Capabilities::plain());
        let state = SessionState::default();
        let turn = ego_core::event::id::TurnId::new();
        let start = PublicEvent::TurnStarted { turn };
        let end = PublicEvent::TurnCompleted {
            turn,
            ok: true,
            exit_reason: ego_core::event::domain::ExitReason::Completed,
        };
        interactive.publish(&start, &state);
        interactive.publish(&end, &state);
        headless.publish(&start, &state);
        headless.publish(&end, &state);
        let notes = out.notes.lock().unwrap();
        assert_eq!(notes.len(), 1);
        assert!(notes[0].starts_with("✻ Turn elapsed "), "{notes:?}");
        assert!(notes[0].contains(" · done "), "{notes:?}");
    }

    /// Catches: a terminal receipt loses the diff-like signs or leaks ANSI
    /// sequences when colour was disabled.
    #[test]
    fn turn_receipt_colours_only_observed_edit_counts() {
        for (colour, green, red) in [(false, "", ""), (true, "\x1b[32m", "\x1b[31m")] {
            let out = Recorded::default();
            let mut caps = Capabilities::plain();
            caps.osc = crate::render::caps::OscProfile::Standard;
            if colour {
                caps.depth = crate::render::caps::Depth::Ansi16;
            }
            let sink = TextSink::new(&out, false, caps);
            let state = SessionState::default();
            let turn = TurnId::new();
            sink.publish(&PublicEvent::TurnStarted { turn }, &state);
            sink.edited_lines.lock().unwrap().insert(
                turn,
                ego_core::permit::LineChange {
                    added: 3,
                    removed: 1,
                },
            );
            sink.publish(
                &PublicEvent::TurnCompleted {
                    turn,
                    ok: true,
                    exit_reason: ego_core::event::domain::ExitReason::Completed,
                },
                &state,
            );
            let notes = out.notes.lock().unwrap();
            let reset = if colour { "\x1b[0m" } else { "" };
            assert!(
                notes[0].contains(&format!("tracked edits {green}+3{reset} {red}-1{reset}")),
                "{notes:?}"
            );
        }
    }

    /// Catches: counting metadata from a failed call, or showing a measured
    /// zero for a successful write whose line changes were never observed.
    #[test]
    fn receipt_reads_only_successful_measured_tool_results_from_reduced_state() {
        use ego_core::event::domain::{DomainEvent, Envelope, EventKind, ExitReason};
        use ego_core::event::id::{EventId, Producer, Seq};
        use ego_core::event::time::Timestamp;
        use ego_core::permit::{LineChange, ToolResult};

        let out = Recorded::default();
        let mut caps = Capabilities::plain();
        caps.osc = crate::render::caps::OscProfile::Standard;
        let sink = TextSink::new(&out, false, caps);
        let turn = TurnId::new();
        let mut state = SessionState::default();
        sink.publish(&PublicEvent::TurnStarted { turn }, &state);
        let mut seq = Seq::ZERO;
        for (ok, change) in [
            (
                true,
                Some(LineChange {
                    added: 3,
                    removed: 1,
                }),
            ),
            (
                false,
                Some(LineChange {
                    added: 9,
                    removed: 9,
                }),
            ),
            (true, None),
        ] {
            let item = ItemId::new();
            let result = match change {
                Some(change) => ToolResult::builtin("done").with_line_change(change),
                None => ToolResult::builtin("done"),
            };
            let event = DomainEvent {
                env: Envelope {
                    seq: seq.next(),
                    id: EventId::new(),
                    turn,
                    item: Some(item),
                    parent: None,
                    ts: Timestamp::from_millis(0),
                    producer: Producer::Main,
                },
                kind: EventKind::ToolCallCompleted { ok, result },
            };
            state.apply(&event);
            sink.publish(&ego_core::public::project_public(&event).unwrap(), &state);
        }
        sink.publish(
            &PublicEvent::TurnCompleted {
                turn,
                ok: true,
                exit_reason: ExitReason::Completed,
            },
            &state,
        );
        let notes = out.notes.lock().unwrap();
        assert!(notes[0].contains("tracked edits +3 -1"), "{notes:?}");
        assert!(!notes[0].contains("+12"), "{notes:?}");
    }

    #[test]
    fn quiet_interactive_turn_does_not_print_progress_receipt() {
        let out = Recorded::default();
        let mut caps = Capabilities::plain();
        caps.osc = crate::render::caps::OscProfile::Standard;
        let sink = TextSink::new(&out, true, caps);
        let state = SessionState::default();
        let turn = TurnId::new();
        sink.publish(&PublicEvent::TurnStarted { turn }, &state);
        sink.publish(
            &PublicEvent::TurnCompleted {
                turn,
                ok: true,
                exit_reason: ego_core::event::domain::ExitReason::Completed,
            },
            &state,
        );
        assert!(out.notes.lock().unwrap().is_empty());
    }

    fn completed(item: ItemId, ok: bool, result: &str) -> PublicEvent {
        PublicEvent::ItemCompleted {
            turn: ego_core::event::id::TurnId::new(),
            item,
            kind: ItemKind::Tool,
            ok,
            text: Some(result.to_owned()),
        }
    }

    #[test]
    fn terminal_plan_rows_use_reduced_state_and_respect_quiet_progress() {
        // Catches: terminal renders stale operation payloads instead of the authoritative plan.
        use ego_core::plan::{PlanStep, PlanUpdate, StepStatus};
        let mut state = SessionState::default();
        state.plan.steps = vec![PlanStep {
            id: "stable".into(),
            text: "Verify output".into(),
            status: StepStatus::Blocked,
        }];
        let event = PublicEvent::PlanUpdated {
            turn: ego_core::event::id::TurnId::new(),
            update: PlanUpdate::Step {
                id: "stable".into(),
                status: StepStatus::Blocked,
            },
        };
        let out = Recorded::default();
        let sink = TextSink::new(&out, false, Capabilities::plain());
        sink.publish(&event, &state);
        assert_eq!(
            *out.notes.lock().unwrap(),
            ["[Blocked] stable: Verify output"]
        );
        state.plan.steps.clear();
        sink.publish(&event, &state);
        assert_eq!(out.notes.lock().unwrap().last().unwrap(), "Plan cleared");
        let quiet_out = Recorded::default();
        TextSink::new(&quiet_out, true, Capabilities::plain()).publish(&event, &state);
        assert!(quiet_out.notes.lock().unwrap().is_empty());
    }

    #[test]
    fn a_tool_call_is_drawn_once_when_it_finishes_and_never_as_raw_json() {
        let out = Recorded::default();
        let sink = TextSink::new(&out, false, Capabilities::plain());
        let state = SessionState::default();
        let item = ItemId::new();

        sink.publish(&started(item, "read", r#"{"path":"a.rs"}"#), &state);
        assert!(
            out.rows.lock().unwrap().is_empty(),
            "a row needs an outcome"
        );
        sink.publish(&completed(item, true, "one\ntwo\n"), &state);

        let rows = out.rows.lock().unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].0 == "✓ read a.rs · 2 lines", "{:?}", rows[0].0);
        assert!(!rows[0].0.contains('{'), "{:?}", rows[0].0);
        assert!(out.notes.lock().unwrap().is_empty());
    }

    /// Two calls in flight finish in the opposite order: each row carries its
    /// own call's name, matched by item and not by arrival.
    #[test]
    fn overlapping_calls_are_matched_to_their_own_start() {
        let out = Recorded::default();
        let sink = TextSink::new(&out, false, Capabilities::plain());
        let state = SessionState::default();
        let (first, second) = (ItemId::new(), ItemId::new());

        sink.publish(&started(first, "read", r#"{"path":"first.rs"}"#), &state);
        sink.publish(&started(second, "read", r#"{"path":"second.rs"}"#), &state);
        sink.publish(&completed(second, true, "x"), &state);
        sink.publish(&completed(first, false, "denied"), &state);

        let rows = out.rows.lock().unwrap();
        assert!(rows[0].0.starts_with("✓ read second.rs"), "{:?}", rows[0].0);
        assert!(rows[1].0.starts_with("✗ read first.rs"), "{:?}", rows[1].0);
    }

    // Catches: child groups lose ownership after task/parent/prose interleaving,
    // or expand detail depends on an earlier task header still being visible.
    #[test]
    fn child_tool_groups_keep_ownership_across_interleaved_output() {
        let out = Recorded::default();
        let sink = TextSink::new(&out, false, Capabilities::plain());
        let state = SessionState::default();
        for name in [
            "alpha › exec",
            "alpha › read",
            "beta › read",
            "alpha › read",
            "read",
            "alpha › read",
        ] {
            let item = ItemId::new();
            sink.publish(
                &started(item, name, r#"{"path":"a.rs","argv":["printf","one"]}"#),
                &state,
            );
            sink.publish(&completed(item, true, "one\ntwo\nthree"), &state);
        }
        sink.stream("parent prose\n\n");
        let item = ItemId::new();
        sink.publish(&started(item, "alpha › read", r#"{"path":"b.rs"}"#), &state);
        sink.publish(&completed(item, true, "hello"), &state);
        let rows = out.rows.lock().unwrap();
        let shown = rows
            .iter()
            .map(|(rows, _)| rows.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let headers: Vec<_> = shown.lines().filter(|line| line.contains('↳')).collect();
        assert_eq!(
            headers,
            [
                "  ↳ task alpha",
                "  ↳ task beta",
                "  ↳ task alpha",
                "  ↳ task alpha",
                "  ↳ task alpha"
            ]
        );
        assert!(!shown.contains("duration unavailable"));
        assert!(
            rows[1].1.starts_with("  ↳ task alpha\n    ✓ read a.rs"),
            "expand detail must be self-contained: {:?}",
            rows[1].1
        );
        assert!(
            !rows[1].0.contains("alpha"),
            "same-task continuation repeats header"
        );
    }

    #[test]
    fn quiet_draws_no_tool_rows_and_a_stray_completion_draws_none() {
        let out = Recorded::default();
        let quiet = TextSink::new(&out, true, Capabilities::plain());
        let state = SessionState::default();
        let item = ItemId::new();
        quiet.publish(&started(item, "read", r#"{"path":"a.rs"}"#), &state);
        quiet.publish(&completed(item, true, "x"), &state);

        let loud = TextSink::new(&out, false, Capabilities::plain());
        loud.publish(&completed(ItemId::new(), true, "x"), &state);
        assert!(out.rows.lock().unwrap().is_empty());
    }

    fn interactive() -> (InteractiveOutput, crossbeam::channel::Receiver<String>) {
        let (sender, receiver) = crossbeam::channel::unbounded();
        let output = InteractiveOutput::new(SurfaceOutput::new(sender, CancelToken::new()));
        (output, receiver)
    }

    #[test]
    fn expand_shows_the_latest_call_and_not_an_earlier_one() {
        let (output, rows) = interactive();
        output.tool_row("row one", "detail one".to_owned());
        output.tool_row("row two", "detail two".to_owned());
        assert_eq!(
            rows.try_iter().collect::<Vec<_>>(),
            ["\n", "row one\n", "\n", "row two\n"]
        );

        output.expand_last_tool();
        assert_eq!(rows.try_iter().collect::<Vec<_>>(), ["\n", "detail two\n"]);
    }

    #[test]
    fn expand_with_nothing_kept_says_so_instead_of_printing_nothing() {
        let (output, rows) = interactive();
        output.expand_last_tool();
        assert_eq!(
            rows.try_iter().collect::<Vec<_>>(),
            ["εgo: no tool call to expand yet\n"]
        );
    }

    /// Detail is a bounded log: a long session does not grow it without limit.
    #[test]
    fn only_the_most_recent_calls_keep_their_detail() {
        let (output, _rows) = interactive();
        for call in 0..(EXPANDABLE_CALLS + 5) {
            output.tool_row("row", format!("detail {call}"));
        }
        assert_eq!(output.details.0.lock().unwrap().len(), EXPANDABLE_CALLS);
        assert_eq!(
            output.details.0.lock().unwrap().front().unwrap(),
            "detail 5"
        );
    }
}
