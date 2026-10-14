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
use ego_core::evidence::EvidenceAdmissionService;
use ego_core::run::RunTermination;
use ego_core::turn::CancelToken;
use std::io::IsTerminal;

fn main() -> std::process::ExitCode {
    // Behavioural network probes are hidden child modes, not CLI commands.
    // Dispatch them before clap or any provider/configuration work.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if let Some(delivered) = ego_tools::exec::dispatch_network_probe(std::env::args_os()) {
        return if delivered {
            std::process::ExitCode::SUCCESS
        } else {
            std::process::ExitCode::from(125)
        };
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
    let evidence = std::sync::Arc::new(EvidenceAdmissionService::new());

    let mut invocation = cli::parse(std::env::args().collect());
    if let Ok(Invocation::Run(args) | Invocation::Resume(args) | Invocation::Interactive(args)) =
        &mut invocation
    {
        ego_cli::config::capture_profile(args);
    }
    invocation = invocation.map(|invocation| {
        invocation.with_terminal(
            std::io::stdin().is_terminal(),
            std::io::stdout().is_terminal(),
        )
    });
    if let Ok(Invocation::SessionMetadata(args)) = &mut invocation {
        ego_cli::config::capture_metadata_profile(args);
    }
    if matches!(
        &invocation,
        Ok(Invocation::Interactive(args)) if args.profile.as_deref() == Some("ci")
    ) {
        invocation = Err(cli::UsageError(
            "profile `ci` is available only through ego run or ego resume".to_owned(),
        ));
    }
    // Before any run work, so a malformed `EGO_LOG` is a usage error rather
    // than a run that quietly reports nothing — the one failure an operator
    // cannot detect by looking. Only when the command line itself parsed: a
    // usage error already in hand is the one worth printing, and replacing it
    // with a complaint about an environment variable would answer a question
    // nobody asked.
    //
    // `Interactive` is the sole invocation that paints its own prompt, and that
    // is what decides whether stderr is free. See `diagnostics::plan`.
    if invocation.is_ok()
        && let Err(message) = ego_cli::diagnostics::install_from_environment(matches!(
            &invocation,
            Ok(Invocation::Interactive(_))
        ))
    {
        invocation = Err(cli::UsageError(message));
    }
    let mut ci_preflight = match &invocation {
        Ok(Invocation::Run(args) | Invocation::Resume(args))
            if args.profile.as_deref() == Some("ci") =>
        {
            Some(run::preflight_ci(args, &cancel).await)
        }
        _ => None,
    };
    let needs_mcp_runtime = matches!(&invocation, Ok(Invocation::Mcp(_)));
    let mcp = if needs_mcp_runtime {
        Some(ego_cli::provider_http::McpProcessScope::online(cancel.clone()).await)
    } else {
        None
    };

    let ended = if let Some(Err(error)) = mcp.as_ref() {
        Ended {
            termination: RunTermination::Failed {
                message: error.clone(),
            },
            session: ego_core::event::id::SessionId::new(),
            state: ego_core::state::SessionState::default(),
            result: serde_json::Value::String(String::new()),
            selected_session: None,
            mcp_errors: Vec::new(),
        }
    } else {
        match invocation {
            Ok(Invocation::Orchestrator(args)) => Ended {
                termination: ego_cli::orchestrator::execute(&args, &out).await,
                session: ego_core::event::id::SessionId::new(),
                state: ego_core::state::SessionState::default(),
                result: serde_json::Value::String(String::new()),
                selected_session: None,
                mcp_errors: Vec::new(),
            },
            Ok(Invocation::Config(args)) => {
                let mut running = Box::pin(ego_cli::config::execute(&args, &out, &cancel));
                let termination = tokio::select! {
                    biased;
                    cause = run::stop_signal(None) => {
                        cancel.cancel(cause);
                        let _ = running.as_mut().await;
                        run::cancelled_termination(&cancel)
                    }
                    result = &mut running => result,
                };
                Ended {
                    termination,
                    session: ego_core::event::id::SessionId::new(),
                    state: ego_core::state::SessionState::default(),
                    result: serde_json::Value::String(String::new()),
                    selected_session: None,
                    mcp_errors: Vec::new(),
                }
            }
            Ok(Invocation::Rules(args)) => Ended {
                termination: ego_cli::rules::execute(&args, &out),
                session: ego_core::event::id::SessionId::new(),
                state: ego_core::state::SessionState::default(),
                result: serde_json::Value::String(String::new()),
                selected_session: None,
                mcp_errors: Vec::new(),
            },
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
                    selected_session: None,
                    mcp_errors: Vec::new(),
                }
            }
            // Configuration management has no session and owns its explicitly
            // online command authority. Runs construct their separate effect
            // authority only after resolving confinement.
            Ok(Invocation::Mcp(args)) => Ended {
                termination: ego_cli::mcp::execute(
                    &args,
                    &out,
                    mcp.as_ref()
                        .and_then(|runtime| runtime.as_ref().ok())
                        .expect("MCP invocation constructed its runtime")
                        .controller(),
                )
                .await,
                session: ego_core::event::id::SessionId::new(),
                state: ego_core::state::SessionState::default(),
                result: serde_json::Value::String(String::new()),
                selected_session: None,
                mcp_errors: Vec::new(),
            },
            // `ls` starts no session and takes no turn, so it has no `Ended` to
            // report — only a termination. The two absent fields are stated as
            // what they are rather than filled with a session id nothing wrote.
            Ok(Invocation::Ls(args)) => Ended {
                termination: run::ls(&args, &out),
                session: ego_core::event::id::SessionId::new(),
                state: ego_core::state::SessionState::default(),
                result: serde_json::Value::String(String::new()),
                selected_session: None,
                mcp_errors: Vec::new(),
            },
            Ok(Invocation::SessionMetadata(args)) => Ended {
                termination: ego_cli::metadata::execute(&args, &out),
                session: args.session,
                state: ego_core::state::SessionState::default(),
                result: serde_json::Value::String(String::new()),
                selected_session: None,
                mcp_errors: Vec::new(),
            },
            Ok(Invocation::Usage(args)) => Ended {
                termination: ego_cli::usage::execute(&args, &out).await,
                session: ego_core::event::id::SessionId::new(),
                state: ego_core::state::SessionState::default(),
                result: serde_json::Value::String(String::new()),
                selected_session: None,
                mcp_errors: Vec::new(),
            },
            Ok(Invocation::Fork(args)) => run::fork(&args, &out).await,
            Ok(Invocation::Compact(args)) => {
                run::compact(&args, &out, std::sync::Arc::clone(&evidence)).await
            }
            Ok(Invocation::Export(args)) => Ended {
                termination: run::export(&args, &out),
                session: args.source,
                state: ego_core::state::SessionState::default(),
                result: serde_json::Value::String(String::new()),
                selected_session: None,
                mcp_errors: Vec::new(),
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
                    selected_session: None,
                    mcp_errors: Vec::new(),
                }
            }
            Ok(Invocation::Setup(args)) => Ended {
                termination: ego_cli::setup::execute(&args, &out),
                session: ego_core::event::id::SessionId::new(),
                state: ego_core::state::SessionState::default(),
                result: serde_json::Value::String(String::new()),
                selected_session: None,
                mcp_errors: Vec::new(),
            },
            Ok(Invocation::Doctor(args)) => Ended {
                termination: ego_cli::doctor::execute(&args, &out),
                session: ego_core::event::id::SessionId::new(),
                state: ego_core::state::SessionState::default(),
                result: serde_json::Value::String(String::new()),
                selected_session: None,
                mcp_errors: Vec::new(),
            },
            Ok(Invocation::Hooks(args)) => Ended {
                termination: ego_cli::hooks::execute(&args, &out),
                session: ego_core::event::id::SessionId::new(),
                state: ego_core::state::SessionState::default(),
                result: serde_json::Value::String(String::new()),
                selected_session: None,
                mcp_errors: Vec::new(),
            },
            // `acp` serves many sessions over one connection, so there is no single
            // one to report — the same shape of absence `ls` has, for the opposite
            // reason. Its termination says whether the connection ended cleanly.
            Ok(Invocation::Acp(args)) => Ended {
                termination: ego_cli::acp::serve(&args, std::sync::Arc::clone(&evidence)).await,
                session: ego_core::event::id::SessionId::new(),
                state: ego_core::state::SessionState::default(),
                result: serde_json::Value::String(String::new()),
                selected_session: None,
                mcp_errors: Vec::new(),
            },
            // `mcp-server` owns stdout as its JSON-RPC transport and serves many
            // rooted runs, so the ordinary process projection must stay silent.
            // `a2a` binds a socket and serves many callers over it, so it has
            // no single session to report either. Its termination says whether
            // the listener refused to start or ended badly.
            Ok(Invocation::A2a(args)) => Ended {
                termination: ego_cli::a2a::serve(&args).await,
                session: ego_core::event::id::SessionId::new(),
                state: ego_core::state::SessionState::default(),
                result: serde_json::Value::String(String::new()),
                selected_session: None,
                mcp_errors: Vec::new(),
            },
            Ok(Invocation::McpServer(args)) => Ended {
                termination: ego_cli::mcp_server::serve(&args, std::sync::Arc::clone(&evidence))
                    .await,
                session: ego_core::event::id::SessionId::new(),
                state: ego_core::state::SessionState::default(),
                result: serde_json::Value::String(String::new()),
                selected_session: None,
                mcp_errors: Vec::new(),
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
                    selected_session: None,
                    mcp_errors: Vec::new(),
                }
            }
            Ok(Invocation::Interactive(args)) => {
                run::interactive_with_mcp_registry(
                    *args,
                    &out,
                    cancel,
                    std::sync::Arc::clone(&evidence),
                )
                .await
            }
            Ok(Invocation::Run(args) | Invocation::Resume(args)) => {
                let json = args.output == cli::Output::Json;
                let ended = match ci_preflight.take() {
                    Some(Err(termination)) => Ended {
                        termination,
                        session: ego_core::event::id::SessionId::new(),
                        state: ego_core::state::SessionState::default(),
                        result: serde_json::Value::String(String::new()),
                        selected_session: None,
                        mcp_errors: Vec::new(),
                    },
                    Some(Ok(preflight)) => {
                        run::run_with_ci_preflight(
                            *args,
                            &out,
                            cancel,
                            std::sync::Arc::clone(&evidence),
                            preflight,
                        )
                        .await
                    }
                    None => {
                        run::run_with_mcp_registry(
                            *args,
                            &out,
                            cancel,
                            std::sync::Arc::clone(&evidence),
                        )
                        .await
                    }
                };
                if json {
                    let report = run::report(&ended);
                    match serde_json::to_string(&report) {
                        Ok(line) => out.line(&line),
                        Err(err) => {
                            output::note(&format!("the result could not be rendered: {err}"))
                        }
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
                selected_session: None,
                mcp_errors: Vec::new(),
            },
        }
    };

    run::diagnose(&ended.termination, out.broken());
    std::process::ExitCode::from(ended.termination.exit_code())
}
