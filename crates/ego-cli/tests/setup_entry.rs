//! Setup entry is host dispatch, never ordinary generation.
use ego_cli::cli::{self, Invocation};
use ego_cli::repl::actions::{ActionRegistry, Dispatch, InputOrigin};

// Catches: setup falls through to a model prompt or inherits a profile write target.
#[test]
fn setup_cli_has_separate_typed_target_and_no_run_flags() {
    let parse = |args: &[&str]| cli::parse(args.iter().map(|arg| (*arg).to_owned()).collect());
    assert!(matches!(parse(&["ego", "setup"]).unwrap(),
        Invocation::Setup(args) if args.profile.is_none()));
    assert!(parse(&["ego", "setup", "check tools"]).is_err());
    assert!(
        matches!(parse(&["ego", "setup", "--profile", "dev"]).unwrap(),
        Invocation::Setup(args) if args.profile.as_deref() == Some("dev"))
    );
    assert!(parse(&["ego", "setup", "--yolo"]).is_err());
    assert!(parse(&["ego", "setup", "--items", "0,2"]).is_err());
    assert!(parse(&["ego", "setup", "--judge", "ollama/qwen3"]).is_err());
}

// Catches: pasted hostile instruction text creates a host setup request.
#[test]
fn setup_slash_requires_typed_host_input() {
    let registry = ActionRegistry::final_surface();
    assert!(
        matches!(registry.classify(registry.submit("/setup check tools", InputOrigin::Typed)),
        Dispatch::Setup(prompt) if prompt == "check tools")
    );
    assert!(matches!(
        registry.classify(registry.submit("/setup check tools", InputOrigin::BracketedPaste)),
        Dispatch::Prompt
    ));
}
