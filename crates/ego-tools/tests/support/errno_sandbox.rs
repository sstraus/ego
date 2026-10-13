use super::*;

// Catches: errno-suffixed EPERM/EACCES lose the human request, or stdout/trailing prose raises one.
#[test]
fn adaptive_errno_refusals_request_only_absolute_stderr_paths() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let target = std::fs::canonicalize(outside.path())
        .unwrap()
        .join("output");
    let printed = target.to_str().unwrap();
    for error in [
        std::io::Error::from_raw_os_error(1).to_string(),
        std::io::Error::from_raw_os_error(13).to_string(),
        "Operation not permitted".to_owned(),
        "Permission denied".to_owned(),
        std::io::Error::from_raw_os_error(2).to_string(),
    ] {
        for (stream, suffix) in [("2", ""), ("1", ""), ("2", " ignored")] {
            let seat = Seat::answering(false);
            let run = run_with(&seat, workspace.path());
            let script =
                format!("printf '%s: %s%s\\n' '{printed}' \"$1\" \"$2\" >&{stream}; exit 1");
            let argv = vec!["/bin/sh", "-c", &script, "probe", &error, suffix];
            let child = SpawnSpec::new(
                argv.into_iter().map(Into::into).collect(),
                Some(run.workspace.clone()),
                None,
                EnvPolicy::Inherited,
            )
            .unwrap();
            let Err(ProcessError::Exit { result, .. }) = execute(&run, &child) else {
                panic!("fixture must fail")
            };
            let expected = stream == "2" && suffix.is_empty() && !error.contains("os error 2");
            assert_eq!(
                seat.asked(),
                if expected {
                    vec![SandboxGrant::new(
                        CanonPath::new(&target),
                        SandboxAccess::Write,
                    )]
                } else {
                    vec![]
                },
                "{error} {stream} {suffix}"
            );
            if expected {
                assert!(
                    text(&result).contains("the user declined"),
                    "{}",
                    text(&result)
                );
            }
        }
    }
}
