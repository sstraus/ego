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
        (&config, "printf compromised > \"$1\"", true),
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
                "ordinary residual write lost its grant request"
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
