//! Everything that reaches a terminal or a pipe: the three `--output` modes
//! of `CLI.md` §4, and the rule that stdout is protocol-clean.
//!
//! §4.5: "stdout is protocol-clean in json/jsonl/acp modes, all diagnostics on
//! stderr". That is enforced here by there being exactly one type that can
//! write to stdout at all — [`Stdout`] — and by the diagnostic path
//! ([`note`]) not being able to reach it.

use std::collections::{HashMap, VecDeque};
use std::io::{ErrorKind, Write};
use std::sync::Mutex;
use std::time::Instant;

use ego_core::cost::{CumulativeCost, RateSource};
use ego_core::event::domain::{CancelCause, OccupancyProvenance};
use ego_core::event::id::{ItemId, SessionId};
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

/// How many finished tool calls the expand key can reach back to.
const EXPANDABLE_CALLS: usize = 16;

/// Interactive text and diagnostics serialized into complete logical rows.
pub struct InteractiveOutput {
    inner: Mutex<SurfaceOutput>,
    /// Detail of the most recent tool calls, oldest first. Its own lock: the
    /// rows are written through `inner` and the detail is only ever read back.
    tools: Mutex<VecDeque<String>>,
}

impl InteractiveOutput {
    pub fn new(output: SurfaceOutput) -> Self {
        Self {
            inner: Mutex::new(output),
            tools: Mutex::new(VecDeque::new()),
        }
    }
}

impl Diagnostics for InteractiveOutput {
    fn note(&self, message: &str) {
        self.line(&format!("{DISPLAY_NAME}: {message}"));
    }

    fn tool_row(&self, rows: &str, detail: String) {
        self.line(rows);
        let mut tools = self.tools.lock().expect("tool detail lock");
        if tools.len() == EXPANDABLE_CALLS {
            tools.pop_front();
        }
        tools.push_back(detail);
    }

    fn expand_last_tool(&self) {
        let detail = self.tools.lock().expect("tool detail lock").back().cloned();
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
        let _ = output.write(text);
        let _ = output.write("\n");
    }

    fn finish(&self) {
        let _ = self.inner.lock().expect("surface output lock").finish();
    }
}

/// A diagnostic. Always stderr, in every mode, with no exception — which is
/// why it does not take a [`Stdout`] and could not use one if it wanted to.
pub fn note(message: &str) {
    let mut err = std::io::stderr();
    write_note(&mut err, message);
}

fn write_note(mut err: impl Write, message: &str) {
    // Nothing to be done if the diagnostic channel itself is gone, and a
    // panic here would replace a report with a crash.
    let _ = writeln!(err, "{DISPLAY_NAME}: {message}");
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
}

struct StartedCall {
    name: String,
    args: String,
    at: Instant,
}

/// The state of the answer currently being written.
struct Streaming {
    answer: Answer,
    open: bool,
}

impl<'a> TextSink<'a> {
    pub fn new(out: &'a dyn TextOutput, quiet: bool, capabilities: Capabilities) -> Self {
        TextSink {
            out,
            quiet,
            text: Mutex::new(Streaming {
                answer: Answer::new(capabilities),
                open: false,
            }),
            ceiling: Mutex::new(None),
            calls: Mutex::new(HashMap::new()),
            capabilities,
        }
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
        let rest = text.answer.finish();
        if !rest.is_empty() {
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
        let ready = text.answer.push(delta);
        if ready.is_empty() {
            return;
        }
        self.out.answer(&ready);
        text.open = !ready.ends_with('\n');
    }
}

impl EventSink for TextSink<'_> {
    fn publish(&self, event: &PublicEvent, _state: &SessionState) {
        match event {
            PublicEvent::SessionStarted { mcp_errors, .. } => {
                for error in mcp_errors {
                    self.finish();
                    self.out.note(&format!("MCP: {error}"));
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
            PublicEvent::ItemStarted {
                kind: ItemKind::Tool,
                item,
                name: Some(name),
                text,
                ..
            } if !self.quiet => {
                self.calls
                    .lock()
                    .expect("a poisoned lock means a writer panicked mid-line")
                    .insert(
                        *item,
                        StartedCall {
                            name: name.clone(),
                            args: text.clone().unwrap_or_default(),
                            at: Instant::now(),
                        },
                    );
            }
            PublicEvent::ItemCompleted {
                kind: ItemKind::Tool,
                item,
                ok,
                text,
                ..
            } if !self.quiet => {
                let started = self
                    .calls
                    .lock()
                    .expect("a poisoned lock means a writer panicked mid-line")
                    .remove(item);
                if let Some(started) = started {
                    let record = tool_row::record(
                        &Finished {
                            name: &started.name,
                            args: &started.args,
                            ok: *ok,
                            result: text.as_deref().unwrap_or(""),
                            elapsed: started.at.elapsed(),
                        },
                        self.capabilities,
                    );
                    self.finish();
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
            PublicEvent::Error { message, .. } => {
                self.finish();
                self.out.note(message);
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
    /// `docs/decisions/2026-08-21-an-exit-code-that-names-who-refused.md` §4.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub denied_by: Option<Authority>,
    pub turns: usize,
    pub usage: ego_core::state::Usage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<ContextReport>,
    pub cost: CumulativeCost,
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
        write_note(&mut err, "something failed");

        assert_eq!(err, "εgo: something failed\n".as_bytes());
    }

    #[test]
    fn a_question_nobody_answered_names_the_action_and_no_authority() {
        let object = report(&RunTermination::PermissionRequired {
            action: "edit may not write /ws/x".to_string(),
            by: DeniedBy::Nobody,
        });
        let object = object.as_object().expect("§4.1 publishes an object");

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
        assert!(
            rows[0].0.starts_with("✓ read a.rs · 2 lines · "),
            "{:?}",
            rows[0].0
        );
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
            ["row one\n", "row two\n"]
        );

        output.expand_last_tool();
        assert_eq!(rows.try_iter().collect::<Vec<_>>(), ["detail two\n"]);
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
        assert_eq!(output.tools.lock().unwrap().len(), EXPANDABLE_CALLS);
        assert_eq!(output.tools.lock().unwrap().front().unwrap(), "detail 5");
    }
}
