#![cfg(unix)]

mod support;
use ego_cli::cli::{self, Invocation};
use std::io::Read;
use std::time::{Duration, Instant};
use support::{FIXTURE_SESSION_ID, Places, Plan, Reply, Stub, text, tool_call};

// Catches: a positional resume on a human TTY has no approver and exits 3.
#[test]
fn tty_resume_task_asks_permission_and_stays_interactive() {
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(text("SESSION-CREATED")),
            Reply::Sse(tool_call("regression_case", "exec", r#"{"argv":["seq","1","3"]}"#)),
            Reply::Sse(text("RESUME-TASK-DONE")),
            Reply::Sse(text("LATER-MESSAGE-DONE")),
        ],
        ..Plan::default()
    });
    let places = Places::new();
    let initial = places
        .ego(&stub)
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--session-id",
            FIXTURE_SESSION_ID,
            "create session",
        ])
        .run();
    assert_eq!(initial.code, Some(0), "{}", initial.stderr);
    let resumed = places
        .ego(&stub)
        .subcommand("resume")
        .args([
            FIXTURE_SESSION_ID,
            "--sandbox",
            "off",
            "--bare",
            "resume task 336",
        ])
        .run_pty_responding("Allow run seq 1 3?", "y", "later message 336");
    assert_eq!(resumed.code, Some(0), "{}", resumed.stdout);
    assert!(
        resumed.stdout.contains("RESUME-TASK-DONE"),
        "{}",
        resumed.stdout
    );
    assert!(
        resumed.stdout.contains("LATER-MESSAGE-DONE"),
        "{}",
        resumed.stdout
    );
    let generations = stub.generations();
    assert_eq!(generations.len(), 4, "argv seed must run exactly once");
    let messages = generations.last().unwrap()["messages"].as_array().unwrap();
    for prompt in ["resume task 336", "later message 336"] {
        assert_eq!(
            messages
                .iter()
                .filter(|m| m["role"] == "user" && m["content"] == prompt)
                .count(),
            1
        );
    }
}

// Catches: explicit machine signals or a redirected terminal acquire a human seat.
#[test]
fn resume_machine_signals_keep_the_headless_driver() {
    let places = Places::new();
    let agent = places.file("task.md", "---\nversion: 1\n---\nresume task\n");
    let agent = agent.to_str().unwrap();
    let answer = format!("{FIXTURE_SESSION_ID}=\"yes\"");
    let cases: &[&[&str]] = &[
        &["task", "--output", "text"],
        &["task", "--output=json"],
        &["task", "--output", "jsonl"],
        &["-"],
        &["--answer", &answer],
        &["--file", agent],
        &["task", "--profile", "ci"],
        &["--output", "text"],
        &["--profile", "ci"],
    ];
    for flags in cases {
        let argv = ["ego", "resume", FIXTURE_SESSION_ID]
            .into_iter()
            .chain(flags.iter().copied())
            .map(str::to_owned)
            .collect();
        assert!(
            matches!(
                cli::parse(argv).unwrap().with_terminal(true, true),
                Invocation::Resume(_)
            ),
            "{flags:?}"
        );
    }
    for (stdin, stdout) in [(false, true), (true, false), (false, false)] {
        let argv = ["ego", "resume", FIXTURE_SESSION_ID, "task"]
            .map(str::to_owned)
            .to_vec();
        assert!(matches!(
            cli::parse(argv).unwrap().with_terminal(stdin, stdout),
            Invocation::Resume(_)
        ));
    }
}

// Catches: CLI-saved sessions silently skip consent when confinement is widened.
#[test]
fn cli_saved_session_wider_sandbox_requires_consent() {
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(text("SESSION-CREATED")),
            Reply::Sse(text("WIDER-RESUME-DONE")),
        ],
        ..Plan::default()
    });
    let places = Places::new();
    let initial = places
        .ego(&stub)
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "ro",
            "--session-id",
            FIXTURE_SESSION_ID,
            "create session",
        ])
        .run();
    assert_eq!(initial.code, Some(0), "{}", initial.stderr);
    let denied = places
        .ego(&stub)
        .subcommand("resume")
        .args([
            FIXTURE_SESSION_ID,
            "--sandbox",
            "off",
            "--output",
            "text",
            "resume wider",
        ])
        .run();
    assert_ne!(
        denied.code,
        Some(0),
        "wider authority ran without a human: {} {}",
        denied.stdout,
        denied.stderr
    );
    assert_eq!(
        stub.generations().len(),
        1,
        "denied re-admission must not call the provider"
    );
    let resumed = places
        .ego(&stub)
        .subcommand("resume")
        .args([
            FIXTURE_SESSION_ID,
            "--sandbox",
            "off",
            "--bare",
            "resume wider",
        ])
        .run_pty_responding_seeded("[y] yes", "y");
    assert_eq!(resumed.code, Some(0), "{}", resumed.stdout);
    assert!(
        resumed.stdout.contains("WIDER-RESUME-DONE"),
        "{}",
        resumed.stdout
    );
}

fn bounded_run(ego: support::Ego) -> support::Finished {
    let mut running = ego.spawn();
    let deadline = Instant::now() + Duration::from_secs(60);
    while running.is_running() {
        if Instant::now() >= deadline {
            running.child.kill().expect("kill hung CI process");
            let result = running.finish();
            panic!(
                "CI run waited for an unavailable approver: {}",
                result.stderr
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut stdout = String::new();
    running
        .child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    let mut result = running.finish();
    result.stdout = stdout;
    result
}

// Catches: CI either waits for an unavailable human or ignores profile allow rules.
#[test]
fn ci_resume_permission_exits_three_or_uses_profile_preapproval_without_prompting() {
    for allow in [false, true] {
        let stub = Stub::start(Plan {
            completions: vec![
                Reply::Sse(text("SESSION-CREATED")),
                Reply::Sse(tool_call(
                    "regression_case",
                    "exec",
                    r#"{"argv":["seq","1","3"]}"#,
                )),
                Reply::Sse(text("CI-PREAPPROVED-DONE")),
            ],
            ..Plan::default()
        });
        let places = Places::new();
        let sessions = places.home_path().join("ci-sessions");
        let artifacts = places.home_path().join("ci-artifacts");
        std::fs::create_dir(&sessions).unwrap();
        std::fs::create_dir(&artifacts).unwrap();
        let instructions = places.file("ci-instructions.md", "Run the requested task.\n");
        let initial = places
            .ego(&stub)
            .args([
                "--model",
                "ollama/qwen3",
                "--sandbox",
                "off",
                "--session-id",
                FIXTURE_SESSION_ID,
                "--session-dir",
                sessions.to_str().unwrap(),
                "create session",
            ])
            .run();
        assert_eq!(initial.code, Some(0), "{}", initial.stderr);
        let rules = if allow {
            "\n[profile.ci.permissions]\nallow = ['exec(seq 1 3)']\n"
        } else {
            ""
        };
        places.ego(&stub).home_file("config.toml", &format!(
            "[profile.ci]\nmode='default'\nmodel='ollama/qwen3'\nsandbox='off'\nsession_dir={sessions:?}\nartifact_dir={artifacts:?}\n{rules}"
        ));
        // Exercise both explicit and environment profile selection at process entry.
        let mut ego = places.ego(&stub).subcommand("resume").args([
            FIXTURE_SESSION_ID,
            "ci task",
            "--instructions",
            instructions.to_str().unwrap(),
            "--output",
            "json",
        ]);
        ego = if allow {
            ego.env("EGO_PROFILE", "ci")
        } else {
            ego.args(["--profile", "ci"])
        };
        let result = bounded_run(ego);
        assert_eq!(
            result.code,
            Some(if allow { 0 } else { 3 }),
            "{}\n{}",
            result.stdout,
            result.stderr
        );
        assert!(!result.stdout.contains("Allow run"));
        assert!(!result.stderr.contains("[y] yes"));
        if allow {
            assert!(
                result.stdout.contains("CI-PREAPPROVED-DONE"),
                "{}",
                result.stdout
            );
            assert_eq!(stub.generations().len(), 3);
        } else {
            assert_eq!(stub.generations().len(), 2);
            assert_eq!(result.report()["exit_reason"], "permission");
        }
    }
}

// Catches: equal or narrower CLI authority incorrectly asks an unavailable human.
#[test]
fn cli_saved_session_equal_and_narrower_sandbox_admits_without_human() {
    for initial_sandbox in ["ro", "workspace"] {
        let stub = Stub::start(Plan {
            completions: vec![Reply::Sse(text("INITIAL")), Reply::Sse(text("CONTINUED"))],
            ..Plan::default()
        });
        let places = Places::new();
        let initial = places
            .ego(&stub)
            .args([
                "--thinking",
                "off",
                "--model",
                "ollama/qwen3",
                "--sandbox",
                initial_sandbox,
                "--session-id",
                FIXTURE_SESSION_ID,
                "initial",
            ])
            .run();
        assert_eq!(initial.code, Some(0), "{}", initial.stderr);
        let resumed = places
            .ego(&stub)
            .subcommand("continue")
            .args([
                FIXTURE_SESSION_ID,
                "--thinking",
                "off",
                "--sandbox",
                "ro",
                "--output",
                "text",
                "continue task",
            ])
            .run();
        assert_eq!(resumed.code, Some(0), "{}", resumed.stderr);
        assert!(resumed.stdout.contains("CONTINUED"), "{}", resumed.stdout);
    }
}

// Catches: resume/continue slash commands are submitted as model prompts.
#[test]
fn resume_and_continue_slashes_dispatch_saved_session_commands() {
    use ego_cli::repl::actions::{ActionRegistry, Dispatch, InputOrigin};
    let registry = ActionRegistry::final_surface();
    for (text, id) in [
        ("/resume", None),
        ("/continue", None),
        ("/resume 123", Some("123")),
    ] {
        assert_eq!(
            registry.classify(registry.submit(text.to_owned(), InputOrigin::Typed)),
            Dispatch::Resume(id.map(str::to_owned))
        );
    }
}
