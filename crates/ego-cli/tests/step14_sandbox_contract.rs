//! RED contract for CLI sandbox parsing, precedence, and process enforcement.
//!
//! The provider transcript exercises the same production tool path as a real
//! run. A missing backend capability is accepted only as a fail-closed result;
//! it must never silently become sandbox off.

mod support;

use std::path::{Path, PathBuf};

use support::{Ego, Plan, Reply, Stub, text, tool_call};

use ego_cli::cli::{self, Invocation};
use ego_core::config::{ConfigLayer, Source, Sourced, Trust, resolve};
use ego_core::permit::Confinement;

fn argv<const N: usize>(args: [&str; N]) -> Vec<String> {
    std::iter::once("ego".to_string())
        .chain(args.iter().map(|arg| arg.to_string()))
        .collect()
}

fn run_args<const N: usize>(args: [&str; N]) -> ego_cli::cli::Args {
    match cli::parse(argv(args)).expect("the command line parses") {
        Invocation::Run(args) | Invocation::Resume(args) => *args,
        other => panic!("expected a run invocation, got {other:?}"),
    }
}

fn layer(source: Source, text: &str) -> Sourced<ConfigLayer> {
    ConfigLayer::parse(source, text).expect("sandbox config parses")
}

fn exec_plan() -> Plan {
    Plan {
        completions: vec![
            Reply::Sse(tool_call(
                "exec-contract",
                "exec",
                r#"{"argv":["/bin/echo","cli exact argv"],"cwd":null,"timeout_ms":1000}"#,
            )),
            Reply::Sse(text("the model saw the process result")),
        ],
        ..Plan::default()
    }
}

fn shell_write_plan(target: &Path) -> Plan {
    let params = serde_json::json!({
        "argv": [
            "/bin/sh",
            "-c",
            "printf confined-write > \"$1\"",
            "ego-sandbox-contract",
            target.to_string_lossy(),
        ],
        "cwd": null,
        "timeout_ms": 1000,
    });
    Plan {
        completions: vec![
            Reply::Sse(tool_call("sandbox-contract", "exec", &params.to_string())),
            Reply::Sse(text("the model saw the process result")),
        ],
        ..Plan::default()
    }
}

fn completed_tool_event(finished: &support::Finished) -> serde_json::Value {
    maybe_completed_tool_event(finished).expect("the run published one completed exec item")
}

fn maybe_completed_tool_event(finished: &support::Finished) -> Option<serde_json::Value> {
    finished
        .events()
        .into_iter()
        .find(|event| event["type"] == "item_completed" && event["kind"] == "tool")
}

fn shell_write_case(
    user_sandbox: Option<&str>,
    cli_sandbox: Option<&str>,
) -> (support::Finished, tempfile::TempDir, PathBuf, Stub) {
    let outside = tempfile::tempdir().expect("outside fixture directory");
    let target = outside.path().join("outside-write");
    let stub = Stub::start(shell_write_plan(&target));
    let ego = Ego::against(&stub);
    if let Some(level) = user_sandbox {
        ego.home_file("config.toml", &format!("sandbox = \"{level}\"\n"));
    }

    let mut invocation = ego.args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--output",
        "jsonl",
    ]);
    if let Some(level) = cli_sandbox {
        invocation = invocation.args(["--sandbox", level]);
    }
    let finished = invocation.args(["--allow", "exec(/bin/sh *)", "Ask"]).run();
    (finished, outside, target, stub)
}

fn assert_sandbox_denied(finished: &support::Finished, target: &Path, stub: &Stub, label: &str) {
    assert!(!target.exists(), "{label} wrote outside its confinement");
    match maybe_completed_tool_event(finished) {
        Some(event) => assert_eq!(
            event["ok"], false,
            "{label} unexpectedly completed an outside write: {event}"
        ),
        None => {
            assert_eq!(
                finished.code,
                Some(2),
                "{label} failed without a tool event or usage refusal: {}",
                finished.stderr
            );
            let all_output = format!("{}\n{}", finished.stdout, finished.stderr);
            assert!(
                all_output.contains("missing"),
                "{label} refusal lacked fail-closed capability evidence: {all_output}"
            );
            assert!(
                stub.generations().is_empty(),
                "{label} reached the provider after startup refusal"
            );
        }
    }
}

/// Read the private durable log without depending on its private manifest
/// wrapper.  The JSONL record payload is the authoritative execution log;
/// public `--output jsonl` intentionally omits internal observations.
fn durable_payloads(home: &Path) -> Vec<serde_json::Value> {
    let sessions = home.join("sessions");
    let Ok(entries) = std::fs::read_dir(sessions) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("events.jsonl"))
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .flat_map(|contents| {
            contents
                .lines()
                .filter_map(|line| {
                    serde_json::from_str::<serde_json::Value>(line)
                        .ok()
                        .and_then(|record| record.get("payload").cloned())
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

fn assert_execution_observation_count(finished: &support::Finished, home: &Path, expected: usize) {
    let payloads = durable_payloads(home);
    let kinds = payloads
        .iter()
        .map(|payload| payload["event"]["kind"]["kind"].clone())
        .collect::<Vec<_>>();
    let observed = payloads
        .iter()
        .filter(|payload| payload["event"]["kind"]["kind"] == "ExecutionObserved")
        .count();
    assert_eq!(
        observed, expected,
        "stderr: {}\ndurable payload kinds: {kinds:?}\ndurable payload records: {payloads:?}",
        finished.stderr
    );
}

#[test]
fn sandbox_accepts_exactly_ro_workspace_and_off() {
    for value in ["ro", "workspace", "off"] {
        let args = run_args(["run", "--sandbox", value, "Ask"]);
        assert_eq!(
            args.sandbox,
            Some(
                value
                    .parse::<Confinement>()
                    .expect("known sandbox spelling")
            ),
            "valid sandbox value rejected: {value}"
        );
    }
}

#[test]
fn invalid_sandbox_spelling_is_a_usage_error_before_provider_or_spawn() {
    let finished = Ego::nowhere()
        .args(["--model", "ollama/qwen3", "--sandbox", "bogus", "Ask"])
        .run();

    assert_eq!(finished.code, Some(2));
    assert!(finished.stderr.contains("--sandbox"));
    assert!(finished.stderr.contains("ro"));
    assert!(finished.stderr.contains("workspace"));
    assert!(finished.stderr.contains("off"));
}

#[test]
fn omitted_cli_sandbox_preserves_profile_and_layer_precedence() {
    let args = run_args(["run", "Ask"]);
    assert_eq!(args.sandbox, None);

    let user = layer(
        Source::User,
        "sandbox = \"ro\"\n[profile.ci]\nsandbox = \"workspace\"\n",
    );
    let project = layer(
        Source::Project,
        "sandbox = \"off\"\n[profile.ci]\nsandbox = \"ro\"\n",
    );
    let profile = resolve(&[user.clone(), project.clone()], Trust::Trusted, Some("ci"))
        .expect("profile resolves");
    assert_eq!(
        profile.sandbox,
        Some(Confinement::Ro),
        "an omitted CLI flag must preserve the higher-precedence project profile value"
    );

    let ordinary = resolve(&[user, project], Trust::Trusted, None).expect("layers resolve");
    assert_eq!(ordinary.sandbox, Some(Confinement::Off));
}

#[test]
fn explicit_cli_sandbox_is_an_override_and_absence_defaults_only_at_the_end() {
    let args = run_args(["run", "--sandbox", "workspace", "Ask"]);
    assert_eq!(args.sandbox, Some(Confinement::Workspace));

    let user = layer(
        Source::User,
        "sandbox = \"ro\"\n[profile.ci]\nsandbox = \"off\"\n",
    );
    let project = layer(
        Source::Project,
        "sandbox = \"off\"\n[profile.ci]\nsandbox = \"ro\"\n",
    );
    // This is the public boundary between parsed Args and the flags config
    // layer.  The value must remain optional until a flag was actually given.
    let flags = Sourced {
        source: Source::Flags,
        value: ConfigLayer {
            sandbox: args.sandbox,
            ..ConfigLayer::default()
        },
    };
    let overridden =
        resolve(&[user, project, flags], Trust::Trusted, Some("ci")).expect("layers resolve");
    assert_eq!(
        overridden.sandbox,
        Some(Confinement::Workspace),
        "an explicit CLI value must override project, user, and profile values"
    );
}

#[test]
fn sandbox_optionality_survives_empty_layers_and_defaults_only_after_resolution() {
    let empty = ConfigLayer::default();
    assert_eq!(empty.sandbox, None);

    // A selected profile with no sandbox key must not manufacture one while
    // resolving the profile.  The final execution boundary supplies the
    // documented workspace default after this Option remains None.
    let profile_only = layer(Source::User, "[profile.ci]\nmodel = \"fixture\"\n");
    assert_eq!(profile_only.value.sandbox, None);
    let effective = resolve(&[profile_only], Trust::Trusted, Some("ci"))
        .expect("profile without sandbox resolves");
    assert_eq!(effective.sandbox, None);
    assert_eq!(
        effective.sandbox.unwrap_or_default(),
        Confinement::Workspace
    );

    let absent = resolve(&[], Trust::Trusted, None).expect("empty layers resolve");
    assert_eq!(absent.sandbox, None);
    assert_eq!(absent.sandbox.unwrap_or_default(), Confinement::Workspace);
}

#[test]
fn cli_off_keeps_permission_and_exec_lifecycle_enforced() {
    let stub = Stub::start(exec_plan());
    let finished = Ego::against(&stub)
        .args([
            "--model",
            "ollama/qwen3",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--output",
            "jsonl",
            "--sandbox",
            "off",
            "--allow",
            "exec(/bin/echo *)",
            "Ask",
        ])
        .run();

    assert_eq!(finished.code, Some(0), "stderr: {}", finished.stderr);
    let event = completed_tool_event(&finished);
    assert_eq!(event["ok"], true);
    assert!(
        event["text"]
            .as_str()
            .unwrap_or_default()
            .contains("cli exact argv")
    );
    assert_eq!(stub.generations().len(), 2);
}

#[test]
fn cli_confinement_uses_the_configured_level_without_degrading_to_off() {
    for level in ["ro", "workspace"] {
        let stub = Stub::start(exec_plan());
        let finished = Ego::against(&stub)
            .args([
                "--model",
                "ollama/qwen3",
                "--session-id",
                support::FIXTURE_SESSION_ID,
                "--output",
                "jsonl",
                "--sandbox",
                level,
                "--allow",
                "exec(/bin/echo *)",
                "Ask",
            ])
            .run();

        match maybe_completed_tool_event(&finished) {
            Some(event) if event["ok"] == true => {
                assert_eq!(
                    finished.code,
                    Some(0),
                    "successful {level} run: {}",
                    finished.stderr
                );
                assert!(
                    event["text"]
                        .as_str()
                        .unwrap_or_default()
                        .contains("cli exact argv")
                );
            }
            Some(event) => {
                panic!(
                    "a missing {level} capability must refuse before the provider/tool event: {event}"
                );
            }
            None => {
                assert_eq!(
                    finished.code,
                    Some(2),
                    "startup refusal must be usage exit 2"
                );
                let all_output = format!("{}\n{}", finished.stdout, finished.stderr);
                assert!(
                    all_output.contains("missing"),
                    "startup refusal was not surfaced: {all_output}"
                );
                assert!(
                    stub.generations().is_empty(),
                    "provider was reached after {level} startup refusal"
                );
            }
        }
    }
}

#[test]
fn cli_real_enforcement_matrix_covers_omission_override_and_final_workspace_default() {
    // A lower user layer remains effective when the CLI omits --sandbox.  Off
    // is deliberately chosen so this case is runnable even where a stronger
    // backend is unavailable: the outside write must succeed only because the
    // user layer reached Settings unchanged.
    let (finished, _outside, target, _stub) = shell_write_case(Some("off"), None);
    assert_eq!(
        finished.code,
        Some(0),
        "omitted user sandbox: {}",
        finished.stderr
    );
    assert_eq!(
        maybe_completed_tool_event(&finished).map(|event| event["ok"].clone()),
        Some(serde_json::Value::Bool(true))
    );
    assert!(
        target.exists(),
        "omitted CLI lost the configured user sandbox off"
    );

    // An explicit CLI value is a real flags-layer override, not a parser-only
    // value.  It must deny the same outside write even when the user layer
    // says off.
    let (finished, _outside, target, stub) = shell_write_case(Some("off"), Some("workspace"));
    assert_sandbox_denied(
        &finished,
        &target,
        &stub,
        "explicit workspace over user off",
    );

    // The explicit off value remains a successful, observable override when
    // no lower layer supplies a value.
    let (finished, _outside, target, _stub) = shell_write_case(None, Some("off"));
    assert_eq!(finished.code, Some(0), "explicit off: {}", finished.stderr);
    assert_eq!(
        maybe_completed_tool_event(&finished).map(|event| event["ok"].clone()),
        Some(serde_json::Value::Bool(true))
    );
    assert!(
        target.exists(),
        "explicit off did not reach the null backend"
    );

    for level in ["ro", "workspace"] {
        let (finished, _outside, target, stub) = shell_write_case(None, Some(level));
        assert_sandbox_denied(&finished, &target, &stub, &format!("explicit {level}"));
    }

    // With no layer value and no flag, Effective remains None and only the
    // final Settings boundary supplies Workspace.  An outside write therefore
    // fails closed (or the prerequisite refusal happens before provider use).
    let (finished, _outside, target, stub) = shell_write_case(None, None);
    assert_sandbox_denied(&finished, &target, &stub, "final absent sandbox default");
}

#[cfg(target_os = "linux")]
#[test]
fn linux_landlock_hidden_current_exe_helper_runs_before_clap_and_provider() {
    // A confined child is launched through ego's absolute current-exe helper
    // path.  The helper must consume its private argv prefix before ordinary
    // clap/provider setup; otherwise the target process sees the protocol
    // marker as a CLI argument and the tool call cannot complete.
    let stub = Stub::start(exec_plan());
    let finished = Ego::against(&stub)
        .args([
            "--model",
            "ollama/qwen3",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--output",
            "jsonl",
            "--sandbox",
            "workspace",
            "--allow",
            "exec(/bin/echo *)",
            "Ask",
        ])
        .run();

    match maybe_completed_tool_event(&finished) {
        Some(event) => {
            assert_eq!(
                finished.code,
                Some(0),
                "helper launch failed: {}",
                finished.stderr
            );
            assert_eq!(event["ok"], true);
            assert!(
                event["text"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("cli exact argv")
            );
            assert_eq!(stub.generations().len(), 2);
        }
        None => {
            // A host without a demonstrable Landlock prerequisite may refuse
            // before provider contact, but it may never degrade to sandbox
            // off or pass the private helper marker to clap.
            assert_eq!(
                finished.code,
                Some(2),
                "unexpected helper failure: {}",
                finished.stderr
            );
            let all_output = format!("{}\n{}", finished.stdout, finished.stderr);
            // Not the word "missing": `unmet_confinement` never writes it.
            // What the refusal promises is the flag that asked for the
            // guarantee and the backend that could not demonstrate it, and a
            // host where the backend *does* work never reaches this branch —
            // so an assertion on a word nobody emits reads as passing
            // everywhere except the one place it is meant to hold.
            assert!(
                all_output.contains("could not be demonstrated on this host"),
                "refusal lacked capability evidence: {all_output}"
            );
            assert!(
                stub.generations().is_empty(),
                "provider was reached after helper refusal"
            );
        }
    }
}

#[test]
fn cli_offline_refuses_even_when_filesystem_sandbox_is_off() {
    let stub = Stub::start(exec_plan());
    let finished = Ego::against(&stub)
        .args([
            "--model",
            "ollama/qwen3",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--output",
            "jsonl",
            "--sandbox",
            "off",
            "--offline",
            "--allow",
            "exec(/bin/echo *)",
            "Ask",
        ])
        .run();

    match maybe_completed_tool_event(&finished) {
        Some(event) if event["ok"] == true => {
            assert_eq!(
                finished.code,
                Some(0),
                "successful offline run: {}",
                finished.stderr
            );
            assert!(
                event["text"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("cli exact argv")
            );
        }
        Some(event) => {
            panic!(
                "a missing offline capability must refuse before the provider/tool event: {event}"
            );
        }
        None => {
            assert_eq!(
                finished.code,
                Some(2),
                "startup refusal must be usage exit 2"
            );
            let all_output = format!("{}\n{}", finished.stdout, finished.stderr);
            assert!(
                all_output.contains("--sandbox off could not be demonstrated"),
                "startup refusal was not surfaced: {all_output}"
            );
            assert!(
                stub.generations().is_empty(),
                "provider was reached after startup refusal"
            );
        }
    }
}

#[test]
fn disposable_startup_capability_proof_never_reaches_the_durable_event_log() {
    // The provider resolves and the durable session opens, but --max-turns 0
    // means no actual turn executor is submitted.  The startup capability
    // probe is therefore disposable evidence, not an ExecutionObserved fact
    // for a turn that never existed.
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub);
    let marker = ego.home_file("marker", "");
    let home = marker
        .parent()
        .expect("marker has a home directory")
        .to_path_buf();
    let finished = ego
        .args([
            "--model",
            "ollama/qwen3",
            "--max-turns",
            "0",
            "--sandbox",
            "off",
            "Ask",
        ])
        .run();

    assert_eq!(
        finished.code,
        Some(5),
        "max-turns 0 stops before the first turn"
    );
    assert_execution_observation_count(&finished, &home, 0);
}

#[test]
fn actual_once_executor_appends_exactly_one_execution_observed_fact() {
    let stub = Stub::start(exec_plan());
    let ego = Ego::against(&stub);
    let marker = ego.home_file("marker", "");
    let home = marker
        .parent()
        .expect("marker has a home directory")
        .to_path_buf();
    let finished = ego
        .args([
            "--model",
            "ollama/qwen3",
            "--output",
            "jsonl",
            "--sandbox",
            "off",
            "--allow",
            "exec(/bin/echo *)",
            "Ask",
        ])
        .run();

    assert_eq!(finished.code, Some(0), "stderr: {}", finished.stderr);
    assert_execution_observation_count(&finished, &home, 1);
}

#[test]
fn interactive_piped_seed_executor_appends_one_observation() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("first answer"))],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing();
    let marker = ego.home_file("marker", "");
    let home = marker
        .parent()
        .expect("marker has a home directory")
        .to_path_buf();
    let finished = ego
        .args(["--model", "ollama/qwen3", "--sandbox", "off"])
        .stdin("first prompt\n")
        .run();

    assert_eq!(finished.code, Some(0), "stderr: {}", finished.stderr);
    assert_eq!(
        stub.generations().len(),
        1,
        "the piped stdin is one interactive seed"
    );
    assert_execution_observation_count(&finished, &home, 1);
}

#[test]
fn durable_actual_executor_creates_a_manifest_and_one_observation() {
    let stub = Stub::start(exec_plan());
    let ego = Ego::against(&stub);
    let marker = ego.home_file("marker", "");
    let home = marker
        .parent()
        .expect("marker has a home directory")
        .to_path_buf();
    let finished = ego
        .args([
            "--model",
            "ollama/qwen3",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--sandbox",
            "off",
            "--allow",
            "exec(/bin/echo *)",
            "Ask",
        ])
        .run();

    assert_eq!(finished.code, Some(0), "stderr: {}", finished.stderr);
    assert!(
        home.join("sessions").exists(),
        "durable run wrote no manifest"
    );
    assert_execution_observation_count(&finished, &home, 1);
}
