//! The connection: ACP requests in, ego turns out.
//!
//! `ARCHITECTURE.md` §7 puts "ACP requests → core operations" in this crate,
//! and this is that half. What it deliberately does *not* contain is a run:
//! resolving a model, opening a store and driving a `TurnLoop` is `ego-cli`'s
//! `run` module, and a second copy here would be the parallel implementation
//! `AGENTS.md`'s reuse rule forbids. The seam between them is [`Turns`].
//!
//! ## The four methods a baseline agent must answer
//!
//! `v1/agent.rs`: "all Agents **MUST** support `session/new`, `session/prompt`,
//! `session/cancel`, and `session/update`". This module answers the first three
//! and [`AcpSink`](crate::sink::AcpSink) sends the fourth. `initialize` is
//! answered here too — see [`negotiate`](crate::negotiate).
//!
//! Everything else is refused by the SDK as an unhandled method, which is the
//! honest answer: [`capabilities`] advertises nothing this build does not do,
//! so a conforming client never asks.
//!
//! ## Why `session/prompt` is spawned and `session/cancel` is not
//!
//! A request callback holds the SDK's dispatch loop until it returns
//! (`concepts::ordering`). A turn takes as long as a model takes, and the
//! `session/cancel` that would stop it arrives on that same loop — so answering
//! a prompt inline would make cancellation unreachable for exactly as long as
//! there was something to cancel. The prompt handler therefore hands the work
//! to [`ConnectionTo::spawn`] and responds from there.
//!
//! `session/cancel` stays on the loop because all it does is flip an atomic:
//! spawning it would add a scheduling hop to the one message whose whole value
//! is arriving promptly.
//!
//! ## One turn per session
//!
//! A `session/prompt` for a session whose turn has not answered is refused
//! rather than queued —
//! `docs/decisions/2026-08-21-one-turn-per-session-and-what-cancel-means.md`.
//! The registry holds one [`CancelToken`] per session because the protocol's
//! cancel names a session and not a turn, so a session with two live turns has
//! no unambiguous answer to give a `session/cancel`. Admitting one turn at a
//! time is what makes the one token correct.
//!
//! This is per session and not per connection: two *different* sessions prompt
//! concurrently, and nothing here serialises them. (`ego-cli`'s `Host` does,
//! for its own reason — `run_with` roots a run with `set_current_dir`, which is
//! process-global — and that is a separate limit recorded in `ego-cli/src/acp.rs`.)

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agent_client_protocol::{
    Agent, AgentProtocolRouter, Client, ConnectionTo, Error, on_receive_notification,
    on_receive_request,
};
use agent_client_protocol_schema::v1;
use ego_core::event::domain::CancelCause;
use ego_core::event::id::SessionId;
use ego_core::question::Elicitor;
use ego_core::turn::CancelToken;

use crate::negotiate::{SERVED, negotiate};
use crate::question::{AcpQuestionAdapter, CapabilityCapture};

/// One `session/prompt`, everything a host needs to run it.
pub struct Turn {
    /// The session this turn continues. Minted by `session/new`, and an ego
    /// [`SessionId`] rather than an opaque token so the very same session is
    /// on disk under `ego ls` and resumable with `ego resume`.
    pub session: SessionId,
    /// The workspace `session/new` named.
    pub cwd: PathBuf,
    /// The prompt, flattened to text.
    pub text: String,
    /// Where this turn's events go, as they happen.
    pub updates: Updates,
    /// Set by `session/cancel` for this session, and by nothing else.
    pub cancel: CancelToken,
    /// The connection-backed human-input boundary for this turn.
    pub elicitor: Arc<dyn Elicitor>,
}

/// The client end of one session, for building the sink a turn publishes into.
///
/// A separate type rather than a bare `ConnectionTo`, because a sink needs the
/// session id too and a host that had to pair them itself could pair them
/// wrongly — sending one session's updates under another's name is a mistake
/// the type system can simply not permit.
#[derive(Clone)]
pub struct Updates {
    connection: ConnectionTo<Client>,
    session: v1::SessionId,
}

impl Updates {
    /// A sink that turns this run's events into `session/update` notifications.
    ///
    /// `caps` is the resolved model's, and is not known until the host has
    /// resolved it — which is why this is a method on the connection rather
    /// than a sink handed over ready-made.
    pub fn sink(
        &self,
        caps: ego_core::generator::capabilities::ModelCapabilities,
    ) -> crate::sink::AcpSink {
        crate::sink::AcpSink::new(self.connection.clone(), self.session.clone(), caps)
    }
}

/// What a host does with one `session/prompt`.
///
/// `Ok(stop_reason)` becomes the response; `Err(message)` becomes the JSON-RPC
/// error on it. [`stop_reason_v1`](crate::project::stop_reason_v1) is the
/// mapping from ego's own vocabulary and says why some endings are the second
/// rather than the first.
pub trait Turns: Clone + Send + Sync + 'static {
    fn run(&self, turn: Turn) -> impl Future<Output = Result<v1::StopReason, String>> + Send;
}

/// What this build tells a client it can do.
///
/// Everything is off. That is not a placeholder — each flag names a subsystem
/// `CLI.md` §2 already records as unbuilt, and advertising one would invite a
/// request this agent would then have to fail:
///
/// - `load_session` needs `session/load`, which replays a stored transcript to
///   the client. The log exists; the replay does not.
/// - `image`, `audio`, `embedded_context` need a non-text content block, which
///   is what `--attach` waits on: ego's conversation model carries text, tool
///   calls and tool results, so an image has no representation to be sent in.
/// - `mcp_capabilities` needs the MCP client (**M2**).
/// - `session_capabilities`' `list` and `delete` are `ego ls` and a deletion
///   this build does not have.
fn capabilities() -> v1::AgentCapabilities {
    v1::AgentCapabilities::new()
}

/// What `session/new` recorded, and `session/prompt` and `session/cancel` read.
struct Registered {
    session: SessionId,
    cwd: PathBuf,
    /// The running turn's token, and `None` between turns.
    ///
    /// A fresh one per turn, because a [`CancelToken`] is one-shot: a session
    /// whose first turn was cancelled must still be able to run a second. An
    /// `Option` rather than a token that is always present, because the two
    /// states this field has to distinguish are *a turn is running* and *none
    /// is* — and a token standing in for both is what let
    /// `docs/decisions/2026-08-21-one-turn-per-session-and-what-cancel-means.md`
    /// §1's defect exist: a second prompt replaced the running turn's token and
    /// left the turn that was actually generating unreachable by
    /// `session/cancel`.
    ///
    /// `None` also gives `session/cancel` between turns the right answer for
    /// the right reason: there is nothing to stop, so it stops nothing.
    running: Option<CancelToken>,
}

/// What `session/prompt` found when it asked to start a turn.
///
/// Three answers and not an `Option`, because the two ways to be refused are
/// different sentences to a host: one is *you never opened this*, which is a
/// mistake, and the other is *you already have one running*, which is a race a
/// correct client can lose.
enum Admitted {
    /// The turn may run: the session it continues, its workspace, and the
    /// token `session/cancel` will flip.
    Turn(SessionId, PathBuf, CancelToken),
    /// This session's previous turn has not answered yet.
    Busy,
    /// No `session/new` opened this id on this connection.
    Unknown,
}

/// The sessions this connection has open.
///
/// A `std::sync::Mutex` and not an async one: every critical section is a map
/// lookup, and nothing awaits while it is held.
#[derive(Clone, Default)]
struct Sessions(Arc<Mutex<HashMap<v1::SessionId, Registered>>>);

impl Sessions {
    fn open(&self, id: v1::SessionId, session: SessionId, cwd: PathBuf) {
        self.lock().insert(
            id,
            Registered {
                session,
                cwd,
                running: None,
            },
        );
    }

    /// Admit a turn, if this session has none running: hand back what it needs
    /// and arm the token `session/cancel` will find.
    ///
    /// The check and the arming are one critical section on purpose. Two
    /// prompts arriving together must not both read "nothing is running" before
    /// either writes.
    fn begin(&self, id: &v1::SessionId) -> Admitted {
        let mut open = self.lock();
        let Some(registered) = open.get_mut(id) else {
            return Admitted::Unknown;
        };
        if registered.running.is_some() {
            return Admitted::Busy;
        }
        let cancel = CancelToken::new();
        registered.running = Some(cancel.clone());
        Admitted::Turn(registered.session, registered.cwd.clone(), cancel)
    }

    /// The turn is over: this session may take another.
    ///
    /// Called on every path out of an admitted turn, including the ones that
    /// end badly — a turn that fails releases its session, or a single refused
    /// prompt would wedge the session for the life of the connection.
    fn end(&self, id: &v1::SessionId) {
        if let Some(registered) = self.lock().get_mut(id) {
            registered.running = None;
        }
    }

    fn cancel(&self, id: &v1::SessionId) {
        if let Some(registered) = self.lock().get(id)
            && let Some(running) = &registered.running
        {
            running.cancel(CancelCause::User);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<v1::SessionId, Registered>> {
        self.0
            .lock()
            .expect("a poisoned lock means a handler panicked mid-session")
    }
}

/// The text of a prompt, and a refusal naming the block kind that has none.
///
/// `ContentBlock::Text` and `ContentBlock::ResourceLink` are the two every
/// agent must accept, and this accepts both: a link becomes its URI, in the
/// prompt, where the model can hand it to a file tool. That is the whole of
/// what "supporting" a link means for an agent with no fetcher — the
/// alternative, dropping it, would silently answer a question about a file the
/// client believed it had attached.
///
/// The other three are refused **by name**, matching `CLI.md` §3.1's standard
/// for a documented-but-unavailable surface. A conforming client never sends
/// them, because [`capabilities`] says so; a client that sends one anyway gets
/// a sentence rather than a turn about the wrong thing.
fn prompt_text(blocks: &[v1::ContentBlock]) -> Result<String, String> {
    let mut parts = Vec::new();
    for block in blocks {
        match block {
            v1::ContentBlock::Text(text) => parts.push(text.text.clone()),
            v1::ContentBlock::ResourceLink(link) => parts.push(link.uri.clone()),
            v1::ContentBlock::Image(_) => {
                return Err(unsupported_block("an image"));
            }
            v1::ContentBlock::Audio(_) => {
                return Err(unsupported_block("audio"));
            }
            v1::ContentBlock::Resource(_) => {
                return Err(unsupported_block("an embedded resource"));
            }
            // `ContentBlock` is `#[non_exhaustive]`: a block kind added by a
            // later schema release must be refused rather than silently
            // dropped, for the same reason the three above are.
            _ => return Err(unsupported_block("a content block this build cannot read")),
        }
    }
    Ok(parts.join("\n"))
}

fn unsupported_block(what: &str) -> String {
    format!(
        "the prompt carries {what}, and this build's conversation model holds \
         only text, tool calls and tool results — so there is nothing for it to \
         be sent in. `initialize` advertises no such prompt capability"
    )
}

/// Build the agent a host connects to.
///
/// Returns the SDK's protocol router with **one arm configured**, which is
/// where the negotiation rule of
/// `docs/decisions/2026-08-21-the-version-an-agent-answers-with.md` §6 actually
/// lives: the router selects the highest configured implementation at or below
/// the version a client asks for, so configuring v1 alone is what makes a
/// client asking for `3` receive `1` rather than the v2 draft.
///
/// Returned rather than connected, so the transport is the caller's: `ego acp`
/// gives it `Stdio`, and a test gives it a pair of in-memory line channels and
/// asks what version came back.
pub fn agent<T: Turns>(turns: T) -> AgentProtocolRouter {
    Agent.protocol_router().with_v1(v1_agent(turns))
}

fn v1_agent<T: Turns>(turns: T) -> impl agent_client_protocol::ConnectTo<Client> {
    let sessions = Sessions::default();
    let client_capabilities = Arc::new(Mutex::new(CapabilityCapture::default()));
    Agent
        .builder()
        .name("ego")
        .on_receive_request(
            {
                let client_capabilities = Arc::clone(&client_capabilities);
                async move |request: v1::InitializeRequest, responder, _cx| {
                    // Not `request.protocol_version`. The router has already
                    // narrowed the input to the arm it selected, so echoing would
                    // happen to be right — and would stop being right the moment a
                    // second arm is configured. See `negotiate`'s module doc.
                    let Some(answer) = negotiate(request.protocol_version) else {
                        return responder.respond_with_error(Error::invalid_request().data(
                            format!(
                                "this agent speaks ACP protocol version {SERVED}, and \
                             {} is below it",
                                request.protocol_version
                            ),
                        ));
                    };
                    client_capabilities
                        .lock()
                        .expect("initialize capability lock poisoned")
                        .capture_initialize(&request.client_capabilities);
                    responder.respond(
                        v1::InitializeResponse::new(answer)
                            .agent_capabilities(capabilities())
                            .agent_info(Some(v1::Implementation::new(
                                "ego",
                                env!("CARGO_PKG_VERSION"),
                            ))),
                    )
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = sessions.clone();
                async move |request: v1::NewSessionRequest, responder, _cx| {
                    if !request.mcp_servers.is_empty() {
                        return responder.respond_with_error(Error::invalid_params().data(
                            "this build has no MCP client, so the servers named \
                             here would be accepted and never connected to",
                        ));
                    }
                    // Ego's own id, rendered, is the ACP session id: one
                    // identifier, so the session a host is talking to is the
                    // session `ego ls` prints and `ego resume` continues.
                    let session = SessionId::new();
                    let id = v1::SessionId::new(session.to_string());
                    sessions.open(id.clone(), session, request.cwd);
                    responder.respond(v1::NewSessionResponse::new(id))
                }
            },
            on_receive_request!(),
        )
        .on_receive_notification(
            {
                let sessions = sessions.clone();
                async move |notification: v1::CancelNotification, _cx| {
                    sessions.cancel(&notification.session_id);
                    Ok(())
                }
            },
            on_receive_notification!(),
        )
        .on_receive_request(
            {
                let sessions = sessions.clone();
                let client_capabilities = Arc::clone(&client_capabilities);
                async move |request: v1::PromptRequest, responder, cx: ConnectionTo<Client>| {
                    let id = request.session_id.clone();
                    let (session, cwd, cancel) = match sessions.begin(&id) {
                        Admitted::Turn(session, cwd, cancel) => (session, cwd, cancel),
                        // `invalid_request` and not `invalid_params`: the
                        // params are fine and the same prompt would be accepted
                        // a moment later. What is wrong is the moment.
                        Admitted::Busy => {
                            return responder.respond_with_error(Error::invalid_request().data(
                                format!(
                                    "session {id} already has a turn running, and this \
                                     agent runs one turn per session: wait for the \
                                     prompt in flight to answer before sending the next"
                                ),
                            ));
                        }
                        Admitted::Unknown => {
                            return responder.respond_with_error(Error::invalid_params().data(
                                format!(
                                    "there is no open session {id}: create one with \
                                     `session/new` first"
                                ),
                            ));
                        }
                    };
                    let text = match prompt_text(&request.prompt) {
                        Ok(text) => text,
                        Err(refusal) => {
                            // Admitted a moment ago, so it has to be released:
                            // a prompt this build cannot read must cost the
                            // client its answer, not its session.
                            sessions.end(&id);
                            return responder
                                .respond_with_error(Error::invalid_params().data(refusal));
                        }
                    };

                    let turns = turns.clone();
                    let sessions = sessions.clone();
                    let updates = Updates {
                        connection: cx.clone(),
                        session: id.clone(),
                    };
                    let capabilities = *client_capabilities
                        .lock()
                        .expect("initialize capability lock poisoned");
                    let elicitor = AcpQuestionAdapter::live(capabilities, cx.clone()).elicitor();
                    // Off the dispatch loop — see the module doc. The responder
                    // travels with the work, so the response is sent when the
                    // turn actually ends rather than when it was accepted.
                    cx.spawn(async move {
                        let outcome = turns
                            .run(Turn {
                                session,
                                cwd,
                                text,
                                updates,
                                cancel,
                                elicitor,
                            })
                            .await;
                        // Before the response, and on both arms: the client is
                        // free to prompt again the instant it has its answer,
                        // and a session still marked busy would refuse it.
                        sessions.end(&id);
                        match outcome {
                            Ok(stop_reason) => {
                                responder.respond(v1::PromptResponse::new(stop_reason))
                            }
                            Err(message) => {
                                responder.respond_with_error(Error::internal_error().data(message))
                            }
                        }
                    })?;
                    Ok(())
                }
            },
            on_receive_request!(),
        )
}
