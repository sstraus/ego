#![cfg(windows)]

//! Process contracts for Windows unconfined-start admission.
//!
//! These tests deliberately drive the real `ego` binary.  The fake provider
//! remains loopback-only, so a passing admission test proves that the chosen
//! branch reached execution without making a paid or external request.

mod support;

use std::fs;

use ego_cli::trust::inventory_digest;
use support::{Ego, FIXTURE_SESSION_ID, Finished, MODEL, Places, Stub};

/// Stable words in the human acknowledgement surface.  The complete prompt
/// may be rendered with terminal control sequences, but it must explain both
/// the platform and the missing guarantee before accepting `yes` or `no`.
const UNCONFINED_PROMPT: &str = "Windows execution is unconfined";

fn direct(places: &Places, stub: &Stub) -> Ego {
    places.ego(stub).args(["--model", MODEL])
}

fn human<const N: usize>(places: &Places, stub: &Stub, args: [&str; N]) -> Ego {
    places
        .ego(stub)
        .conversing()
        .args(["--model", "ollama/qwen3"])
        .args(args)
}

fn output(finished: &Finished) -> String {
    format!("{}\n{}", finished.stdout, finished.stderr)
}

fn assert_unconfined_prompt(finished: &Finished) {
    let text = output(finished);
    assert!(
        text.contains("Windows") && text.contains("unconfined"),
        "the human must be told the Windows limitation before answering:\n{text}"
    );
}

fn assert_no_provider_effects(stub: &Stub) {
    assert!(
        stub.requests().is_empty(),
        "Windows admission must settle before provider effects: {:?}",
        stub.requests()
    );
}

#[test]
fn direct_once_requires_literal_off_even_when_configured_off() {
    let places = Places::new();
    let stub = Stub::start(Default::default());
    let config = "model = \"ollama/qwen3\"\nsandbox = \"off\"\n";
    let config_path = places.ego(&stub).home_file("config.toml", config);

    let finished = direct(&places, &stub)
        .args(["--session-id", FIXTURE_SESSION_ID])
        .run();

    assert_eq!(finished.code, Some(2), "{}", output(&finished));
    assert!(
        output(&finished).contains("--sandbox off"),
        "the usage error must name the literal CLI remedy:\n{}",
        output(&finished)
    );
    assert_no_provider_effects(&stub);
    assert_eq!(
        fs::read_to_string(config_path).expect("config remains readable"),
        config
    );
}

#[test]
fn piped_no_human_requires_literal_off_even_when_configured_off() {
    let places = Places::new();
    let stub = Stub::start(Default::default());
    places.ego(&stub).home_file(
        "config.toml",
        "model = \"ollama/qwen3\"\nsandbox = \"off\"\n",
    );

    let finished = places
        .ego(&stub)
        .conversing()
        .args(["--model", MODEL, "--session-id", FIXTURE_SESSION_ID])
        .stdin("hello")
        .run();

    assert_eq!(finished.code, Some(2), "{}", output(&finished));
    assert!(output(&finished).contains("--sandbox off"));
    assert_no_provider_effects(&stub);
}

#[test]
fn headless_and_piped_offline_starts_require_literal_off_even_when_configured_off() {
    for piped in [false, true] {
        let places = Places::new();
        let stub = Stub::start(Default::default());
        places.ego(&stub).home_file(
            "config.toml",
            "model = \"ollama/qwen3\"\nsandbox = \"off\"\n",
        );

        let finished = if piped {
            places
                .ego(&stub)
                .conversing()
                .args([
                    "--model",
                    MODEL,
                    "--offline",
                    "--session-id",
                    FIXTURE_SESSION_ID,
                ])
                .stdin("hello")
                .run()
        } else {
            places
                .ego(&stub)
                .args([
                    "--model",
                    MODEL,
                    "--offline",
                    "--session-id",
                    FIXTURE_SESSION_ID,
                ])
                .run()
        };

        assert_eq!(finished.code, Some(2), "{}", output(&finished));
        assert!(output(&finished).contains("--sandbox off"));
        assert_no_provider_effects(&stub);
    }
}

#[test]
fn human_explicit_off_proceeds_without_an_acknowledgement_prompt() {
    let places = Places::new();
    let stub = Stub::start(Default::default());

    let finished = human(
        &places,
        &stub,
        ["--sandbox", "off", "--session-id", FIXTURE_SESSION_ID],
    )
    .arg("hello")
    .run_pty_seeded();

    assert_eq!(finished.code, Some(0), "{}", output(&finished));
    assert!(!output(&finished).contains(UNCONFINED_PROMPT));
    assert!(
        stub.generations().len() >= 1,
        "the explicit choice reached the provider"
    );
}

#[test]
fn human_configured_off_proceeds_without_rewriting_configuration() {
    let places = Places::new();
    let stub = Stub::start(Default::default());
    let config = "model = \"ollama/qwen3\"\nsandbox = \"off\"\n";
    let config_path = places.ego(&stub).home_file("config.toml", config);

    let finished = human(&places, &stub, ["--session-id", FIXTURE_SESSION_ID])
        .arg("hello")
        .run_pty_seeded();

    assert_eq!(finished.code, Some(0), "{}", output(&finished));
    assert!(!output(&finished).contains(UNCONFINED_PROMPT));
    assert_eq!(
        fs::read_to_string(config_path).expect("config remains readable"),
        config
    );
    assert!(!places.home_path().join("unconfined.json").exists());
    assert!(
        stub.generations().len() >= 1,
        "configured off reached the provider"
    );
}

#[test]
fn human_explicit_workspace_preserves_the_truthful_capability_refusal() {
    let places = Places::new();
    let stub = Stub::start(Default::default());

    let finished = human(&places, &stub, ["--sandbox", "workspace"]).run_pty_to_exit();
    let text = output(&finished);

    assert_eq!(finished.code, Some(2), "{text}");
    assert!(text.contains("--sandbox workspace"), "{text}");
    assert!(text.contains("no confinement backend"), "{text}");
    assert!(
        text.contains("--sandbox ro") && text.contains("--sandbox off"),
        "{text}"
    );
    assert!(
        !text.contains(UNCONFINED_PROMPT),
        "a constrained choice is not the acknowledgement path"
    );
    assert_no_provider_effects(&stub);
}

#[test]
fn human_explicit_ro_preserves_the_truthful_capability_refusal() {
    let places = Places::new();
    let stub = Stub::start(Default::default());

    let finished = human(&places, &stub, ["--sandbox", "ro"]).run_pty_to_exit();
    let text = output(&finished);

    assert_eq!(finished.code, Some(2), "{text}");
    assert!(text.contains("--sandbox ro"), "{text}");
    assert!(text.contains("no confinement backend"), "{text}");
    assert!(text.contains("--sandbox off"), "{text}");
    assert!(!text.contains(UNCONFINED_PROMPT), "{text}");
    assert_no_provider_effects(&stub);
}

#[test]
fn offline_refusal_is_preserved_even_when_unconfined_is_explicit() {
    let places = Places::new();
    let stub = Stub::start(Default::default());

    let finished = human(&places, &stub, ["--sandbox", "off", "--offline"]).run_pty_to_exit();
    let text = output(&finished);

    assert_eq!(finished.code, Some(2), "{text}");
    assert!(text.contains("--offline"), "{text}");
    assert!(text.contains("no confinement backend"), "{text}");
    assert_no_provider_effects(&stub);
}

#[test]
fn human_implicit_offline_refuses_without_an_acknowledgement_prompt() {
    let places = Places::new();
    let stub = Stub::start(Default::default());

    let finished = human(&places, &stub, ["--offline"]).run_pty_to_exit();
    let text = output(&finished);

    assert_eq!(finished.code, Some(2), "{text}");
    assert!(text.contains("--offline"), "{text}");
    assert!(text.contains("no confinement backend"), "{text}");
    assert!(!text.contains(UNCONFINED_PROMPT), "{text}");
    assert_no_provider_effects(&stub);
    assert!(!places.home_path().join("unconfined.json").exists());
}

#[test]
fn human_configured_workspace_preserves_the_truthful_capability_refusal() {
    let places = Places::new();
    let stub = Stub::start(Default::default());
    places.ego(&stub).home_file(
        "config.toml",
        "model = \"ollama/qwen3\"\nsandbox = \"workspace\"\n",
    );

    let finished = human(&places, &stub, []).run_pty_to_exit();
    let text = output(&finished);

    assert_eq!(finished.code, Some(2), "{text}");
    assert!(text.contains("--sandbox workspace"), "{text}");
    assert!(text.contains("--sandbox off"), "{text}");
    assert!(!text.contains(UNCONFINED_PROMPT), "{text}");
    assert_no_provider_effects(&stub);
}

#[test]
fn implicit_default_prompts_once_per_canonical_workspace_and_records_yes() {
    let places = Places::new();
    let stub = Stub::start(Default::default());
    let config = "model = \"ollama/qwen3\"\n";
    let config_path = places.ego(&stub).home_file("config.toml", config);

    let first = human(&places, &stub, ["--session-id", FIXTURE_SESSION_ID])
        .arg("hello")
        .run_pty_responding_seeded(UNCONFINED_PROMPT, "yes");
    assert_eq!(first.code, Some(0), "{}", output(&first));
    assert_unconfined_prompt(&first);
    assert!(
        stub.generations().len() >= 1,
        "yes must change this invocation to off"
    );

    let second = human(
        &places,
        &stub,
        [
            "-C",
            ".",
            "--session-id",
            "0198c7f4-9c1f-7a2b-8c3d-4e5f60718294",
        ],
    )
    .arg("hello")
    .run_pty_seeded();
    assert_eq!(second.code, Some(0), "{}", output(&second));
    assert!(!output(&second).contains(UNCONFINED_PROMPT));
    assert_eq!(
        fs::read_to_string(config_path).expect("config remains readable"),
        config
    );
    assert!(!places.home_path().join("trust.json").exists());
    assert!(places.home_path().join("unconfined.json").exists());
}

#[test]
fn implicit_default_no_refuses_before_provider_or_persistence_effects() {
    let places = Places::new();
    let stub = Stub::start(Default::default());

    let finished = human(&places, &stub, []).run_pty_responding_to_exit(UNCONFINED_PROMPT, "no");
    let text = output(&finished);

    assert_eq!(finished.code, Some(2), "{text}");
    assert_unconfined_prompt(&finished);
    assert_no_provider_effects(&stub);
    assert!(!places.home_path().join("unconfined.json").exists());
    assert!(!places.home_path().join("trust.json").exists());
}

#[test]
fn corrupt_acknowledgement_warns_asks_again_and_is_not_overwritten() {
    let places = Places::new();
    let stub = Stub::start(Default::default());
    let corrupt = places
        .ego(&stub)
        .home_file("unconfined.json", "{ not json\n");
    let before = fs::read(&corrupt).expect("corrupt ledger is readable");

    let finished = human(&places, &stub, [])
        .arg("hello")
        .run_pty_responding_seeded(UNCONFINED_PROMPT, "yes");
    let text = output(&finished);

    assert_eq!(finished.code, Some(0), "{text}");
    assert_unconfined_prompt(&finished);
    assert!(
        text.contains("could not be read") || text.contains("could not be parsed"),
        "{text}"
    );
    assert_eq!(fs::read(&corrupt).expect("corrupt ledger remains"), before);
    assert!(
        stub.generations().len() >= 1,
        "accepted answer proceeds for this invocation"
    );
}

#[test]
fn accepted_answer_with_write_failure_warns_and_proceeds_once() {
    let places = Places::new();
    let stub = Stub::start(Default::default());
    let ledger = places
        .ego(&stub)
        .home_file("unconfined.json", "{\"v\":1,\"workspaces\":{}}\n");
    let lock = places.home_path().join("unconfined.lock");
    fs::create_dir(&lock).expect("the lock path can be made unwritable");
    let before = fs::read(&ledger).expect("ledger is readable");

    let first = human(&places, &stub, [])
        .arg("hello")
        .run_pty_responding_seeded(UNCONFINED_PROMPT, "yes");
    let first_text = output(&first);
    assert_eq!(first.code, Some(0), "{first_text}");
    assert!(
        first_text.contains("could not") && first_text.contains("again"),
        "{first_text}"
    );
    assert_eq!(fs::read(&ledger).expect("ledger remains readable"), before);
    assert!(
        stub.generations().len() >= 1,
        "write failure is not a refusal for this invocation"
    );

    let second = human(&places, &stub, []).run_pty_responding_to_exit(UNCONFINED_PROMPT, "no");
    assert_eq!(second.code, Some(2), "{}", output(&second));
    assert_unconfined_prompt(&second);
}

#[test]
fn acknowledgement_does_not_mutate_user_configuration_or_trust() {
    let places = Places::new();
    let stub = Stub::start(Default::default());
    let config = "model = \"ollama/qwen3\"\n";
    let config_path = places.ego(&stub).home_file("config.toml", config);
    places.file("AGENTS.md", "# already trusted by this fixture\n");
    let workspace = fs::canonicalize(places.workspace_path()).expect("canonical workspace");
    let digest = inventory_digest(&workspace).expect("instructions are inventoried");
    let mut workspaces = serde_json::Map::new();
    workspaces.insert(
        workspace.display().to_string(),
        serde_json::json!({"decision": "trusted", "digest": digest}),
    );
    let trust = format!(
        "{}\n",
        serde_json::json!({"v": 1, "workspaces": workspaces})
    );
    let trust_path = places.ego(&stub).home_file("trust.json", &trust);

    let finished = human(&places, &stub, [])
        .arg("hello")
        .run_pty_responding_seeded(UNCONFINED_PROMPT, "yes");

    assert_eq!(finished.code, Some(0), "{}", output(&finished));
    assert_unconfined_prompt(&finished);
    assert_eq!(
        fs::read_to_string(config_path).expect("config remains readable"),
        config
    );
    assert_eq!(
        fs::read_to_string(trust_path).expect("trust remains readable"),
        trust
    );
    assert!(places.home_path().join("unconfined.json").exists());
}
