//! contract for hardened status Git command grammar.

use ego_cli::repl::status::GitCommand;

#[test]
fn hardened_git_refresh_cannot_read_user_config_or_launch_a_pager() {
    let command = GitCommand::status("/workspace/ego");
    assert!(command.arguments().contains(&"--no-pager".to_string()));
    assert!(command.arguments().windows(2).any(|pair| {
        pair.iter()
            .map(String::as_str)
            .eq(["-c", "core.hooksPath=/dev/null"])
    }));
    assert!(command.arguments().windows(2).any(|pair| {
        pair.iter()
            .map(String::as_str)
            .eq(["-c", "core.fsmonitor=false"])
    }));
    assert_eq!(command.env("GIT_OPTIONAL_LOCKS"), Some("0"));
    assert_eq!(command.env("GIT_CONFIG_NOSYSTEM"), Some("1"));
    assert_eq!(command.env("GIT_CONFIG_GLOBAL"), Some("/dev/null"));
    assert!(!command.uses_shell());
}
