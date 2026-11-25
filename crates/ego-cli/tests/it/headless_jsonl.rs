//! Real CLI, real stdin/stdout pipes, and the isolated provider loopback.

use std::io::{BufRead, Write};
use std::process::ChildStdin;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use serde_json::{Value, json};

use crate::support::{self, Places, Plan, ReleaseGate, Reply, Running, Stub, paced, text};

const WAIT: Duration = Duration::from_secs(10);
const SESSION: &str = support::FIXTURE_SESSION_ID;

struct Stream {
    child: Option<Running>,
    stdin: Option<ChildStdin>,
    received: mpsc::Receiver<String>,
    gate: Arc<ReleaseGate>,
}

impl Stream {
    fn start(places: &Places, stub: &Stub, gate: Arc<ReleaseGate>) -> Self {
        let mut child = places
            .ego(stub)
            .args([
                "--model",
                "ollama/qwen3",
                "--sandbox",
                "off",
                "--session-id",
                SESSION,
                "--input-format",
                "jsonl",
                "--output",
                "jsonl",
            ])
            .spawn_conversing();
        let stdin = child.child.stdin.take().unwrap();
        let stdout = child.stdout();
        let (sender, received) = mpsc::channel();
        std::thread::spawn(move || {
            for line in stdout.lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child: Some(child),
            stdin: Some(stdin),
            received,
            gate,
        }
    }

    fn prompt(&mut self, prompt: &str) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{}", json!({"prompt": prompt})).expect("write a complete prompt line");
        stdin.flush().unwrap();
    }

    fn next(&self) -> Option<Value> {
        match self.received.recv_timeout(WAIT) {
            Ok(line) => Some(
                serde_json::from_str(&line).unwrap_or_else(|e| panic!("bad JSONL: {e}: {line}")),
            ),
            Err(mpsc::RecvTimeoutError::Disconnected) => None,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                panic!("timed out waiting for JSONL output or EOF")
            }
        }
    }

    fn wait_for_active_delta(&self) {
        loop {
            let line = self
                .next()
                .expect("process exited before the admitted turn streamed");
            assert_ne!(line["type"], "result", "provider gate is still closed");
            if line["type"] == "item_delta" && line.to_string().contains("first answer") {
                return;
            }
        }
    }

    fn finish(mut self, expected: i32) -> Vec<Value> {
        let mut tail = Vec::new();
        while let Some(line) = self.next() {
            tail.push(line);
        }
        let finished = self.child.take().unwrap().finish();
        assert_eq!(finished.code, Some(expected), "{}", finished.stderr);
        tail
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.child.kill();
            let _ = child.child.wait();
        }
        self.gate.release();
    }
}

fn provider() -> (Stub, Arc<ReleaseGate>) {
    let Reply::Paced { head, tail, .. } = paced("first answer", Duration::ZERO) else {
        unreachable!("paced fixture reply")
    };
    let gate = ReleaseGate::new();
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::GatedPaced {
                head,
                tail,
                gate: Arc::clone(&gate),
            },
            Reply::Sse(text("second answer")),
            Reply::Sse(text("third answer")),
        ],
        ..Plan::default()
    });
    (stub, gate)
}

// Catches: a second prompt runs before the first settles, or EOF discards a
// complete line buffered during generation (or adds a phantom empty turn).
#[test]
fn buffered_turns_drain_in_order_before_eof_exit() {
    let places = Places::new();
    let (stub, gate) = provider();
    let mut stream = Stream::start(&places, &stub, Arc::clone(&gate));
    stream.prompt("first prompt");
    stream.wait_for_active_delta();
    stream.prompt("second prompt");
    assert_eq!(stub.generations().len(), 1, "turn A is still gated");
    drop(stream.stdin.take());
    gate.release();

    let events = stream.finish(0);
    let turn_order: Vec<_> = events
        .iter()
        .filter_map(|event| match event["type"].as_str()? {
            kind @ ("turn_started" | "turn_completed" | "result") => Some(kind),
            _ => None,
        })
        .collect();
    assert_eq!(
        turn_order,
        [
            "turn_completed",
            "result",
            "turn_started",
            "turn_completed",
            "result"
        ]
    );
    let results: Vec<_> = events
        .iter()
        .filter(|line| line["type"] == "result")
        .collect();
    for (result, answer) in results.into_iter().zip(["first answer", "second answer"]) {
        assert_eq!(result["session"], SESSION);
        assert_eq!(result["result"], answer);
        assert!(result["usage"]["cache_read_tokens"].is_u64(), "{result}");
    }
    let generations = stub.generations();
    assert_eq!(generations.len(), 2);
    assert!(!generations[0].to_string().contains("second prompt"));
    let second = generations[1].to_string();
    for text in ["first prompt", "first answer", "second prompt"] {
        assert!(second.contains(text), "missing {text} in {second}");
    }
}

// Catches: shutdown waits for stdin EOF, admits a buffered prompt after the
// signal, or leaves the writer lease held after cancelling the live turn.
#[cfg(unix)]
#[test]
fn sigterm_settles_active_turn_and_leaves_session_resumable_with_stdin_open() {
    let places = Places::new();
    let (stub, gate) = provider();
    let mut stream = Stream::start(&places, &stub, Arc::clone(&gate));
    stream.prompt("interrupt this prompt");
    stream.wait_for_active_delta();
    stream.prompt("never admit this prompt");
    let pid = stream.child.as_ref().unwrap().child.id();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(pid).unwrap()),
        nix::sys::signal::Signal::SIGTERM,
    )
    .expect("signal the owned CLI child");
    let tail = stream.finish(143);
    assert_eq!(
        tail.iter()
            .filter(|line| line["type"] == "turn_completed")
            .count(),
        1
    );
    assert_eq!(stub.generations().len(), 1);

    let streamed = places
        .ego(&stub)
        .subcommand("resume")
        .args([
            SESSION,
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--input-format",
            "jsonl",
            "--output",
            "jsonl",
        ])
        .stdin("{\"prompt\":\"first record after interruption\"}\n")
        .run();
    assert_eq!(streamed.code, Some(0), "{}", streamed.stderr);
    let events: Vec<Value> = streamed
        .stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let results: Vec<_> = events
        .iter()
        .filter(|line| line["type"] == "result")
        .collect();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["session"], SESSION);
    assert_eq!(results[0]["result"], "second answer");
    let requests = stub.generations();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1]
            .to_string()
            .contains("first record after interruption")
    );

    let resumed = places
        .ego(&stub)
        .subcommand("resume")
        .args([
            SESSION,
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--output",
            "json",
            "continue after signal",
        ])
        .run();
    assert_eq!(resumed.code, Some(0), "{}", resumed.stderr);
    assert_eq!(resumed.report()["session"], SESSION);
    let generations = stub.generations();
    assert_eq!(generations.len(), 3);
    assert!(
        !generations[2]
            .to_string()
            .contains("never admit this prompt")
    );
}

// Catches: malformed/blank records become model prompts, or their position is lost.
#[test]
fn invalid_records_fail_at_their_line_without_generation() {
    for input in [
        "\n",
        "not json\n",
        "{\"prompt\":\" \"}\n",
        "{\"prompt\":42}\n",
    ] {
        let places = Places::new();
        let stub = Stub::start(Plan::default());
        let finished = places
            .ego(&stub)
            .args([
                "--model",
                "ollama/qwen3",
                "--sandbox",
                "off",
                "--input-format",
                "jsonl",
                "--output",
                "jsonl",
            ])
            .stdin(input)
            .run();
        assert_eq!(finished.code, Some(2), "{}", finished.stderr);
        assert!(finished.stderr.contains("line 1"), "{}", finished.stderr);
        assert!(stub.generations().is_empty());
    }
}

// Catches: resumed JSONL reopens a new session, loses fork history or omits cache usage.
#[test]
fn forked_jsonl_turns_retain_history_and_provider_cache_counts() {
    let places = Places::new();
    let cached = text("cached answer").replace(
        "\"prompt_tokens\":10,",
        "\"prompt_tokens\":10,\"prompt_tokens_details\":{\"cached_tokens\":6},",
    );
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(text("source answer")),
            Reply::Sse(cached.clone()),
            Reply::Sse(cached),
        ],
        ..Plan::default()
    });
    let source = places
        .ego(&stub)
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--session-id",
            SESSION,
            "--output",
            "json",
            "source prompt",
        ])
        .run();
    assert_eq!(source.code, Some(0), "{}", source.stderr);
    let fork = places
        .ego(&stub)
        .subcommand("resume")
        .args([
            SESSION,
            "--fork",
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--input-format",
            "jsonl",
            "--output",
            "jsonl",
        ])
        .stdin("{\"prompt\":\"fork prompt\"}\n{\"prompt\":\"follow up\"}\n")
        .run();
    assert_eq!(fork.code, Some(0), "{}", fork.stderr);
    let events: Vec<Value> = fork
        .stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let results: Vec<_> = events
        .iter()
        .filter(|event| event["type"] == "result")
        .collect();
    assert_eq!(results.len(), 2);
    assert_ne!(results[0]["session"], SESSION);
    assert_eq!(results[0]["session"], results[1]["session"]);
    assert_eq!(results[0]["result"], "cached answer");
    assert_eq!(results[0]["usage"]["cache_read_tokens"], 6);
    assert_eq!(results[1]["usage"]["cache_read_tokens"], 12);
    let requests = stub.generations();
    assert_eq!(requests.len(), 3);
    for needle in ["source prompt", "source answer", "fork prompt", "follow up"] {
        assert!(requests[2].to_string().contains(needle), "missing {needle}");
    }
}

// Catches: the input flag still chooses a human seat or silently combines prompt sources.
#[test]
fn jsonl_help_and_prompt_source_conflicts_are_explicit() {
    use ego_cli::cli::{self, Invocation, Stdin};
    for command in ["run", "resume"] {
        let parse = |tail: &[&str]| {
            cli::parse(
                ["ego", command]
                    .into_iter()
                    .chain(tail.iter().copied())
                    .map(str::to_owned)
                    .collect(),
            )
        };
        let Invocation::Answered(help) = parse(&["--help"]).unwrap() else {
            panic!("help")
        };
        assert!(help.contains("--input-format"));
        assert!(help.contains("{\"prompt\":\"...\"}"));
        let invocation = parse(&["--input-format", "jsonl"])
            .unwrap()
            .with_terminal(true, true);
        let (Invocation::Run(args) | Invocation::Resume(args)) = invocation else {
            panic!("headless")
        };
        assert_eq!(args.stdin, Stdin::Jsonl);
        for prompt in ["prompt text", "-"] {
            assert!(parse(&["--input-format", "jsonl", prompt]).is_err());
        }
        assert!(parse(&["--input-format", "jsonl", "--file", "unused.md"]).is_err());
    }
}
