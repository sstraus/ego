use super::*;

fn script(run: &Run, body: &str) -> SpawnSpec {
    let file = run.workspace.as_path().join("internal.sh");
    std::fs::write(&file, body).expect("workspace script");
    spec(run, &["/bin/sh", file.to_str().unwrap()])
}

// Catches: a program-internal read is discarded because argv names only its script.
#[test]
fn internal_read_refusal_reaches_the_human_without_an_argv_path() {
    let workspace = tempfile::tempdir().unwrap();
    let (_keep, target, printed) = outside_file("internal read\n");
    let seat = Seat::answering(false);
    let run = run_with(&seat, workspace.path());
    let child = script(&run, &format!("exec /bin/cat '{printed}'\n"));
    assert!(!child.argv().iter().any(|arg| arg == target.as_path()));
    assert!(matches!(
        execute(&run, &child),
        Err(ProcessError::Exit { .. })
    ));
    assert_eq!(
        seat.asked(),
        vec![SandboxGrant::new(target, SandboxAccess::Read)]
    );
}

// Catches: an internal cache create never asks the human because argv lacks the cache path.
#[test]
fn internal_write_refusal_reaches_the_human_without_an_argv_path() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let target = std::fs::canonicalize(outside.path())
        .unwrap()
        .join("cache-entry");
    let seat = Seat::answering(false);
    let run = run_with(&seat, workspace.path());
    let child = script(
        &run,
        &format!("exec /usr/bin/touch '{}'\n", target.display()),
    );
    assert!(!child.argv().iter().any(|arg| arg == target.as_os_str()));
    assert!(matches!(
        execute(&run, &child),
        Err(ProcessError::Exit { .. })
    ));
    assert!(!target.exists());
    assert_eq!(
        seat.asked(),
        vec![SandboxGrant::new(
            CanonPath::new(target),
            SandboxAccess::Write
        )]
    );
}

// Catches: a PATH-resolved binary outside exec_paths is unanchored and loses its exec question.
#[test]
fn path_resolved_exec_refusal_reaches_the_human_as_exec() {
    use ego_core::credentials::SecretMaterial;
    use ego_core::permit::{EnvName, ExplicitEnvBinding};
    let workspace = tempfile::tempdir().unwrap();
    let (_keep, target, _) = outside_program();
    let seat = Seat::answering(false);
    let run = run_with(&seat, workspace.path());
    let child = SpawnSpec::new(
        vec![target.as_path().file_name().unwrap().into()],
        Some(run.workspace.clone()),
        None,
        EnvPolicy::exact(vec![ExplicitEnvBinding::new(
            EnvName::new("PATH").unwrap(),
            SecretMaterial::new(
                target
                    .as_path()
                    .parent()
                    .unwrap()
                    .as_os_str()
                    .as_encoded_bytes()
                    .to_vec(),
            ),
        )])
        .unwrap(),
    )
    .unwrap();
    let child = run.ports.process().observe(&child).unwrap();
    assert!(matches!(
        execute(&run, &child),
        Err(ProcessError::Exit { .. })
    ));
    assert_eq!(
        seat.asked(),
        vec![SandboxGrant::new(target, SandboxAccess::Exec)]
    );
}
