use ego_core::permit::shell::observe;

// Catches: recognizing just -su misses other cluster prefixes, operands, and headers.
#[test]
fn curl_clusters_hide_the_first_value_operand_and_preserve_preceding_flags() {
    for (options, flags) in [
        ("-fsSLualice:private-value", "-fsSLu"),
        ("-fsSLu alice:private-value", "-fsSLu"),
        ("-sHAuthorization:private-value", "-sH"),
        ("-sH Authorization:private-value", "-sH"),
        ("-sXprivate-value", "-sX"),
        ("-sX private-value", "-sX"),
    ] {
        let native = std::iter::once("curl")
            .chain(options.split_whitespace())
            .chain(std::iter::once("https://api.example.com"))
            .map(String::from)
            .collect::<Vec<_>>();
        for argv in [
            native,
            vec![
                "bash".into(),
                "-c".into(),
                format!("curl {options} https://api.example.com"),
            ],
        ] {
            let skeleton = observe(&argv).skeleton;
            assert!(!skeleton.contains("private-value"), "{skeleton}");
            assert!(skeleton.contains(flags), "{skeleton}");
            assert!(skeleton.contains("api.example.com"), "{skeleton}");
        }
    }
}

// Catches: applying curl's -r/-F operand rules to unrelated commands hides dangerous flags.
#[test]
fn curl_operand_rules_do_not_erase_other_commands_flags() {
    for (command, flags) in [
        ("rm -rf /ws", "-rf"),
        ("rm -drf /ws", "-drf"),
        ("grep -F pattern /ws", "-F"),
    ] {
        let argv = command
            .split_whitespace()
            .map(String::from)
            .collect::<Vec<_>>();
        for argv in [argv, vec!["bash".into(), "-c".into(), command.into()]] {
            let skeleton = observe(&argv).skeleton;
            assert!(skeleton.contains(flags), "{skeleton}");
            assert!(skeleton.contains("/ws"), "target lost: {skeleton}");
        }
    }
}
