//! Compact view of a process output tail for people and models.
//!
//! Build tools redraw one progress line with carriage returns and print one
//! line per compiled unit. Repeating that in a status answer buries the
//! result. [`compact`] applies terminal semantics (a carriage return
//! overwrites the line), folds each run of progress lines into its last line
//! and a count. Long output that remains after folding is shown as an explicit
//! bounded tail excerpt. The complete bytes stay in the process evidence; this
//! only changes the text shown.

/// Words that open a pure progress line of cargo, nextest and similar tools.
const PROGRESS_WORDS: &[&str] = &[
    "Compiling",
    "Downloading",
    "Downloaded",
    "Fresh",
    "Checking",
    "Blocking",
    "Updating",
    "Locking",
    "Fetching",
    "Building [",
];

fn is_progress(line: &str) -> bool {
    let line = line.trim_start();
    PROGRESS_WORDS.iter().any(|word| line.starts_with(word))
}

/// The tail as shown: carriage-return redraws resolved, progress folded.
pub fn compact(tail: &[u8]) -> String {
    let text = String::from_utf8_lossy(tail);
    let mut shown: Vec<String> = Vec::new();
    let mut folded = 0usize;
    // A run of progress lines ends with a note that says how many were folded.
    let end_run = |shown: &mut Vec<String>, folded: &mut usize| {
        if *folded > 0 {
            shown.push(format!(
                "[{folded} earlier progress lines folded; the full output is recoverable]"
            ));
            *folded = 0;
        }
    };
    for raw in text.split('\n') {
        // A carriage return returns to the start of the line: a terminal
        // shows only the last non-empty redraw.
        let redraws: Vec<&str> = raw
            .split('\r')
            .filter(|part| !part.trim().is_empty())
            .collect();
        let Some(last) = redraws.last() else {
            end_run(&mut shown, &mut folded);
            shown.push(String::new());
            continue;
        };
        let overwritten = redraws.len() - 1;
        if is_progress(last) {
            if shown.last().is_some_and(|line| is_progress(line)) {
                shown.pop();
                folded += 1;
            }
            folded += overwritten;
            shown.push(last.trim_end().to_owned());
        } else {
            end_run(&mut shown, &mut folded);
            shown.push(last.trim_end().to_owned());
        }
    }
    end_run(&mut shown, &mut folded);
    let shown = shown.join("\n");
    const EXCERPT_BYTES: usize = 4096;
    if shown.len() <= EXCERPT_BYTES {
        return shown;
    }
    let mut start = shown.len() - EXCERPT_BYTES;
    while !shown.is_char_boundary(start) {
        start += 1;
    }
    format!(
        "[bounded output excerpt; earlier output omitted]\n{}",
        &shown[start..]
    )
}

#[cfg(test)]
mod tests {
    use super::compact;

    const REAL_CARGO_STREAM: &[u8] = include_bytes!("../tests/fixtures/cargo-build-progress.txt");

    // Catches: a status answer forwards every carriage-return progress redraw of a real build.
    #[test]
    fn a_real_cargo_progress_stream_becomes_one_row_and_keeps_the_result() {
        let shown = compact(REAL_CARGO_STREAM);
        assert!(shown.len() < REAL_CARGO_STREAM.len() / 2, "{shown}");
        // A terminal shows the last redraw of each line: the final row is
        // overwritten by `Finished`, so no raw progress row remains.
        assert!(shown.matches("Building [").count() <= 1, "{shown}");
        assert!(shown.contains("progress lines folded"), "{shown}");
        assert!(shown.contains("Finished `dev` profile"), "{shown}");
        assert!(shown.contains("warning: unused variable"), "{shown}");
        assert!(shown.contains("mbx[cache]: 714 hits"), "{shown}");
    }

    // Catches: folding progress swallows a diagnostic that sits between progress lines.
    #[test]
    fn diagnostics_between_progress_lines_are_kept() {
        let tail = b"   Compiling a\n   Compiling b\nerror[E0425]: cannot find value `x`\n --> src/lib.rs:1:1\n   Compiling c\ntest result: FAILED. 1 passed; 1 failed\n";
        let shown = compact(tail);
        assert!(shown.contains("error[E0425]"));
        assert!(shown.contains(" --> src/lib.rs:1:1"));
        assert!(shown.contains("test result: FAILED. 1 passed; 1 failed"));
        assert!(shown.contains("Compiling c"));
    }

    // Catches: an unrecognized noisy build is returned without a bounded fallback notice.
    #[test]
    fn unknown_noisy_output_has_a_bounded_explicit_excerpt() {
        let input = "unrecognized build output\n".repeat(2000);
        let shown = compact(input.as_bytes());
        assert!(shown.len() < 8192, "{} bytes", shown.len());
        assert!(shown.contains("excerpt"), "{shown}");
        assert!(shown.contains("unrecognized build output"));
    }

    #[test]
    fn unknown_output_is_unchanged() {
        assert_eq!(compact(b"hello\nworld"), "hello\nworld");
    }
}
