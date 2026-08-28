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

fn main() -> std::process::ExitCode {
    // Behavioural network probes are hidden child modes, not CLI commands.
    // Dispatch them before clap or any provider/configuration work.
    #[cfg(target_os = "macos")]
    if let Some(exit) = ego_tools::exec::dispatch_network_probe(std::env::args_os()) {
        return exit;
    }

    // Landlock restricts the calling thread. Dispatch before constructing the
    // multithreaded Tokio runtime so the private helper has exactly one thread
    // to confine before it replaces itself with the authorised process.
    #[cfg(target_os = "linux")]
    if let Some(exit) = ego_tools::exec::dispatch_landlock_helper(std::env::args_os()) {
        return exit;
    }

    ordinary_main()
}

#[tokio::main]
async fn ordinary_main() -> std::process::ExitCode {
    let cancel = CancelToken::new();
    let out = Stdout::new(cancel.clone());

    let ended = match cli::parse(std::env::args().collect()) {
        Ok(Invocation::Auth(args)) => {
            let mut running = Box::pin(ego_cli::auth::execute(&args, &cancel));
            let termination = tokio::select! {
                biased;
                cause = run::stop_signal(None) => {
                    cancel.cancel(cause);
                    let _ = running.as_mut().await;
                    run::cancelled_termination(&cancel)
                }
                result = &mut running => {
                    if cancel.is_cancelled() {
                        run::cancelled_termination(&cancel)
                    } else {
                        match result {
                            Ok(text) => {
                                out.write(&text);
                                RunTermination::Done
                            }
                            Err(err) => RunTermination::UsageError {
                                message: err.to_string(),
                            },
                        }
                    }
                }
            };
            Ended {
                termination,
                session: ego_core::event::id::SessionId::new(),
                state: ego_core::state::SessionState::default(),
                result: serde_json::Value::String(String::new()),
            }
        }
        // Configuration management has no session and observes no admitted
        // protocol snapshot. It reports only the durable user-config outcome.
        Ok(Invocation::Mcp(args)) => Ended {
            termination: ego_cli::mcp::execute(&args, &out),
            session: ego_core::event::id::SessionId::new(),
            state: ego_core::state::SessionState::default(),
            result: serde_json::Value::String(String::new()),
        },
        // `ls` starts no session and takes no turn, so it has no `Ended` to
        // report — only a termination. The two absent fields are stated as
        // what they are rather than filled with a session id nothing wrote.
        Ok(Invocation::Ls(args)) => Ended {
            termination: run::ls(&args, &out),
            session: ego_core::event::id::SessionId::new(),
            state: ego_core::state::SessionState::default(),
            result: serde_json::Value::String(String::new()),
        },
        Ok(Invocation::Models(args)) => {
            let termination = match ego_cli::models::execute(&args, &cancel).await {
                Ok(rendered) => {
                    out.line(&rendered);
                    RunTermination::Done
                }
                Err(termination) => termination,
            };
            Ended {
                termination,
                session: ego_core::event::id::SessionId::new(),
                state: ego_core::state::SessionState::default(),
                result: serde_json::Value::String(String::new()),
            }
        }
        // `acp` serves many sessions over one connection, so there is no single
        // one to report — the same shape of absence `ls` has, for the opposite
        // reason. Its termination says whether the connection ended cleanly.
        Ok(Invocation::Acp(args)) => Ended {
            termination: ego_cli::acp::serve(&args).await,
            session: ego_core::event::id::SessionId::new(),
            state: ego_core::state::SessionState::default(),
            result: serde_json::Value::String(String::new()),
        },
        // `mcp-server` owns stdout as its JSON-RPC transport and serves many
        // rooted runs, so the ordinary process projection must stay silent.
        Ok(Invocation::McpServer(args)) => Ended {
            termination: ego_cli::mcp_server::serve(&args).await,
            session: ego_core::event::id::SessionId::new(),
            state: ego_core::state::SessionState::default(),
            result: serde_json::Value::String(String::new()),
        },
        // `--help` and `--version`. On **stdout** and exit 0, because the
        // question was answered: `CLI.md` §4.3's exit 2 is a usage error, and
        // asking what a command does is not misusing it. The text is written
        // whole rather than through `out.line`, which appends a newline clap's
        // rendering already has.
        Ok(Invocation::Answered(text)) => {
            out.write(&text);
            Ended {
                termination: RunTermination::Done,
                session: ego_core::event::id::SessionId::new(),
                state: ego_core::state::SessionState::default(),
                result: serde_json::Value::String(String::new()),
            }
        }
        Ok(Invocation::Interactive(args)) => run::interactive(*args, &out, cancel).await,
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
