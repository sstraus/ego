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
//! `docs/internals/decisions/2026-09-05-a-trust-answer-is-remembered-per-root.md`.
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

use crate::support;

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

/// One launch, normalised down to what a person would read off the screen.
fn transcript(case: &str, answer: Option<&str>) -> String {
    transcript_with_reply(case, answer, Reply::Sse(text("a launch transcript")))
}

fn transcript_with_reply(case: &str, answer: Option<&str>, reply: Reply) -> String {
    let places = Places::new();
    let stub = Stub::start(Plan {
        completions: vec![reply; 2],
        ..Plan::default()
    });
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
        // One golden for every OS: the separator after the root is `/`.
        let redacted = raw
            .replace(&root, "<workspace>")
            .replace("<workspace>\\", "<workspace>/");
        redact_session_id(&readable(&redacted))
    };
    let first = launch(places.ego(&stub));
    // The second launch is where a remembered answer proves itself, so both
    // are in the file: a golden of only the first would agree with a build
    // that asks every time.
    let second = launch(places.ego(&stub));
    format!("# {case}: first launch\n{first}\n# {case}: next launch\n{second}")
}

/// The resume hint names a session id minted per run, so the golden carries a
/// placeholder in its place. Without it every run would differ from the file.
fn redact_session_id(text: &str) -> String {
    const HINT: &str = "ego resume ";
    const ID_LEN: usize = "0198c7f4-9c1f-7a2b-8c3d-4e5f60718293".len();
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find(HINT) {
        let id_start = at + HINT.len();
        out.push_str(&rest[..id_start]);
        out.push_str("<session>");
        rest = &rest[(id_start + ID_LEN).min(rest.len())..];
    }
    out.push_str(rest);
    out
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
        let mut line = line.replace("^D", "").trim_start().to_owned();
        // A CPR sent as raw mode retires can be echoed by the PTY in caret
        // notation. It is terminal bookkeeping, just like an unquoted CPR.
        if line
            .trim()
            .strip_prefix("^[[")
            .and_then(|report| report.strip_suffix('R'))
            .and_then(|report| report.split_once(';'))
            .is_some_and(|(row, col)| {
                !row.is_empty()
                    && !col.is_empty()
                    && row.bytes().all(|byte| byte.is_ascii_digit())
                    && col.bytes().all(|byte| byte.is_ascii_digit())
            })
        {
            continue;
        }
        while let Some(rest) = line.strip_prefix("> ").or_else(|| line.strip_prefix("… ")) {
            line = rest.to_owned();
        }
        // The fixed `hello` fixture may carry its prompt twice across one
        // cursor redraw. Do not collapse arbitrary answer text like `A> A`.
        let line = if line.trim() == "hello> hello" {
            "hello"
        } else {
            line.trim()
        };
        let mut line = undouble(without_glued_status(line));
        // The real temporary root is wrapped at 79 cells before it is
        // redacted. Join this fixture's split question before identifying
        // modal redraws; otherwise the redraw starts with `[y/N]`, not Trust.
        if lines
            .last()
            .is_some_and(|last| last == "Trust project files in <workspace>?")
            && (line == "[y/N]" || line.starts_with("[y/N] "))
        {
            let question = lines.pop().expect("the wrapped trust question");
            line = format!("{question} {line}");
        }
        // A longer root (a Windows temporary directory) wraps before itself.
        if lines
            .last()
            .is_some_and(|last| last == "Trust project files in")
            && line.starts_with("<workspace>?")
        {
            let question = lines.pop().expect("the wrapped trust question");
            line = format!("{question} {line}");
        }
        let mut repainted_modal = false;
        if line.starts_with("Trust project files")
            && let Some((_, redrawn_banner)) = line.split_once("Project files offered for trust:")
        {
            line = format!("Project files offered for trust:{redrawn_banner}");
            repainted_modal = true;
        }
        if line.starts_with("Project files offered for trust:")
            && let Some(start) = lines
                .iter()
                .position(|previous| previous.starts_with("Project files offered for trust:"))
            && (repainted_modal
                || lines[start..]
                    .iter()
                    .any(|previous| previous.starts_with("Trust project files")))
        {
            // Reedline repainted the entire modal. Keep its last complete
            // frame, which carries the operator's answer.
            lines.truncate(start);
        }
        // A capture can lose the separator at the right edge of the terminal.
        for answer in ["yes", "no"] {
            if line == format!("Trust project files in <workspace>? [y/N]{answer}") {
                line = format!("Trust project files in <workspace>? [y/N] {answer}");
                break;
            }
        }
        // A bare marker is a prompt this harness happened to catch mid-draw,
        // with nothing typed into it yet.
        if line == ">" || line == "…" || is_prompt_repaint(&line) || is_thinking_repaint(&line) {
            continue;
        }
        if line.is_empty() || lines.last().is_some_and(|last| *last == line) {
            continue;
        }
        if lines
            .last()
            .is_some_and(|last| line.len() > last.len() && line.starts_with(last.as_str()))
        {
            lines.pop();
        }
        for prefix in ["ego ", "εgo: ego "] {
            if let Some(identity) = line.strip_prefix(prefix) {
                let version = identity
                    .split_once(" · ")
                    .map_or(identity, |(version, _)| version);
                if version
                    .bytes()
                    .next()
                    .is_some_and(|byte| byte.is_ascii_digit())
                    && !version.contains(char::is_whitespace)
                {
                    let tail = without_build_stamp(&identity[version.len()..]);
                    line = format!("{prefix}<version>{tail}");
                    break;
                }
            }
        }
        lines.push(line);
    }
    lines
        .into_iter()
        .map(|line| format!("{line}\n"))
        .collect::<String>()
}

/// Drop the build stamp (` · <commit> · <profile>`) that follows the version: it names the
/// binary, so it changes with every commit and cannot sit in a committed golden.
fn without_build_stamp(tail: &str) -> &str {
    let Some(rest) = tail.strip_prefix(" · ") else {
        return tail;
    };
    let Some((commit, rest)) = rest.split_once(" · ") else {
        return tail;
    };
    let is_commit = commit == "unknown"
        || (commit.len() == 9 && commit.bytes().all(|byte| byte.is_ascii_hexdigit()));
    for profile in ["debug", "release"] {
        if is_commit
            && let Some(after) = rest.strip_prefix(profile)
            && (after.is_empty() || after.starts_with(" · "))
        {
            return after;
        }
    }
    tail
}

/// A Windows pseudo-console redraws the answer over a finished status row
/// without a line break, so the answer follows `Esc to interrupt` directly.
fn without_glued_status(line: &str) -> &str {
    const END: &str = "Esc to interrupt";
    if let Some(at) = line.find(END) {
        let (head, tail) = line.split_at(at + END.len());
        if is_thinking_repaint(head)
            && tail
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric())
        {
            return tail;
        }
    }
    line
}

fn is_prompt_repaint(line: &str) -> bool {
    line.replace('>', "").trim().is_empty()
}

fn is_thinking_repaint(line: &str) -> bool {
    // An empty composer can be redrawn immediately after a status frame.
    // Remove only its final marker, then validate the entire status grammar.
    let line = line.strip_suffix('>').unwrap_or(line);
    // Cursor-only updates lose their boundaries when escapes are stripped.
    // Validate every adjacent frame, not one duration containing other frames.
    let mut frames = line.split(['◐', '◓', '◑', '◒']);
    frames.next() == Some("")
        && frames.all(|frame| {
            frame
                .strip_prefix(" Working ")
                .is_some_and(is_thinking_elapsed)
        })
}

fn is_thinking_elapsed(elapsed: &str) -> bool {
    if elapsed.is_empty() {
        return false;
    }
    // Replace numeric runs, not words: a real answer after a status-like prefix
    // must still survive. A bounded row can end anywhere in this grammar.
    let mut shape = String::new();
    let mut was_digit = false;
    for character in elapsed.chars() {
        let digit = character.is_ascii_digit();
        if !digit || !was_digit {
            shape.push(if digit { '#' } else { character });
        }
        was_digit = digit;
    }
    ["#s", "#m #s", "#h #m"].iter().any(|duration| {
        [
            " · Esc to interrupt",
            " · ↓ ~# tokens · Esc to interrupt",
            " · thinking started #s ago · Esc to interrupt",
            " · ↓ ~# tokens · thinking started #s ago · Esc to interrupt",
        ]
        .iter()
        .any(|suffix| format!("{duration}{suffix}").starts_with(&shape))
    })
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
                b']' => {
                    index += 2;
                    while index < bytes.len() {
                        if bytes[index] == b'\x07' {
                            index += 1;
                            break;
                        }
                        if index + 1 < bytes.len()
                            && bytes[index] == b'\x1b'
                            && bytes[index + 1] == b'\\'
                        {
                            index += 2;
                            break;
                        }
                        index += 1;
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

#[test]
fn readable_discards_status_prompt_repaints_but_keeps_transcript_rows() {
    let raw = "\x1b]0;C:\\ego.exe\x07Trust files? [y/N]\r\nTrust files? [y/N] yes\r\nhello\r\n> >\r\nanswer\r\n> ";

    assert_eq!(readable(raw), "Trust files? [y/N] yes\nhello\nanswer\n");
}

#[test]
fn readable_discards_reply_row_repaints_but_keeps_the_answer() {
    // The real PTY stream after the reply-row change redraws an echoed prompt,
    // a timed thinking row, and a busy composer before the answer arrives.
    let raw = "hello> hello\r\n◐ Working 0s · Esc to interrupt\r\n>\r\n◓ Working 1s · Esc to interrupt\r\n>\r\na launch transcript\r\n";
    assert_eq!(readable(raw), "hello\na launch transcript\n");
}

// Catches: adjacent status frames with different clocks or token counts survive.
#[test]
fn readable_discards_concatenated_working_repaints() {
    // Recorded from rb result 20261029T042025Z-53407 and the story 702 RED
    // (20261029T061159Z-5592), after the existing escape-stripping boundary.
    for row in [
        "◓ Working 1s · ↓ ~5 tokens · Esc to interrupt◑ Working 2s · ↓ ~5 tokens · Esc to interrupt",
        "◓ Working 1s · Esc to interrupt◓ Working 1s · ↓ ~5 tokens · Esc to interrupt",
    ] {
        assert_eq!(
            readable(&format!("hello\n{row}\na launch transcript\n")),
            "hello\na launch transcript\n",
        );
        let answer = format!("{row} is an example, not a repaint");
        assert_eq!(readable(&answer), format!("{answer}\n"));
    }
}

// Catches: a completed status frame joined to an empty composer leaks into
// launch goldens, while status-like answers must retain every character.
#[test]
fn readable_discards_working_repaint_followed_by_an_empty_prompt() {
    let row = "◑ Working 2s · ↓ ~5 tokens · Esc to interrupt>";
    assert_eq!(
        readable(&format!("hello\r\n{row}\r\na launch transcript\r\n")),
        "hello\na launch transcript\n"
    );
    for answer in [
        "◑ Working on the answer>",
        "◑ Working 2s · ↓ ~5 tokens · Esc to interrupt>answer",
        "◑ Working 2s · ↓ ~5 tokens · Esc to interrupt is a hint>",
        "Working 2s · ↓ ~5 tokens · Esc to interrupt>",
        "A> A>",
    ] {
        assert_eq!(readable(answer), format!("{answer}\n"));
    }
}

// Catches: live progress leaking into goldens, or filtering real answer text.
#[test]
fn readable_discards_extended_working_repaints_but_keeps_real_answers() {
    let rows = [
        "◐ Working 0s · ↓ ~5 tokens · Esc to interrupt",
        "◓ Working 1m 0s · thinking started 3s ago · Esc to interrupt",
        "◑ Working 1h 05m · ↓ ~438 tokens · thinking started 12s ago · Esc to interrupt",
        "◒ Working 42s · Esc to interrupt",
    ];
    for row in rows {
        // Truncation cuts at a terminal cell without adding an ellipsis.
        for width in [19, 39, 79, 120] {
            let bounded: String = row.chars().take(width).collect();
            assert_eq!(
                readable(&format!("hello\r\n{bounded}\r\n>\r\nanswer\r\n")),
                "hello\nanswer\n",
                "{bounded:?}"
            );
        }
    }
    for answer in [
        "Working 0s · ↓ ~5 tokens · Esc to interrupt",
        "◐ Working on the answer",
        "◐ Working 0s is the example",
        "◐ Working 0s · ↓ ~5 tokens are estimated",
        "◐ Working 0s · Esc to interrupt is a hint",
        "A> A",
    ] {
        assert_eq!(readable(answer), format!("{answer}\n"));
    }
}

#[test]
fn redact_session_id_replaces_only_the_id_after_the_resume_hint() {
    assert_eq!(
        redact_session_id(
            "εgo: To resume: 'ego resume 0198c7f4-9c1f-7a2b-8c3d-4e5f60718293'\nid 0198c7f4-9c1f-7a2b-8c3d-4e5f60718293\n"
        ),
        "εgo: To resume: 'ego resume <session>'\nid 0198c7f4-9c1f-7a2b-8c3d-4e5f60718293\n"
    );
}

#[test]
fn readable_keeps_answer_text_that_looks_like_a_prompt_echo() {
    assert_eq!(readable("hello\r\nA> A\r\n"), "hello\nA> A\n");
}

#[test]
fn readable_keeps_the_last_complete_trust_modal_frame() {
    let raw = concat!(
        "εgo: ego ",
        env!("CARGO_PKG_VERSION"),
        "\nProject files offered for trust:\n- <workspace>/AGENTS.md\nYes: load project instructions, configuration, skills, hooks and MCP server declarations.\nNo: continue without project files.\nTrust project files in <workspace>? [y/N] Project files offered for trust:\n- <workspace>/AGENTS.md\nYes: load project instructions, configuration, skills, hooks and MCP server declarations.\nNo: continue without project files.\nTrust project files in <workspace>? [y/N] yes\n"
    );
    assert_eq!(
        readable(raw),
        "εgo: ego <version>\nProject files offered for trust:\n- <workspace>/AGENTS.md\nYes: load project instructions, configuration, skills, hooks and MCP server declarations.\nNo: continue without project files.\nTrust project files in <workspace>? [y/N] yes\n"
    );
}

// Catches: the path-dependent wrap before [y/N] hides a modal repaint boundary.
#[test]
fn readable_keeps_the_last_wrapped_trust_modal_frame() {
    // Recorded after escape stripping and redaction from rb queue
    // 970eb349894e2cf3, master 67ed09908d, first run (also failed all 19 others).
    let recorded = include_str!("../goldens/launch/trust-wrapped-repaint.txt");
    // Historical terminal bytes retain their own historical wording oracle.
    let expected = include_str!("../goldens/launch/trust-historical-readable.txt");
    assert_eq!(readable(recorded), expected);
    let right_edge_repaint = recorded.replace("?\n[y/N] ", "? [y/N]");
    assert_eq!(readable(&right_edge_repaint), expected);
}

// Catches: a late PTY cursor reply changes a launch golden while actual
// startup prose stays the same. The echoed report was recorded in the Mac gate.
#[test]
fn readable_removes_echoed_cursor_reports_without_removing_prose() {
    assert_eq!(
        readable("a launch transcript\r\n^[[24;1R\r\nnext sentence\r\n"),
        "a launch transcript\nnext sentence\n"
    );
    assert_eq!(readable("^[[row;1R\r\n"), "^[[row;1R\n");
}

// Catches: joining a wrapped trust hint also swallows real answer rows.
#[test]
fn readable_joins_only_the_wrapped_trust_question() {
    for answer in ["yes", "no"] {
        assert_eq!(
            readable(&format!(
                "Trust project files in <workspace>?\r\n[y/N] {answer}\r\nanswer\r\n[y/N] is literal answer text\r\n"
            )),
            format!(
                "Trust project files in <workspace>? [y/N] {answer}\nanswer\n[y/N] is literal answer text\n"
            ),
        );
    }
    let changed = "Trust project files in <workspace>?\n[y/N] never\n";
    assert_eq!(
        readable(changed),
        "Trust project files in <workspace>? [y/N] never\n",
    );
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

// Catches: changing elapsed-time frames concatenate and leak into the golden.
#[test]
fn the_first_launch_keeps_its_wording_after_one_second_of_working() {
    let committed = std::fs::read_to_string(golden_path("nothing-to-trust"))
        .expect("the committed launch transcript");
    assert_eq!(
        transcript_with_reply(
            "nothing-to-trust",
            None,
            Reply::Paced {
                head: String::new(),
                tail: text("a launch transcript"),
                pause: std::time::Duration::from_millis(1100),
            },
        ),
        committed,
    );
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

// Catches: the build stamp of the startup line leaks into a committed golden, so every commit
// breaks the launch comparisons, or the redactor eats a model name that is not a stamp.
#[test]
fn readable_drops_the_build_stamp_and_keeps_other_tails() {
    assert_eq!(
        readable("εgo: ego 1.2.3 · 0123abcde · release\n"),
        "εgo: ego <version>\n"
    );
    assert_eq!(
        readable("ego 1.2.3 · unknown · debug · ollama/qwen3\n"),
        "ego <version> · ollama/qwen3\n"
    );
    assert_eq!(
        readable("ego 1.2.3 · ollama/qwen3 · debug\n"),
        "ego <version> · ollama/qwen3 · debug\n"
    );
}

// Catches: a version-only bump breaks launch golden comparisons.
#[test]
fn readable_redacts_version_bumps_only_in_banner_identity() {
    for version in [env!("CARGO_PKG_VERSION"), "9.42.17", "10.0.0-rc.2"] {
        for identity in [
            format!("ego {version} · ollama/qwen3"),
            format!("εgo: ego {version}"),
        ] {
            let raw = format!("{identity}\nanswer cites {version}\n");
            assert_eq!(
                readable(&raw),
                format!(
                    "{}\nanswer cites {version}\n",
                    identity.replace(version, "<version>")
                )
            );
        }
    }
}
