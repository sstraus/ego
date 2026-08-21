//! `ego acp`: the same run, driven by a host instead of a shell.
//!
//! `ego-acp` owns everything protocol-shaped — the router, the four methods,
//! the projection. This owns the one thing it must not: how a turn actually
//! runs. That is [`run::run_with`], the same function `ego run` and
//! `ego resume` go through, with a different destination for its events.
//!
//! ## A prompt is a resumed run
//!
//! `session/new` mints an ego [`SessionId`] and `session/prompt` continues it.
//! That is exactly `ego resume <id> "<prompt>"` with the store already chosen,
//! so this module builds [`Args`] rather than a second turn driver:
//! [`SessionChoice::Id`] is documented as a caller-supplied id, created when it
//! is not there and attached when it is, which is `session/new` followed by
//! `session/prompt` written down.
//!
//! The consequence is worth stating plainly: **a session a host created over
//! ACP is an ordinary ego session.** It appears in `ego ls`, it is resumable
//! with `ego resume`, and its log is the same JSONL. There is no ACP store.
//!
//! ## One turn at a time, and why
//!
//! `run::run_with` calls `std::env::set_current_dir` — a run is rooted at a
//! workspace, and that is a property of the *process*. Two ACP sessions in two
//! directories prompting at once would each be rooted at the other's
//! workspace. So turns are serialised across the connection: a second
//! `session/prompt` waits for the first to finish.
//!
//! This is a real limit, not an implementation detail to be tidied away later —
//! a host that opens two sessions gets sequential turns. It is the honest
//! answer while "here" is process-global, and the alternative was to run them
//! concurrently and be silently wrong about which directory each one read.

use std::path::PathBuf;
use std::sync::Arc;

use agent_client_protocol::{ConnectTo, Stdio};
use agent_client_protocol_schema::v1;
use ego_acp::project::stop_reason_v1;
use ego_acp::serve::{Turn, Turns};
use ego_core::generator::types::ThinkingLevel;
use ego_core::run::RunTermination;

use crate::cli::{AcpArgs, Args, Output, SessionChoice, Stdin};
use crate::run;

/// Serve the protocol on stdio until the host closes it.
pub async fn serve(args: &AcpArgs) -> RunTermination {
    // `-C` is applied once, here, for the same reason `run` applies it before
    // anything reads a path: a relative `--session-dir` has to mean the same
    // directory for the whole process. Each session's own `cwd` still roots the
    // turns run inside it.
    if let Err(err) = run::workspace(args.directory.as_ref()) {
        return RunTermination::UsageError {
            message: err.to_string(),
        };
    }

    let host = Host {
        session_dir: args.session_dir.clone(),
        one_at_a_time: Arc::new(tokio::sync::Mutex::new(())),
    };

    match ego_acp::serve::agent(host).connect_to(Stdio::new()).await {
        // The host closed the connection. That is how an ACP agent ends: it
        // serves until the thing that launched it stops asking.
        Ok(()) => RunTermination::Done,
        Err(err) => RunTermination::Failed {
            message: format!("the ACP connection ended badly: {err}"),
        },
    }
}

/// The host end of [`Turns`]: everything a prompt needs that the protocol does
/// not carry.
#[derive(Clone)]
struct Host {
    /// `--session-dir`, applied to every session this connection opens, so a
    /// host launched against a particular store keeps writing to it.
    session_dir: Option<PathBuf>,
    one_at_a_time: Arc<tokio::sync::Mutex<()>>,
}

impl Turns for Host {
    async fn run(&self, turn: Turn) -> Result<v1::StopReason, String> {
        let _serialised = self.one_at_a_time.lock().await;

        let ended = run::run_with(
            self.args(&turn),
            |caps| turn.updates.sink(caps.clone()),
            turn.cancel.clone(),
        )
        .await;
        stop_reason_v1(&ended.termination)
    }
}

impl Host {
    /// The command line this prompt would have been, had a person typed it.
    ///
    /// Every field is either the protocol's answer or the documented default,
    /// and none is a guess. The flags that have no ACP counterpart are the
    /// flags a host has no way to express: it cannot name a model
    /// (`EGO_MODEL` or a config file does), a budget or a schema, so those stay
    /// at what a bare `ego resume` would resolve them to.
    fn args(&self, turn: &Turn) -> Args {
        Args {
            prompt: Some(turn.text.clone()),
            // The prompt came over the wire; this process's stdin is the
            // protocol stream and must never be read as content.
            stdin: Stdin::Ambient,
            model: None,
            offline: false,
            thinking: ThinkingLevel::Off,
            system: None,
            append_system: None,
            instructions: None,
            bare: false,
            // `Id` and not `Resume`: `session/new` minted this id before the
            // session existed, so the first prompt has to create it and every
            // later one has to attach to it — which is precisely what `Id`
            // means and precisely what `Resume` refuses to do.
            session: SessionChoice::Id(turn.session.to_string()),
            session_dir: self.session_dir.clone(),
            // Not read: `run_with` takes the destination as a parameter, and
            // this run's events go to `session/update`. `Json` is the nearest
            // honest spelling of "nothing is written to stdout as it happens",
            // which is the one thing that would break the protocol.
            output: Output::Json,
            schema: None,
            schema_retries: 0,
            budget: None,
            max_turns: None,
            timeout: None,
            tools: None,
            // The workspace `session/new` named, applied per turn.
            directory: Some(turn.cwd.clone()),
            set: Vec::new(),
            profile: None,
            quiet: false,
            verbose: false,
            no_color: false,
        }
    }
}
