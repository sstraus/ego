//! RED contract for non-interactive regression case resume metadata grammar.

use ego_cli::cli::{Invocation, parse};

const SOME_ID: &str = "0192f3a1-1c2b-7d3e-9a4f-5b6c7d8e8c41";

fn argv(words: &[&str]) -> Vec<String> {
    std::iter::once("ego")
        .chain(words.iter().copied())
        .map(str::to_owned)
        .collect()
}

#[test]
fn resume_metadata_operations_are_non_interactive_for_every_output_format() {
    for operation in ["show", "set=Release candidate", "clear"] {
        for output in ["text", "json", "jsonl"] {
            let parsed = parse(argv(&[
                "resume", SOME_ID, "--name", operation, "--output", output,
            ]));
            assert!(parsed.is_ok());
            assert!(!matches!(parsed.unwrap(), Invocation::Interactive(_)));
        }
    }
    for operation in [
        "show",
        "set=Ship the product",
        "achieve",
        "block=API unavailable",
    ] {
        for output in ["text", "json", "jsonl"] {
            let parsed = parse(argv(&[
                "resume", SOME_ID, "--goal", operation, "--output", output,
            ]));
            assert!(parsed.is_ok());
            assert!(!matches!(parsed.unwrap(), Invocation::Interactive(_)));
        }
    }
}

#[test]
fn resume_metadata_rejects_prompt_answers_mixed_operations_and_malformed_values() {
    for words in [
        vec!["resume", SOME_ID, "prompt", "--name", "show"],
        vec!["resume", SOME_ID, "--answer", "yes", "--goal", "show"],
        vec!["resume", SOME_ID, "--name", "show", "--goal", "show"],
        vec!["resume", SOME_ID, "--name", "set="],
        vec!["resume", SOME_ID, "--goal", "set="],
        vec!["resume", SOME_ID, "--goal", "block="],
        vec!["resume", SOME_ID, "--name", "rename=old"],
        vec!["resume", SOME_ID, "--goal", "finish"],
    ] {
        assert!(
            parse(argv(&words)).is_err(),
            "accepted invalid metadata argv: {words:?}"
        );
    }
}
