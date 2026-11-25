//! Catches command names being submitted to the model after launch flags.
use crate::support;

use ego_cli::cli::{BUILT, Invocation, parse};
use support::{Ego, Places};

#[test]
fn global_flags_before_acp_serve_acp() {
    // A usage refusal is allowed; chat output is not an ACP response.
    let places = Places::new();
    let output = Ego::at(places.home_path(), places.workspace_path(), None)
        .conversing()
        .args(["--model", "openai-codex/gpt-6-luna", "acp"])
        .stdin("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":1,\"clientCapabilities\":{}}}\n")
        .run();
    assert_eq!(output.code, Some(2), "{}", output.stderr);
    assert!(output.stderr.contains("acp"), "{}", output.stderr);
    assert!(output.stdout.is_empty(), "{}", output.stdout);
}

#[test]
fn global_flags_before_each_subcommand_are_not_a_prompt() {
    // Catches an ACP-only fix that leaves other operator commands as prompts.
    for command in BUILT {
        for flags in [
            vec!["--model", "ollama/qwen3"],
            vec!["-q"],
            vec!["--model=ollama/qwen3", "--plain"],
        ] {
            let mut argv = vec!["ego".to_owned()];
            argv.extend(flags.into_iter().map(str::to_owned));
            argv.push((*command).to_owned());
            match parse(argv) {
                Err(error) => assert!(error.0.contains(command), "{command}: {error}"),
                Ok(invocation) => assert!(
                    !matches!(invocation, Invocation::Interactive(_)),
                    "{command} became a prompt: {invocation:?}"
                ),
            }
        }
        assert!(matches!(
            parse(vec!["ego".into(), (*command).into(), "--help".into()]).unwrap(),
            Invocation::Answered(_)
        ));
        assert!(matches!(
            parse(vec![
                "ego".into(),
                "-q".into(),
                "--".into(),
                (*command).into()
            ])
            .unwrap(),
            Invocation::Interactive(_)
        ));
        let error = parse(vec![
            "ego".into(),
            "-q".into(),
            (*command).into(),
            "--".into(),
        ])
        .unwrap_err();
        assert!(error.0.contains(command), "{command}: {error}");
    }
}

#[test]
fn command_names_in_values_and_escaped_prompts_remain_data() {
    // Catches a token scan stealing option values or explicit prompt text.
    for argv in [
        vec!["ego", "--system", "acp", "hello"],
        vec!["ego", "--system=acp", "hello"],
        vec!["ego", "--plain", "--", "acp"],
        vec!["ego", "--plain", "discuss acp"],
        vec!["ego", "--plain", "acp details"],
        vec!["ego", "--system", "acp"],
        vec!["ego", "--system=acp"],
    ] {
        assert!(matches!(
            parse(argv.into_iter().map(str::to_owned).collect()).unwrap(),
            Invocation::Interactive(_)
        ));
    }
}
