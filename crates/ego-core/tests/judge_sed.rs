// Catches: trailing sed script options are treated as harmless input files.
#[test]
fn sed_print_proof_rejects_every_extra_script_option() {
    for extra in ["-e 'w changed.txt'", "-f script.sed", "--expression='w changed.txt'", "--file=script.sed"] {
        let argv = vec!["bash".into(), "-c".into(), format!("sed -n '1p' README.md {extra}")];
        assert!(!ego_core::permit::shell::observe(&argv).read_only, "{extra}");
        assert_eq!(ego_core::permit::shell::cache_argv(&argv), argv, "{extra}");
    }
    assert!(ego_core::permit::shell::observe(&["bash".into(), "-c".into(), "sed -n '1,20p' README.md other.txt".into()]).read_only);
}
