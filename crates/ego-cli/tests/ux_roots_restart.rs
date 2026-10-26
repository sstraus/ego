mod support;
use support::{Plan, Stub};

// Catches: a saved root silently leaves the running process on its old perimeter.
#[test]
fn roots_mutations_tell_the_user_to_exit_and_resume() {
    let stub = Stub::start(Plan::default());
    let places = support::Places::new();
    let path = places.workspace_path().display().to_string();
    for args in [
        vec!["add", path.as_str()],
        vec!["add", path.as_str()],
        vec!["rm", path.as_str()],
    ] {
        let ego = places.ego(&stub).subcommand("roots");
        let result = args.into_iter().fold(ego, |ego, arg| ego.arg(arg)).run();
        assert_eq!(result.code, Some(0), "{}", result.stderr);
        assert!(
            result
                .stderr
                .contains("Saved roots apply when a session starts or resumes"),
            "{}",
            result.stderr
        );
        assert!(
            result
                .stderr
                .contains("running sessions keep their current file access"),
            "{}",
            result.stderr
        );
    }
}
