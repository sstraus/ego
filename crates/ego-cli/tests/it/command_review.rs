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

// Catches: the two aliases assign different meanings to the same goal arguments.
#[test]
fn resume_and_continue_share_goal_grammar() {
    for goal in ["show", "Ship the release", "set=This is an objective"] {
        let resume = parse(&["resume", SESSION, "--goal", goal, "continue work"]).unwrap();
        let continued = parse(&["continue", SESSION, "--goal", goal, "continue work"]).unwrap();
        assert_eq!(resume, continued);
    }
    for operation in ["show", "set=Ship", "achieve", "block=API unavailable"] {
        let resume = parse(&["resume", SESSION, "--goal-operation", operation]).unwrap();
        assert!(matches!(resume, Invocation::SessionMetadata(_)));
        assert_eq!(
            resume,
            parse(&["continue", SESSION, "--goal-operation", operation]).unwrap()
        );
    }
}

// Catches: resume steals a launch objective for the metadata-only operation parser.
#[test]
fn resumed_launch_goal_is_not_metadata() {
    for alias in ["resume", "continue"] {
        let Invocation::Resume(args) =
            parse(&[alias, SESSION, "--goal", "Ship the release", "do the work"]).unwrap()
        else {
            panic!("a prompted resume must start the resumed run");
        };
        assert_eq!(args.goal.as_deref(), Some("Ship the release"));
        assert_eq!(args.prompt.as_deref(), Some("do the work"));
        for tail in [
            vec!["task"],
            vec!["--model", "ollama/qwen3"],
            vec!["--goal", "launch objective"],
        ] {
            let mut words = vec![alias, SESSION, "--goal-operation", "show"];
            words.extend(tail);
            assert!(
                parse(&words).is_err(),
                "metadata accepted run input: {words:?}"
            );
        }
        let Invocation::Answered(help) = parse(&[alias, "--help"]).unwrap() else {
            panic!("help")
        };
        assert!(
            help.contains("--goal-operation") && help.contains("--goal <GOAL>"),
            "{help}"
        );
    }
    for alias in ["resume", "continue"] {
        let command = cli::root_command()
            .get_subcommands()
            .find(|command| command.get_name() == alias)
            .unwrap()
            .clone();
        assert!(
            command
                .get_arguments()
                .any(|argument| argument.get_long() == Some("goal-operation"))
        );
        assert!(
            command
                .get_arguments()
                .any(|argument| argument.get_long() == Some("goal"))
        );
    }
}

// Catches: retired diagnostic flags still appear to configure a saved setting.
#[test]
fn setup_help_names_only_configuration_inputs() {
    let Invocation::Setup(args) = parse(&["setup", "--profile", "docs"]).unwrap() else {
        panic!("setup")
    };
    assert_eq!(args.profile.as_deref(), Some("docs"));
    for flag in ["--items", "--judge"] {
        assert!(parse(&["setup", flag, "0"]).is_err());
    }
    let Invocation::Answered(help) = parse(&["setup", "--help"]).unwrap() else {
        panic!("help")
    };
    assert!(
        help.contains("--profile") && help.contains("--directory"),
        "{help}"
    );
    assert!(
        !help.contains("--items") && !help.contains("--judge"),
        "{help}"
    );
}

fn help(words: &[&str]) -> String {
    let Invocation::Answered(help) = parse(words).unwrap() else {
        panic!("help must answer without effects")
    };
    help
}

// Catches: resume and continue advertise session-creation flags they always refuse.
#[test]
fn resume_help_omits_refused_flags() {
    for alias in ["resume", "continue"] {
        let rendered = help(&[alias, "--help"]);
        for flag in ["--session", "--session-id", "--worktree", "--ephemeral"] {
            assert!(
                !rendered.split_whitespace().any(|word| word == flag),
                "{rendered}"
            );
            assert!(parse(&[alias, SESSION, flag, "value"]).is_err());
        }
        for marker in ["picker", "latest", "non-UUID", "prompt-only", "--output"] {
            assert!(rendered.contains(marker), "missing {marker}: {rendered}");
        }
    }
}

// Catches: sharing Flags causes headless run to advertise interactive-only Vim.
#[test]
fn run_help_omits_vim() {
    assert!(!help(&["run", "--help"]).contains("--vim"));
    assert!(help(&["--help"]).contains("--vim"));
    let Invocation::Interactive(args) = parse(&["--vim"]).unwrap() else {
        panic!("interactive Vim")
    };
    assert!(args.vim);
    assert!(
        parse(&["run", "--vim", "task"])
            .unwrap_err()
            .to_string()
            .contains("interactive")
    );
}

// Catches: prompt conflicts are enforced only after clap, invisible to generated discovery.
#[test]
fn prompt_shaping_constraints_are_owned_by_clap_and_reference() {
    for words in [
        vec!["run", "--bare", "--instructions", "missing.md"],
        vec!["acp", "--bare", "--instructions", "missing.md"],
        vec!["run", "--file", "missing.md", "task"],
        vec!["run", "--var", "key=value", "task"],
        vec!["run", "--var", "key=value"],
        vec!["--var", "key=value", "task"],
        vec!["resume", SESSION, "--var", "key=value", "task"],
    ] {
        let error = parse(&words).unwrap_err().to_string();
        assert!(
            error.contains("Usage:"),
            "clap must own structural conflicts: {error}"
        );
        assert!(
            !error.contains("could not") && !error.contains("No such file"),
            "must refuse before loading: {error}"
        );
    }
    let rendered = ego_cli::reference::reference();
    let run = rendered
        .split("## `ego run`\n")
        .nth(1)
        .unwrap()
        .split("\n## ")
        .next()
        .unwrap();
    for (argument, rule) in [
        ("--bare", "--instructions"),
        ("-f, --file", "<PROMPT>"),
        ("--var", "Requires --file"),
    ] {
        let line = run
            .lines()
            .find(|line| line.starts_with(&format!("- `{argument}")))
            .unwrap();
        assert!(line.contains(rule), "{line}");
    }
}

// Catches: CLI.md omits writable keys or assigns them to the wrong user scope.
#[test]
fn managed_config_documentation_matches_the_closed_key_scopes() {
    use ego_core::config::{ManagedConfigKey, ProfileName, UserConfigTarget};
    use std::collections::{BTreeMap, BTreeSet};
    let targets = [
        UserConfigTarget::Base,
        UserConfigTarget::Profile(ProfileName::parse("ordinary").unwrap()),
        UserConfigTarget::Profile(ProfileName::parse("ci").unwrap()),
    ];
    let mut keys = BTreeSet::new();
    for target in &targets {
        keys.extend(ManagedConfigKey::for_target(target.clone()).iter().cloned());
    }
    keys.extend([
        ManagedConfigKey::AuthHeaders("PROVIDER".into()),
        ManagedConfigKey::AuthCredentialCommand("PROVIDER".into()),
    ]);
    let expected = keys
        .into_iter()
        .map(|key| {
            (
                key.as_str().into_owned(),
                targets.each_ref().map(|target| key.available_in(target)),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let document = include_str!("../../../../CLI.md");
    let table = document
        .split("<!-- ego-managed-config-keys:start -->")
        .nth(1)
        .unwrap()
        .split("<!-- ego-managed-config-keys:end -->")
        .next()
        .unwrap();
    let mut actual = BTreeMap::new();
    for line in table.lines().filter(|line| line.starts_with("| `")) {
        let cells = line.split('|').map(str::trim).collect::<Vec<_>>();
        let key = cells[1].trim_matches('`').to_owned();
        let scopes = [cells[2], cells[3], cells[4]].map(|cell| match cell {
            "yes" => true,
            "no" => false,
            other => panic!("invalid scope {other}"),
        });
        assert!(
            actual.insert(key, scopes).is_none(),
            "duplicate key: {line}"
        );
    }
    assert_eq!(actual, expected);
}

// Catches: help claims missing commands when every command is already implemented.
#[test]
fn root_help_does_not_invent_unavailable_commands() {
    if cli::UNBUILT.is_empty() {
        let rendered = help(&["--help"]);
        assert!(!rendered.contains("remaining commands"), "{rendered}");
        assert!(!rendered.contains("waits on"), "{rendered}");
    }
}
