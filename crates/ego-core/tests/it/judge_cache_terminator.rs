// Catches: option termination erases patterns or read paths from reusable opinion identity.
#[test]
fn option_terminators_keep_native_and_wrapper_commands_exact() {
    for words in [
        vec!["grep", "--", "-n", "README.md"],
        vec!["grep", "--", "-n", ".env"],
        vec!["echo", "--", "private"],
        vec!["bash", "-c", "grep one README.md; grep -- -n README.md"],
    ] {
        let argv: Vec<String> = words.into_iter().map(String::from).collect();
        assert_eq!(ego_core::permit::shell::cache_argv(&argv), argv);
    }
}
