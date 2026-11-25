use ego_cli::cli::{Invocation, parse};

// Catches: the root overview omits the run options accepted without a subcommand.
#[test]
fn root_help_lists_model_mode_sandbox_and_workspace_options() {
    for flag in ["--help", "-h"] {
        let Invocation::Answered(help) = parse(vec!["ego".into(), flag.into()]).unwrap() else {
            panic!("help did not answer");
        };
        for option in ["--model", "--mode", "--sandbox", "-C <DIR>", "--thinking"] {
            assert!(help.contains(option), "missing {option}: {help}");
        }
        assert!(help.contains("resume"));
    }
}
