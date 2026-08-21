//! Everything that reaches a terminal or a pipe: the three `--output` modes
//! of `CLI.md` §4, and the rule that stdout is protocol-clean.
//!
//! §4.5: "stdout is protocol-clean in json/jsonl/acp modes, all diagnostics on
//! stderr". That is enforced here by there being exactly one type that can
//! write to stdout at all — [`Stdout`] — and by the diagnostic path
//! ([`note`]) not being able to reach it.

use std::io::{ErrorKind, Write};
use std::sync::Mutex;

use ego_core::cost::CumulativeCost;
use ego_core::event::domain::{CancelCause, OccupancyProvenance};
use ego_core::event::id::SessionId;
use ego_core::public::{ItemKind, PUBLIC_EVENT_VERSION, PublicEvent};
use ego_core::run::RunTermination;
use ego_core::sink::EventSink;
use ego_core::state::SessionState;
use ego_core::turn::CancelToken;
use serde::Serialize;

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

    /// Write without a newline, for streamed text.
    fn write(&self, text: &str) {
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

/// A diagnostic. Always stderr, in every mode, with no exception — which is
/// why it does not take a [`Stdout`] and could not use one if it wanted to.
pub fn note(message: &str) {
    let mut err = std::io::stderr();
    // Nothing to be done if the diagnostic channel itself is gone, and a
    // panic here would replace a report with a crash.
    let _ = writeln!(err, "ego: {message}");
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
    fn publish(&self, event: &PublicEvent) {
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
    out: &'a Stdout,
    quiet: bool,
    /// Whether a partial line of assistant text is outstanding, so the run can
    /// finish the line before anything else is printed.
    open: Mutex<bool>,
}

impl<'a> TextSink<'a> {
    pub fn new(out: &'a Stdout, quiet: bool) -> Self {
        TextSink {
            out,
            quiet,
            open: Mutex::new(false),
        }
    }

    /// End the streamed line, if one is open.
    pub fn finish(&self) {
        let mut open = self
            .open
            .lock()
            .expect("a poisoned lock means a writer panicked mid-line");
        if *open {
            self.out.line("");
            *open = false;
        }
    }

    fn stream(&self, text: &str) {
        let mut open = self
            .open
            .lock()
            .expect("a poisoned lock means a writer panicked mid-line");
        self.out.write(text);
        *open = true;
    }
}

impl EventSink for TextSink<'_> {
    fn publish(&self, event: &PublicEvent) {
        match event {
            PublicEvent::ItemDelta {
                kind: ItemKind::Message,
                text,
                ..
            } => self.stream(text),
            PublicEvent::ItemStarted {
                kind: ItemKind::Tool,
                name: Some(name),
                text,
                ..
            } if !self.quiet => {
                self.finish();
                let args = text.as_deref().unwrap_or("");
                note(&format!("{name} {args}"));
            }
            PublicEvent::Error { message, .. } => {
                self.finish();
                note(message);
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
    /// The action a permission-refused run was denied. §4.4 requires it: a
    /// script handed exit 3 and no subject has been told nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub denied: Option<String>,
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
        let denied = match termination {
            RunTermination::PermissionRequired { action } => Some(action.clone()),
            _ => None,
        };
        Report {
            v: PUBLIC_EVENT_VERSION,
            session,
            result,
            ok: matches!(termination, RunTermination::Done),
            exit_reason: termination.exit_reason(),
            denied,
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
