//! Story 930-6d69: learned build-output recipes shorten repeated exec answers.
//!
//! Every test drives the real binary: the stub plays the model, `/bin/sh`
//! is the build, and the answers are read from what the provider was sent.

use crate::support;

use std::path::Path;

use serde_json::{Value, json};
use support::{Ego, Plan, Reply, Stub, text, tool_call};

const CONFIG: &str = "model = \"ollama/qwen3\"\nsandbox = \"off\"\nbuild_recipes = true\n";
const SCRIPT: &str = "cat build.log; exit $(cat code)";
const SESSION: &str = "0198c7f4-9c1f-7a2b-8c3d-4e5f60719300";

/// A successful build: 40 progress lines, one warning, one summary.
fn noisy() -> String {
    let progress: String = (0..39)
        .map(|i| format!("   Compiling crate{i} v1\n"))
        .collect();
    format!(
        "   Compiling alpha v1\n{progress}warning: unused variable x\n    Finished dev profile\n"
    )
}

fn argv() -> Vec<String> {
    vec!["/bin/sh".into(), "-c".into(), SCRIPT.into()]
}

fn exec() -> Reply {
    Reply::Sse(tool_call(
        "build",
        "exec",
        &json!({ "argv": argv() }).to_string(),
    ))
}

/// The full-output reference exec prints for `bytes`.
fn reference(bytes: &str) -> String {
    format!(
        "{}:{}",
        ego_core::digest::sha256_hex(bytes.as_bytes()),
        bytes.len()
    )
}

fn learn(require: &str) -> Reply {
    let args = json!({
        "argv": argv(),
        "require": [require],
        "omit": [{"match": "prefix", "text": "   Compiling "}],
        "stdout": reference(&noisy()),
    });
    Reply::Sse(tool_call("learn", "build_recipe", &args.to_string()))
}

fn build(workspace: &Path, log: &str, code: u8) {
    std::fs::write(workspace.join("build.log"), log).unwrap();
    std::fs::write(workspace.join("code"), code.to_string()).unwrap();
}

/// Run one headless prompt in `workspace` with `home`'s config.
fn run(home: &Path, workspace: &Path, replies: Vec<Reply>, extra: &[&str]) -> Stub {
    let stub = Stub::start(Plan {
        completions: replies,
        ..Plan::default()
    });
    let mut ego = Ego::at(home, workspace, Some(&stub)).args([
        "--thinking",
        "off",
        "--sandbox",
        "off",
        "--allow",
        "exec(/bin/sh *)",
    ]);
    for arg in extra {
        ego = ego.arg(arg);
    }
    let finished = ego.arg("Build it").run();
    assert_eq!(
        finished.code,
        Some(0),
        "{}\n{}",
        finished.stderr,
        finished.stdout
    );
    stub
}

/// The answer of the last tool message in generation `index`.
fn last_tool_answer(stub: &Stub, index: usize) -> String {
    let generations = stub.generations();
    let messages = generations[index]["messages"].as_array().unwrap().clone();
    messages
        .iter()
        .rev()
        .find(|message| message["role"] == "tool")
        .map(|message| match &message["content"] {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
        .expect("a tool answer")
}

fn recipes(home: &Path, workspace: &Path, action: &[&str]) -> support::Finished {
    let mut ego = Ego::at(home, workspace, None).subcommand("build-recipes");
    for arg in action {
        ego = ego.arg(arg);
    }
    ego.run()
}

fn learned_places() -> support::Places {
    let places = support::Places::new();
    std::fs::write(places.home_path().join("config.toml"), CONFIG).unwrap();
    build(places.workspace_path(), &noisy(), 0);
    let stub = run(
        places.home_path(),
        places.workspace_path(),
        vec![exec(), learn("Finished"), Reply::Sse(text("learned"))],
        &[],
    );
    let first = last_tool_answer(&stub, 1);
    // No recipe yet: the built-in fold keeps the result and points to the full output.
    assert!(first.contains("progress lines folded"), "{first}");
    let learned = last_tool_answer(&stub, 2);
    assert!(learned.contains("learned build recipe"), "{learned}");
    places
}

// Catches: a learned recipe is not reused by a new session, hides a warning,
// drops the evidence reference, makes the complete output unreachable, or
// replay re-filters history after the recipe is deleted.
#[test]
fn a_learned_recipe_shortens_a_later_session_and_replay_keeps_the_delivered_answer() {
    let places = learned_places();
    let (home, workspace) = (places.home_path(), places.workspace_path());
    let listed = recipes(home, workspace, &["ls"]);
    let id = ego_core::build_recipe::recipe_id(&argv());
    assert!(listed.stdout.contains(&id), "{}", listed.stdout);

    let read = json!({ "reference": reference(&noisy()) }).to_string();
    let stub = run(
        home,
        workspace,
        vec![
            exec(),
            Reply::Sse(tool_call("page", "read_tool_result", &read)),
            Reply::Sse(text("built")),
        ],
        &["--session-id", SESSION],
    );
    let compact = last_tool_answer(&stub, 1);
    assert!(compact.contains("omitted 40 of 42 lines"), "{compact}");
    assert!(compact.contains("warning: unused variable x"), "{compact}");
    assert!(compact.contains("Finished dev profile"), "{compact}");
    assert!(compact.contains(&reference(&noisy())), "{compact}");
    assert!(!compact.contains("Compiling alpha"), "{compact}");
    let page = last_tool_answer(&stub, 2);
    assert!(page.contains("   Compiling alpha v1"), "{page}");

    let removed = recipes(home, workspace, &["rm", &id]);
    assert_eq!(removed.code, Some(0), "{}", removed.stderr);
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("again"))],
        ..Plan::default()
    });
    let resumed = Ego::at(home, workspace, Some(&stub))
        .subcommand("resume")
        .args([SESSION, "--thinking", "off", "--sandbox", "off", "next"])
        .run();
    assert_eq!(resumed.code, Some(0), "{}", resumed.stderr);
    let replayed = stub.generations()[0]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "tool")
        .map(|message| message["content"].as_str().unwrap_or_default().to_owned())
        .collect::<Vec<_>>();
    assert!(replayed.contains(&compact), "{replayed:?}");
}

// Catches: a recipe applies to another format, to a failed build, to a
// disabled entry, to another workspace, or while the owner has it off.
#[test]
fn unknown_format_failure_disabled_other_workspace_and_off_keep_the_complete_answer() {
    let places = learned_places();
    let (home, workspace) = (places.home_path(), places.workspace_path());
    let raw = |stub: &Stub| last_tool_answer(stub, 1);

    build(workspace, "   Compiling alpha v1\nother tool layout\n", 0);
    let answer = raw(&run(
        home,
        workspace,
        vec![exec(), Reply::Sse(text("x"))],
        &[],
    ));
    assert!(
        answer.contains("Compiling alpha") && !answer.contains("Build recipe"),
        "{answer}"
    );

    build(workspace, &noisy(), 1);
    let answer = raw(&run(
        home,
        workspace,
        vec![exec(), Reply::Sse(text("x"))],
        &[],
    ));
    // A failed build never takes a recipe; its progress folds, its diagnostics stay.
    assert!(
        answer.contains("progress lines folded")
            && answer.contains("warning: unused variable x")
            && !answer.contains("Build recipe"),
        "{answer}"
    );

    build(workspace, &noisy(), 0);
    let id = ego_core::build_recipe::recipe_id(&argv());
    assert_eq!(recipes(home, workspace, &["disable", &id]).code, Some(0));
    let answer = raw(&run(
        home,
        workspace,
        vec![exec(), Reply::Sse(text("x"))],
        &[],
    ));
    assert!(
        answer.contains("progress lines folded") && !answer.contains("omitted 40 of 42 lines"),
        "{answer}"
    );
    assert_eq!(recipes(home, workspace, &["enable", &id]).code, Some(0));
    let answer = raw(&run(
        home,
        workspace,
        vec![exec(), Reply::Sse(text("x"))],
        &[],
    ));
    assert!(answer.contains("omitted 40 of 42 lines"), "{answer}");

    let other = tempfile::tempdir().unwrap();
    build(other.path(), &noisy(), 0);
    let answer = raw(&run(
        home,
        other.path(),
        vec![exec(), Reply::Sse(text("x"))],
        &[],
    ));
    assert!(
        answer.contains("progress lines folded") && !answer.contains("omitted 40 of 42 lines"),
        "{answer}"
    );

    std::fs::write(
        home.join("config.toml"),
        CONFIG.replace("build_recipes = true", ""),
    )
    .unwrap();
    let stub = run(home, workspace, vec![exec(), Reply::Sse(text("x"))], &[]);
    assert!(
        raw(&stub).contains("progress lines folded"),
        "{}",
        raw(&stub)
    );
    let tools = stub.generations()[0]["tools"].to_string();
    assert!(!tools.contains("build_recipe"), "{tools}");
}

// Catches: a proposal that does not fit its evidence is stored anyway, or a
// recipe file appears for a workspace without a checked recipe.
#[test]
fn a_proposal_that_does_not_fit_its_evidence_is_refused_and_not_stored() {
    let places = support::Places::new();
    let (home, workspace) = (places.home_path(), places.workspace_path());
    std::fs::write(home.join("config.toml"), CONFIG).unwrap();
    build(workspace, &noisy(), 0);
    let stub = run(
        home,
        workspace,
        vec![
            exec(),
            learn("marker that is absent"),
            Reply::Sse(text("x")),
        ],
        &[],
    );
    let refused = last_tool_answer(&stub, 2);
    assert!(refused.contains("does not apply"), "{refused}");
    assert!(!home.join("build-recipes").exists());
    let listed = recipes(home, workspace, &["ls"]);
    assert!(
        listed.stdout.contains("no build recipes"),
        "{}",
        listed.stdout
    );
}

// Catches: the model saving the same argv again turns a recipe the owner
// disabled back on, so `ego build-recipes disable` does not stick.
#[test]
fn saving_a_recipe_again_keeps_the_owner_disable() {
    let places = learned_places();
    let (home, workspace) = (places.home_path(), places.workspace_path());
    let id = ego_core::build_recipe::recipe_id(&argv());
    assert_eq!(recipes(home, workspace, &["disable", &id]).code, Some(0));
    let stub = run(
        home,
        workspace,
        vec![exec(), learn("Finished"), Reply::Sse(text("again"))],
        &[],
    );
    assert!(last_tool_answer(&stub, 1).contains("progress lines folded"));
    assert!(last_tool_answer(&stub, 2).contains("revision 2"));
    let listed = recipes(home, workspace, &["ls"]).stdout;
    assert!(listed.contains("r2  disabled"), "{listed}");
}
