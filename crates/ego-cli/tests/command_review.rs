//! Parser-boundary regressions from the command review; no external effects.

use ego_cli::cli::{self, Invocation};

const SESSION: &str = "0192f3a1-1c2b-7d3e-9a4f-5b6c7d8e8c41";

fn parse(words: &[&str]) -> Result<Invocation, cli::UsageError> {
    cli::parse(
        std::iter::once("ego")
            .chain(words.iter().copied())
            .map(str::to_owned)
            .collect(),
    )
}

// Catches: a missing option value is diagnosed as a supplied retired flag.
#[test]
fn option_value_is_not_a_diagnostic_flag() {
    for prefix in [vec!["run"], vec!["resume", SESSION], vec![]] {
        let mut words = prefix;
        words.extend(["--system", "--verbose"]);
        let error = parse(&words).unwrap_err().to_string();
        assert!(error.contains("--system"), "{error}");
        assert!(!error.contains("waiting on"), "{error}");
    }
    let Invocation::Run(args) = parse(&["run", "--system=--verbose", "task"]).unwrap() else {
        panic!("system value must not become a diagnostic flag");
    };
    assert_eq!(args.system.as_deref(), Some("--verbose"));
    for flag in ["-v", "--verbose"] {
        let error = parse(&["run", flag, "task"]).unwrap_err().to_string();
        assert!(
            error.contains(flag) && error.contains("waiting on"),
            "{error}"
        );
    }
}

// Catches: an option-like value sends resume through the metadata-only parser.
#[test]
fn option_value_equal_to_goal_is_not_metadata() {
    let error = parse(&["resume", SESSION, "--system", "--goal"])
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("a value is required for '--system"),
        "{error}"
    );
    let error = parse(&["resume", SESSION, "--name", "--goal"])
        .unwrap_err()
        .to_string();
    assert!(error.contains("a value is required for '--name"), "{error}");
}
