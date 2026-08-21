//! The process.
//!
//! Short on purpose. The only thing that happens here which cannot happen
//! anywhere else is the last line: a `u8` from
//! [`RunTermination::exit_code`](ego_core::run::RunTermination::exit_code)
//! becoming the status the shell sees. That mapping lives in `ego-core` and is
//! never re-derived here — `CLI.md` §4.3 is one table, and a second copy in a
//! binary would be a second chance to disagree with an ACP host about what
//! "budget" exits with.

use ego_cli::cli::{self, Invocation};
use ego_cli::output::{self, Stdout};
use ego_cli::run::{self, Ended};
use ego_core::run::RunTermination;
use ego_core::turn::CancelToken;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cancel = CancelToken::new();
    let out = Stdout::new(cancel.clone());

    let ended = match cli::parse(std::env::args().collect()) {
        // `ls` starts no session and takes no turn, so it has no `Ended` to
        // report — only a termination. The two absent fields are stated as
        // what they are rather than filled with a session id nothing wrote.
        Ok(Invocation::Ls(args)) => Ended {
            termination: run::ls(&args, &out),
            session: ego_core::event::id::SessionId::new(),
            state: ego_core::state::SessionState::default(),
            result: serde_json::Value::String(String::new()),
        },
        // `acp` serves many sessions over one connection, so there is no single
        // one to report — the same shape of absence `ls` has, for the opposite
        // reason. Its termination says whether the connection ended cleanly.
        Ok(Invocation::Acp(args)) => Ended {
            termination: ego_cli::acp::serve(&args).await,
            session: ego_core::event::id::SessionId::new(),
            state: ego_core::state::SessionState::default(),
            result: serde_json::Value::String(String::new()),
        },
        Ok(Invocation::Run(args) | Invocation::Resume(args)) => {
            let json = args.output == cli::Output::Json;
            let ended = run::run(*args, &out, cancel).await;
            if json {
                let report = run::report(&ended);
                match serde_json::to_string(&report) {
                    Ok(line) => out.line(&line),
                    Err(err) => output::note(&format!("the result could not be rendered: {err}")),
                }
            }
            ended
        }
        Err(usage) => Ended {
            termination: RunTermination::UsageError {
                message: usage.to_string(),
            },
            session: ego_core::event::id::SessionId::new(),
            state: ego_core::state::SessionState::default(),
            result: serde_json::Value::String(String::new()),
        },
    };

    run::diagnose(&ended.termination, out.broken());
    std::process::ExitCode::from(ended.termination.exit_code())
}
