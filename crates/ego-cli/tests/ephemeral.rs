//! contract for regression case: `--ephemeral` is no longer a session mode.

mod support;

use ego_cli::cli::{self, Invocation};
use support::{Ego, Plan, Stub};

fn argv<const N: usize>(args: [&str; N]) -> Vec<String> {
    std::iter::once("ego".to_owned())
        .chain(args.into_iter().map(str::to_owned))
        .collect()
}

fn argv_slice(args: &[&str]) -> Vec<String> {
    std::iter::once("ego".to_owned())
        .chain(args.iter().map(|arg| (*arg).to_owned()))
        .collect()
}

fn refusal<const N: usize>(args: [&str; N]) -> String {
    cli::parse(argv(args))
        .expect_err("--ephemeral must be refused as a usage error")
        .to_string()
}

fn assert_refusal_message(message: &str) {
    assert!(
        message.contains("'--ephemeral' is not supported; remove it to use a saved session"),
        "{message}"
    );
}

#[test]
fn every_session_surface_refuses_ephemeral_and_names_durable_alternatives() {
    for message in [
        refusal(["run", "--ephemeral", "Ask"]),
        refusal(["--ephemeral", "Ask"]),
        refusal(["resume", "--ephemeral"]),
    ] {
        assert_refusal_message(&message);
    }
}

#[test]
fn parser_refuses_ephemeral_before_constructing_any_session_invocation() {
    for args in [
        vec!["run", "--ephemeral", "Ask"],
        vec!["--ephemeral", "Ask"],
        vec!["resume", "--ephemeral"],
    ] {
        let parsed = cli::parse(argv_slice(&args));
        assert!(parsed.is_err(), "parsed invocation: {parsed:?}");
        assert!(!matches!(
            parsed,
            Ok(Invocation::Run(_) | Invocation::Resume(_))
        ));
    }
}

#[test]
fn process_refuses_ephemeral_before_session_root_or_provider_effects() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub);
    let home = ego
        .home_file("071-marker", "")
        .parent()
        .unwrap()
        .to_path_buf();
    let finished = ego
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--ephemeral",
            "Ask",
        ])
        .run();

    assert_eq!(finished.code, Some(2), "stdout: {}", finished.stdout);
    assert_refusal_message(&finished.stderr);
    assert!(stub.generations().is_empty(), "provider was contacted");
    assert!(!home.join("sessions").exists(), "session root was created");
}
