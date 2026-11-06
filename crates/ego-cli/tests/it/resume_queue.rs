//! contract for `ego resume ID --queue PROMPT` (`CLI.md` §2).
//!
//! A queued prompt waits while another process owns the session, then runs
//! through the ordinary headless resume and exits with that turn's status.
//! Every test here drives the real binary against the loopback provider fake.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ego_cli::cli::{Invocation, parse};

use crate::support::{self, FIXTURE_SESSION_ID, Places, Plan, ReleaseGate, Reply, Running, Stub};

const CONFIG: &str = "model = \"ollama/qwen3\"\nthinking = \"off\"\nsandbox = \"off\"\n";

/// A text reply whose end waits for `gate`, so its process holds the session.
fn held(content: &str) -> (Reply, Arc<ReleaseGate>) {
    let Reply::Paced { head, tail, .. } = support::paced(content, Duration::ZERO) else {
        unreachable!("paced always returns a paced reply");
    };
    let gate = ReleaseGate::new();
    (
        Reply::GatedPaced {
            head,
            tail,
            gate: Arc::clone(&gate),
        },
        gate,
    )
}

/// Save the fixture session with one completed turn.
fn saved(places: &Places, stub: &Stub) {
    places.ego(stub).home_file("config.toml", CONFIG);
    let first = places
        .ego(stub)
        .args(["--session-id", FIXTURE_SESSION_ID, "Begin"])
        .run();
    assert_eq!(first.code, Some(0), "stderr: {}", first.stderr);
}

fn queued(places: &Places, stub: &Stub, extra: &[&str], prompt: &str) -> Running {
    let mut ego = places
        .ego(stub)
        .subcommand("resume")
        .args([FIXTURE_SESSION_ID, "--queue"]);
    for arg in extra {
        ego = ego.arg(arg);
    }
    ego.arg(prompt).spawn()
}

fn wait_for_generations(stub: &Stub, count: usize) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while stub.generations().len() < count {
        assert!(
            Instant::now() < deadline,
            "the provider saw no generation {count}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn user_texts(generation: &serde_json::Value) -> Vec<String> {
    generation["messages"]
        .as_array()
        .expect("messages are an array")
        .iter()
        .filter_map(|message| message["content"].as_str().map(str::to_owned))
        .collect()
}

// Catches: two queued prompts on one session both opening it, so the second
// either fails on the writer lease or runs a turn beside the first.
#[test]
fn concurrent_queued_prompts_run_one_turn_at_a_time() {
    let places = Places::new();
    let (second, gate) = held("Second.");
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(support::text("First.")),
            second,
            Reply::Sse(support::text("Third.")),
        ],
        ..Plan::default()
    });
    saved(&places, &stub);

    let first = queued(&places, &stub, &[], "Continue");
    wait_for_generations(&stub, 2);
    let mut waiting = queued(&places, &stub, &[], "Finally");
    std::thread::sleep(Duration::from_millis(1_500));
    assert!(
        waiting.is_running(),
        "the second queued prompt did not wait for the turn in progress"
    );
    assert_eq!(
        stub.generations().len(),
        2,
        "a second turn reached the provider while the first was running"
    );

    gate.release();
    let first = first.finish();
    let waiting = waiting.finish();
    assert_eq!(first.code, Some(0), "stderr: {}", first.stderr);
    assert_eq!(waiting.code, Some(0), "stderr: {}", waiting.stderr);
    let generations = stub.generations();
    assert_eq!(
        generations.len(),
        3,
        "each queued prompt ran exactly one turn"
    );
    let history = user_texts(&generations[2]);
    for expected in ["Begin", "First.", "Continue", "Second.", "Finally"] {
        assert!(
            history.iter().any(|text| text == expected),
            "the queued turn did not run on the session the first left: {history:?}"
        );
    }
}

// Catches: the wait ignoring --timeout, so a cron job blocks forever on a
// session that never becomes idle.
#[test]
fn a_queued_prompt_that_waits_past_its_timeout_exits_124_without_a_turn() {
    let places = Places::new();
    let (second, gate) = held("Second.");
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(support::text("First.")), second],
        ..Plan::default()
    });
    saved(&places, &stub);

    let first = queued(&places, &stub, &[], "Continue");
    wait_for_generations(&stub, 2);
    let late = queued(&places, &stub, &["--timeout", "1s"], "Never").finish();
    assert_eq!(late.code, Some(124), "stderr: {}", late.stderr);
    assert!(
        late.stderr.contains("--timeout expired"),
        "stderr: {}",
        late.stderr
    );

    gate.release();
    let first = first.finish();
    assert_eq!(first.code, Some(0), "stderr: {}", first.stderr);
    assert_eq!(
        stub.generations().len(),
        2,
        "the timed-out prompt reached the provider"
    );
}

// Catches: the queue reporting its own successful wait instead of the
// turn's result, which hides a provider failure from the calling script.
#[test]
fn a_queued_prompt_exits_with_the_status_of_its_turn() {
    let places = Places::new();
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(support::text("First.")), Reply::Status(500)],
        ..Plan::default()
    });
    saved(&places, &stub);

    let failed = queued(&places, &stub, &["--retry", "0"], "Continue").finish();
    assert_eq!(failed.code, Some(4), "stderr: {}", failed.stderr);
}

// Catches: `--queue` without a prompt opening an interactive session or
// waiting on stdin, or sharing an invocation with an answer it would wait
// to apply.
#[test]
fn queue_needs_a_headless_prompt() {
    let argv = |args: &[&str]| {
        std::iter::once("ego")
            .chain(args.iter().copied())
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    let terminal = parse(argv(&["resume", FIXTURE_SESSION_ID, "--queue", "next"]))
        .expect("a queued prompt parses")
        .with_terminal(true, true);
    let Invocation::Resume(args) = terminal else {
        panic!("a queued prompt on a terminal must stay headless: {terminal:?}");
    };
    assert!(args.queue);

    let answered = parse(argv(&[
        "resume",
        FIXTURE_SESSION_ID,
        "--queue",
        "--answer",
        &format!("{}=\"yes\"", ego_core::event::id::ItemId::new()),
    ]));
    assert!(answered.is_err(), "--queue and --answer must not combine");

    let promptless = parse(argv(&["resume", FIXTURE_SESSION_ID, "--queue"]));
    assert!(
        promptless.is_err(),
        "--queue without a prompt must be refused"
    );
}

fn git(root: &std::path::Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "user.name=Queue Test",
            "-c",
            "user.email=queue@example.invalid",
        ])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("git prints UTF-8")
}

// Catches: the checkout lease of a --worktree session taken before the queue
// wait, so a queued prompt run from that checkout exits at once with "in use
// by another ego process" while the owner runs, instead of waiting for it.
#[test]
fn a_queued_prompt_waits_for_the_owner_of_a_worktree_session() {
    let places = Places::new();
    let (second, gate) = held("Second.");
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(support::text("First.")),
            second,
            Reply::Sse(support::text("Third.")),
        ],
        ..Plan::default()
    });
    let repository = places.workspace_path();
    git(repository, &["init", "-q", "-b", "master"]);
    std::fs::write(repository.join("tracked"), "committed\n").unwrap();
    git(repository, &["add", "tracked"]);
    git(repository, &["commit", "-qm", "initial"]);
    places.ego(&stub).home_file("config.toml", CONFIG);
    let first = places
        .ego(&stub)
        .subcommand("run")
        .args(["--worktree", "feat/queued", "--output", "json", "Begin"])
        .run();
    assert_eq!(first.code, Some(0), "stderr: {}", first.stderr);
    let session = first.report()["session"]
        .as_str()
        .expect("session id")
        .to_owned();
    let checkout = git(repository, &["worktree", "list", "--porcelain"])
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .nth(1)
        .expect("the linked checkout")
        .to_owned();

    let owner = places
        .ego(&stub)
        .subcommand("resume")
        .args([session.as_str(), "-C", checkout.as_str(), "Continue"])
        .spawn();
    wait_for_generations(&stub, 2);
    let mut waiting = places
        .ego(&stub)
        .subcommand("resume")
        .args([
            session.as_str(),
            "--queue",
            "-C",
            checkout.as_str(),
            "Finally",
        ])
        .spawn();
    std::thread::sleep(Duration::from_millis(1_500));
    assert!(
        waiting.is_running(),
        "the queued prompt did not wait for the owner of the worktree session"
    );

    gate.release();
    let owner = owner.finish();
    let waiting = waiting.finish();
    assert_eq!(owner.code, Some(0), "stderr: {}", owner.stderr);
    assert_eq!(waiting.code, Some(0), "stderr: {}", waiting.stderr);
    assert_eq!(
        stub.generations().len(),
        3,
        "the queued prompt ran its turn"
    );
}
