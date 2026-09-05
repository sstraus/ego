//! Golden transcripts: the first interactive launch, three postures.
//!
//! # What these goldens are, and what they are worth
//!
//! `trust_persistence.rs` asserts one rule each — that an empty
//! workspace is never asked, that an answered one is not asked twice. Every
//! one of them is true of a launch that is wrong about *everything else a
//! person sees*: the wording of the question, whether a refusal says anything
//! at all, whether the answer scrolls past under a repaint.
//!
//! These are what `ego` writes at a real terminal for one launch in each
//! posture, committed, so a change to any of that shows up as a diff in a page
//! of text a person can read. See
//! `docs/decisions/2026-09-05-a-trust-answer-is-remembered-per-root.md`.
//!
//! **They prove no bytes reach a terminal.** Like the render transcripts, this
//! is a fixture, not an observation: the escape sequences are stripped and the
//! redraws collapsed, so what is committed is the *sentences* of a launch, not
//! its cursor movements.
//!
//! **Regenerating them is the last resort, not the first.** A diff here means
//! a launch says something different. Read the diff, decide whether the new
//! wording is better, and only then write the files — `cargo test -p ego-cli
//! --test launch_transcripts -- --ignored` does it.

mod support;

use support::{Ego, Places, Plan, Reply, Stub, text};

/// Every posture a first launch can be in, and how the operator answers.
///
/// `None` is a launch that is never asked, which is the whole point of the
/// third case: the transcript has to show the absence.
const LAUNCHES: [(&str, Option<&str>); 3] = [
    ("nothing-to-trust", None),
    ("trusted", Some("yes")),
    ("untrusted", Some("no")),
];

fn stub() -> Stub {
    Stub::start(Plan {
        completions: (0..2)
            .map(|_| Reply::Sse(text("a launch transcript")))
            .collect(),
        ..Plan::default()
    })
}

/// One launch, normalised down to what a person would read off the screen.
fn transcript(case: &str, answer: Option<&str>) -> String {
    let places = Places::new();
    let stub = stub();
    if answer.is_some() {
        places.ego(&stub).file("AGENTS.md", "# a project speaks\n");
    }
    let root = places
        .ego(&stub)
        .workspace_path()
        .canonicalize()
        .expect("a temp workspace")
        .display()
        .to_string();
    let launch = |ego: Ego| {
        let raw = ego
            .conversing()
            .args(["--model", "ollama/qwen3", "--sandbox", "off"])
            .run_pty_trusting(answer.unwrap_or("yes"), "hello")
            .stdout;
        readable(&raw.replace(&root, "<workspace>"))
    };
    let first = launch(places.ego(&stub));
    // The second launch is where a remembered answer proves itself, so both
    // are in the file: a golden of only the first would agree with a build
    // that asks every time.
    let second = launch(places.ego(&stub));
    format!("# {case}: first launch\n{first}\n# {case}: next launch\n{second}")
}

/// Strip the terminal's own bookkeeping and collapse repaints.
///
/// Reedline draws a prompt, reads the cursor-position report this harness
/// owes it, and draws it again, erasing the first copy with sequences that are
/// gone by the time this runs. A golden that kept every one of those copies
/// would be a golden of the harness's timing rather than of what ego says, so
/// the prompt markers, the echoed EOF and every immediate repetition come out
/// and only the sentences stay.
fn readable(raw: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    for line in strip_escapes(raw).replace("\r\n", "\n").split(['\n', '\r']) {
        let mut line = line.replace("^D", "");
        while let Some(rest) = line.strip_prefix("> ").or_else(|| line.strip_prefix("… ")) {
            line = rest.to_owned();
        }
        let line = undouble(line.trim());
        // A bare marker is a prompt this harness happened to catch mid-draw,
        // with nothing typed into it yet.
        if line == ">" || line == "…" {
            continue;
        }
        if line.is_empty() || lines.last().is_some_and(|last| *last == line) {
            continue;
        }
        lines.push(line);
    }
    lines
        .into_iter()
        .map(|line| format!("{line}\n"))
        .collect::<String>()
}

/// Drop a repaint that landed on the same line as the draw it replaced.
fn undouble(line: &str) -> String {
    for half in (1..=line.len() / 2).rev() {
        if line.is_char_boundary(half)
            && line.is_char_boundary(half * 2)
            && line[..half] == line[half..half * 2]
        {
            return line[half..].to_owned();
        }
    }
    line.to_owned()
}

fn strip_escapes(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut plain = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if index + 1 < bytes.len() && bytes[index] == b'\x1b' {
            match bytes[index + 1] {
                b'7' | b'8' => {
                    index += 2;
                    continue;
                }
                b'[' => {
                    index += 2;
                    while index < bytes.len() {
                        let byte = bytes[index];
                        index += 1;
                        if (0x40..=0x7e).contains(&byte) {
                            break;
                        }
                    }
                    continue;
                }
                _ => {}
            }
        }
        // Backspaces, bells and the rest of C0 are cursor bookkeeping, not
        // sentences; `\n` and `\r` stay because they are where lines end.
        let byte = bytes[index];
        if byte >= 0x20 || byte == b'\n' || byte == b'\r' {
            plain.push(byte);
        }
        index += 1;
    }
    String::from_utf8_lossy(&plain).into_owned()
}

fn golden_path(case: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/goldens/launch")
        .join(format!("{case}.txt"))
}

#[test]
fn the_first_launch_says_what_it_is_committed_to_saying() {
    for (case, answer) in LAUNCHES {
        let path = golden_path(case);
        let committed = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("{}: {err}", path.display()));
        assert_eq!(
            transcript(case, answer),
            committed,
            "{case} changed. Read the diff before regenerating it: \
             cargo test -p ego-cli --test launch_transcripts -- --ignored"
        );
    }
}

/// Not a test — the way the files above are written. `--ignored` so it never
/// runs in a gate, because a golden that regenerates itself asserts nothing.
#[test]
#[ignore = "writes the goldens; run deliberately after reading the diff"]
fn regenerate() {
    for (case, answer) in LAUNCHES {
        let path = golden_path(case);
        std::fs::create_dir_all(path.parent().expect("a golden lives in a directory"))
            .expect("a writable fixture directory");
        std::fs::write(&path, transcript(case, answer))
            .unwrap_or_else(|err| panic!("{}: {err}", path.display()));
        println!("wrote {}", path.display());
    }
}
