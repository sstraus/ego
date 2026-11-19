//! Safe paired native probes; never inspect a real credential store.

use super::*;

pub(super) async fn measure(
    backend: Arc<dyn Backend>,
    policy: &RunConfinement,
    scratch: &CanonPath,
    evidence: ProbeEvidence,
    cancel: CancelToken,
    limits: ExecLimits,
) -> bool {
    let Some(perimeter) = policy.adaptive_perimeter() else {
        return record_control_failure(
            &evidence,
            "adaptive.setup".into(),
            policy.workspace().as_path(),
            "missing admitted Adaptive perimeter".into(),
        );
    };
    // A workspace's parent need not be host-writable. Captured HOME offers
    // another location, but every candidate must preserve the negative probe.
    let mut causes = Vec::new();
    let outside = policy
        .workspace()
        .as_path()
        .parent()
        .into_iter()
        .chain([perimeter.home.as_path()])
        .find_map(|parent| {
            let directory = tempfile::Builder::new()
                .prefix(".ego-adaptive-probe-")
                .tempdir_in(parent);
            let directory = match directory {
                Ok(directory) => directory,
                Err(error) => {
                    causes.push(format!(
                        "create outside fixture in {}: {error}",
                        parent.display()
                    ));
                    return None;
                }
            };
            let path = match fs::canonicalize(directory.path()) {
                Ok(path) => path,
                Err(error) => {
                    causes.push(format!(
                        "canonicalize outside fixture {}: {error}",
                        directory.path().display()
                    ));
                    return None;
                }
            };
            let covered = path.starts_with(policy.workspace().as_path())
                || path.starts_with(scratch.as_path())
                || perimeter
                    .writable
                    .iter()
                    .chain(&perimeter.credentials)
                    .any(|root| path.starts_with(root.as_path()))
                || policy
                    .roots()
                    .iter()
                    .any(|root| path.starts_with(root.path().as_path()));
            if covered {
                causes.push(format!(
                    "outside fixture {} is covered by the admitted perimeter",
                    path.display()
                ));
                None
            } else {
                Some((directory, path))
            }
        });
    let Some((_outside, outside_path)) = outside else {
        return record_control_failure(
            &evidence,
            "adaptive.setup".into(),
            policy.workspace().as_path(),
            causes.join("; "),
        );
    };
    let inside = match tempfile::Builder::new()
        .prefix(".ego-adaptive-probe-")
        .tempdir_in(policy.workspace().as_path())
    {
        Ok(dir) => dir,
        Err(error) => {
            return setup_failure(
                &evidence,
                "create workspace fixture",
                policy.workspace().as_path(),
                error,
            );
        }
    };
    let protected = scratch.as_path().join("credential-probe");
    if let Err(error) = fs::create_dir(&protected) {
        return setup_failure(&evidence, "create credential fixture", &protected, error);
    }
    let readable = outside_path.join("instruction.txt");
    let secret = protected.join("synthetic.txt");
    let outside_env = scratch.as_path().join(".env.ego-probe");
    for (path, bytes) in [
        (&outside_env, b"synthetic".as_slice()),
        (&readable, b"instruction".as_slice()),
        (&secret, b"synthetic".as_slice()),
    ] {
        if let Err(error) = fs::write(path, bytes) {
            return setup_failure(&evidence, "write probe fixture", path, error);
        }
    }
    for (name, path, allowed) in [
        ("adaptive.read.outside", readable, true),
        ("adaptive.read.credential-floor", secret, false),
        ("adaptive.read.env-floor", outside_env, false),
    ] {
        let suite = ProbeSuite {
            name,
            cwd: policy.workspace(),
            root: &path,
        };
        let argv = if name == "adaptive.read.env-floor" {
            // Exercise program-internal access, so the higher-level credential
            // argv guard cannot substitute for the OS guarantee being measured.
            vec![
                "/bin/sh".into(),
                "-c".into(),
                "p=$1/.env.ego-probe; exec /bin/cat \"$p\"".into(),
                "ego-probe".into(),
                scratch.as_path().as_os_str().to_owned(),
            ]
        } else {
            vec!["/bin/cat".into(), path.clone().into_os_string()]
        };
        let control = suite.control_name();
        if !probe(
            Arc::new(super::super::null::NullBackend),
            evidence.clone(),
            cancel.clone(),
            limits,
            &suite.renamed(&control),
            argv.clone(),
        )
        .await
        {
            return false;
        }
        let outcome = probe_result(
            backend.clone(),
            evidence.clone(),
            cancel.clone(),
            limits,
            &suite,
            argv,
        )
        .await;
        if allowed {
            if outcome.is_err() {
                return false;
            }
        } else if !matches!(outcome, Err(reason) if reason.starts_with("exit ")) {
            return false;
        }
    }
    for (name, root, allowed) in [
        ("adaptive.write.workspace", inside.path(), true),
        ("adaptive.write.scratch", scratch.as_path(), true),
        ("adaptive.write.outside", outside_path.as_path(), false),
    ] {
        let target = root.join("probe-created");
        let suite = ProbeSuite {
            name,
            cwd: policy.workspace(),
            root,
        };
        let argv = vec!["/usr/bin/touch".into(), target.clone().into_os_string()];
        let control = suite.control_name();
        if !probe(
            Arc::new(super::super::null::NullBackend),
            evidence.clone(),
            cancel.clone(),
            limits,
            &suite.renamed(&control),
            argv.clone(),
        )
        .await
        {
            return false;
        }
        if let Err(error) = fs::remove_file(&target) {
            return record_control_failure(
                &evidence,
                control,
                &target,
                format!("remove control fixture: {error}"),
            );
        }
        let outcome = probe_result(
            backend.clone(),
            evidence.clone(),
            cancel.clone(),
            limits,
            &suite,
            argv,
        )
        .await;
        if allowed {
            if outcome.is_err() || !target.exists() {
                return false;
            }
        } else if !matches!(outcome, Err(reason) if reason.starts_with("exit ")) || target.exists()
        {
            return false;
        }
    }
    true
}

fn setup_failure(
    evidence: &ProbeEvidence,
    operation: &str,
    path: &Path,
    error: std::io::Error,
) -> bool {
    record_control_failure(
        evidence,
        "adaptive.setup".into(),
        path,
        format!("{operation}: {error}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ego_core::permit::AdaptivePerimeter;

    // Catches: failed outside fixture setup refuses Adaptive without a cause record.
    #[test]
    fn adaptive_unwritable_location_records_setup_cause() {
        let root = tempfile::tempdir().unwrap();
        let blocker = root.path().join("not-a-directory");
        fs::write(&blocker, "fixture").unwrap();
        let workspace = CanonPath::new(blocker.join("workspace"));
        let policy = RunConfinement::new(Confinement::Adaptive, false, workspace)
            .unwrap()
            .with_adaptive_perimeter(AdaptivePerimeter {
                home: CanonPath::new(&blocker),
                writable: vec![],
                credentials: vec![],
                controls: vec![],
            });
        let evidence = probe_evidence().unwrap();
        let result = futures::executor::block_on(measure(
            Arc::new(super::super::super::null::NullBackend),
            &policy,
            &CanonPath::new(root.path().join("scratch")),
            evidence.clone(),
            CancelToken::new(),
            ExecLimits::default(),
        ));
        assert!(!result);
        let records = evidence.log.snapshot();
        assert_eq!(records.len(), 1, "setup failure had no transcript");
        assert_eq!(records[0].suite, "adaptive.setup");
        assert!(
            records[0].outcome.contains("directory"),
            "{}",
            records[0].outcome
        );
        assert!(
            evidence
                .log
                .host_control_failure()
                .unwrap()
                .contains("adaptive.setup")
        );
    }
    // Catches: workspace, credential-directory or fixture-write failures vanish after outside setup.
    #[test]
    fn adaptive_fixture_failures_keep_operation_and_os_cause() {
        for stage in ["workspace", "credential", "write"] {
            let root = tempfile::tempdir().unwrap();
            let workspace = root.path().join("workspace");
            let scratch = root.path().join("scratch");
            if stage == "workspace" {
                fs::write(&workspace, "blocker").unwrap();
            } else {
                fs::create_dir(&workspace).unwrap();
            }
            fs::create_dir(&scratch).unwrap();
            if stage == "credential" {
                fs::write(scratch.join("credential-probe"), "blocker").unwrap();
            }
            if stage == "write" {
                fs::create_dir(scratch.join(".env.ego-probe")).unwrap();
            }
            let policy =
                RunConfinement::new(Confinement::Adaptive, false, CanonPath::new(&workspace))
                    .unwrap()
                    .with_adaptive_perimeter(AdaptivePerimeter {
                        home: CanonPath::new(root.path()),
                        writable: vec![],
                        credentials: vec![],
                        controls: vec![],
                    });
            let evidence = probe_evidence().unwrap();
            assert!(!futures::executor::block_on(measure(
                Arc::new(super::super::super::null::NullBackend),
                &policy,
                &CanonPath::new(&scratch),
                evidence.clone(),
                CancelToken::new(),
                ExecLimits::default(),
            )));
            let cause = evidence
                .log
                .host_control_failure()
                .expect("cause reaches refusal");
            let operation = match stage {
                "workspace" => "create workspace fixture",
                "credential" => "create credential fixture",
                _ => "write probe fixture",
            };
            assert!(cause.contains(operation), "{cause}");
            assert!(cause.contains("os error"), "{cause}");
        }
    }
}
