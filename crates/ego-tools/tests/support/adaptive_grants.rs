use super::*;
use ego_core::permit::{ProcessError, SandboxGrant, SandboxGrantPort};
use futures::future::BoxFuture;
use std::sync::Mutex;

#[derive(Default)]
struct Seat(Mutex<Vec<SandboxGrant>>);

impl SandboxGrantPort for Seat {
    fn granted(&self) -> Vec<SandboxGrant> {
        vec![]
    }

    fn ask<'a>(&'a self, request: &'a SandboxGrant, _command: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            self.0.lock().unwrap().push(request.clone());
            false
        })
    }
}

// Catches: a binding Adaptive floor offers a human allowance that cannot work.
// Exact Git control files can now be maintained by the owner.
#[test]
fn adaptive_floor_denials_never_offer_an_ineffective_grant() {
    let Some(root) =
        isolated("adaptive_grants::adaptive_floor_denials_never_offer_an_ineffective_grant")
    else {
        return;
    };
    let workspace = root.join("workspace");
    std::fs::remove_dir(&workspace).unwrap();
    git_at(&root, &["init".as_ref(), "main".as_ref()]);
    git_at(
        &root.join("main"),
        &[
            "worktree".as_ref(),
            "add".as_ref(),
            "--orphan".as_ref(),
            "-b".as_ref(),
            "fixture".as_ref(),
            "../workspace".as_ref(),
        ],
    );
    let credential = root.join("home/.ssh/synthetic");
    std::fs::create_dir(credential.parent().unwrap()).unwrap();
    std::fs::write(&credential, "synthetic").unwrap();
    let alias = root.join("credential-alias");
    std::os::unix::fs::symlink(&credential, &alias).unwrap();
    let env = root.join("home/.env.fixture");
    std::fs::write(&env, "synthetic").unwrap();
    let config = root.join("main/.git/config");
    let metadata_directory = root.join("main/.git/worktrees/empty");
    std::fs::create_dir(&metadata_directory).unwrap();
    let policy = admit(&workspace).unwrap();
    for (path, script, floor) in [
        (&credential, "exec /bin/cat \"$1\"", true),
        (&alias, "exec /bin/cat \"$1\"", true),
        (&env, "exec /bin/cat \"$1\"", true),
        (&config, "printf compromised > \"$1\"", false),
        (&metadata_directory, "exec /bin/rmdir \"$1\"", true),
        (
            &root.join("home/ordinary-output"),
            "exec /usr/bin/touch \"$1\"",
            false,
        ),
    ] {
        let seat = Arc::new(Seat::default());
        let result = execute_with_grants(
            policy.clone(),
            &root,
            vec![
                "/bin/sh".into(),
                "-c".into(),
                script.into(),
                "fixture".into(),
                path.clone().into_os_string(),
            ],
            Some(seat.clone()),
        );
        let Err(ProcessError::Exit { result, .. }) = result else {
            panic!("expected native refusal for {}: {result:?}", path.display());
        };
        let questions = seat.0.lock().unwrap();
        if floor {
            assert!(
                questions.is_empty(),
                "ineffective floor grant offered: {}",
                path.display()
            );
            let note = result
                .content
                .iter()
                .map(|block| block.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            assert!(note.contains("Adaptive floor cannot be lifted"), "{note}");
        } else {
            assert_eq!(
                questions.len(),
                1,
                "eligible denied path lost its owner grant request"
            );
        }
    }
}

// Catches: a program-internal Adaptive hint is never put to the human.
#[test]
fn adaptive_unanchored_refusal_reaches_the_human() {
    let Some(root) = isolated("adaptive_grants::adaptive_unanchored_refusal_reaches_the_human")
    else {
        return;
    };
    let cache = root.join("outside/cache");
    std::fs::create_dir_all(&cache).unwrap();
    let target = std::fs::canonicalize(cache).unwrap().join("run.tmp");
    let policy = admit(&root.join("workspace")).unwrap();
    let seat = Arc::new(Seat::default());
    let result = execute_with_grants(
        policy,
        &root,
        vec![
            "/bin/sh".into(),
            "-c".into(),
            format!("exec /usr/bin/touch '{}'", target.display()).into(),
        ],
        Some(seat.clone()),
    );
    let Err(ProcessError::Exit { result, .. }) = result else {
        panic!("expected native refusal: {result:?}");
    };
    assert_eq!(
        *seat.0.lock().unwrap(),
        vec![SandboxGrant::new(
            CanonPath::new(&target),
            ego_core::permit::SandboxAccess::Write
        )]
    );
    assert!(!target.exists(), "declined hint performed the write");
    let note = result
        .content
        .iter()
        .map(|block| block.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(note.contains("the user declined"), "{note}");
}

/// A person who allows every question; the grants it gave are the run's.
#[derive(Default)]
struct AllowingSeat(Mutex<Vec<SandboxGrant>>);

impl SandboxGrantPort for AllowingSeat {
    fn granted(&self) -> Vec<SandboxGrant> {
        self.0.lock().unwrap().clone()
    }

    fn ask<'a>(&'a self, request: &'a SandboxGrant, _command: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            self.0.lock().unwrap().push(request.clone());
            true
        })
    }
}

// Catches: Adaptive already reads every non-floor path, yet a program-internal
// write to an existing outside file is first offered as a Read grant that
// cannot unblock it, so the person answers a useless question (and "always"
// stores a useless grant) before the Write question that works.
#[test]
fn adaptive_internal_write_to_an_existing_file_is_asked_as_one_write() {
    let Some(root) = isolated(
        "adaptive_grants::adaptive_internal_write_to_an_existing_file_is_asked_as_one_write",
    ) else {
        return;
    };
    let target = root.join("outside/notes.txt");
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(&target, "old").unwrap();
    let target = std::fs::canonicalize(target).unwrap();
    let policy = admit(&root.join("workspace")).unwrap();
    let seat = Arc::new(AllowingSeat::default());
    let script = format!("printf new >> '{}'", target.display());
    let result = execute_with_grants(
        policy.clone(),
        &root,
        vec!["/bin/sh".into(), "-c".into(), script.clone().into()],
        Some(seat.clone()),
    );
    assert!(
        matches!(result, Err(ProcessError::Exit { .. })),
        "{result:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "old",
        "approval replayed the write"
    );
    assert_eq!(
        *seat.0.lock().unwrap(),
        vec![SandboxGrant::new(
            CanonPath::new(&target),
            ego_core::permit::SandboxAccess::Write
        )],
        "result: {result:?}"
    );
    let result = execute_with_grants(
        policy,
        &root,
        vec!["/bin/sh".into(), "-c".into(), script.into()],
        Some(seat.clone()),
    );
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(
        seat.0.lock().unwrap().len(),
        1,
        "the next explicit write asked again"
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "oldnew");
}
