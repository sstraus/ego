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
        return false;
    };
    let Some(parent) = policy.workspace().as_path().parent() else {
        return false;
    };
    let Ok(outside) = tempfile::Builder::new()
        .prefix(".ego-adaptive-probe-")
        .tempdir_in(parent)
    else {
        return false;
    };
    let Ok(outside_path) = fs::canonicalize(outside.path()) else {
        return false;
    };
    if perimeter
        .writable
        .iter()
        .any(|root| outside_path.starts_with(root.as_path()))
        || policy
            .roots()
            .iter()
            .any(|root| outside_path.starts_with(root.path().as_path()))
    {
        return false;
    }
    let inside = match tempfile::Builder::new()
        .prefix(".ego-adaptive-probe-")
        .tempdir_in(policy.workspace().as_path())
    {
        Ok(dir) => dir,
        Err(_) => return false,
    };
    let protected = scratch.as_path().join("credential-probe");
    if fs::create_dir(&protected).is_err() {
        return false;
    }
    let readable = outside_path.join("instruction.txt");
    let secret = protected.join("synthetic.txt");
    let outside_env = scratch.as_path().join(".env.ego-probe");
    if fs::write(&outside_env, b"synthetic").is_err() {
        return false;
    }
    if fs::write(&readable, b"instruction").is_err() || fs::write(&secret, b"synthetic").is_err() {
        return false;
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
            || fs::remove_file(&target).is_err()
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
