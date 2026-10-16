//! Public editor lifecycle contracts using real Unix children.
#![cfg(unix)]

use std::ffi::OsString;

use ego_core::event::domain::CancelCause;
use ego_core::turn::CancelToken;
use ego_tools::exec::{EditorProcessError, run_editor_process};

// Catches: accepting an empty command and trying to spawn an unspecified editor.
#[tokio::test]
async fn empty_editor_command_is_rejected_before_launch() {
    for argv in [Vec::new(), vec![OsString::new()]] {
        let error = run_editor_process(argv, CancelToken::new())
            .await
            .expect_err("an empty editor cannot run");
        assert!(matches!(error, EditorProcessError::InvalidRequest(_)));
    }
}

// Catches: reporting success when the selected editor executable cannot be launched.
#[tokio::test]
async fn missing_editor_executable_reports_launch_failure() {
    let root = tempfile::tempdir().unwrap();
    let error = run_editor_process(
        vec![root.path().join("missing-editor").into_os_string()],
        CancelToken::new(),
    )
    .await
    .expect_err("the missing editor cannot run");
    assert!(matches!(error, EditorProcessError::Launch(_)));
}

// Catches: dropping editor arguments or returning before the editor finishes its write.
#[tokio::test]
async fn successful_editor_finishes_its_candidate_write() {
    let root = tempfile::tempdir().unwrap();
    let candidate = root.path().join("candidate.toml");
    run_editor_process(
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf 'edited candidate' > \"$1\"".into(),
            "editor".into(),
            candidate.clone().into_os_string(),
        ],
        CancelToken::new(),
    )
    .await
    .expect("the editor completed");
    assert_eq!(std::fs::read(candidate).unwrap(), b"edited candidate");
}

// Catches: treating an unsuccessful editor exit as permission to publish the candidate.
#[tokio::test]
async fn unsuccessful_editor_exit_keeps_its_exit_code() {
    let error = run_editor_process(
        vec!["/bin/sh".into(), "-c".into(), "exit 23".into()],
        CancelToken::new(),
    )
    .await
    .expect_err("the editor refused the edit");
    assert!(matches!(error, EditorProcessError::Exit(Some(23))));
}

// Catches: launching an editor after cancellation and allowing it to modify the candidate.
#[tokio::test]
async fn cancellation_before_launch_does_not_write_the_candidate() {
    let root = tempfile::tempdir().unwrap();
    let candidate = root.path().join("candidate.toml");
    let cancel = CancelToken::new();
    cancel.cancel(CancelCause::User);
    let error = run_editor_process(
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf 'unexpected' > \"$1\"".into(),
            "editor".into(),
            candidate.clone().into_os_string(),
        ],
        cancel,
    )
    .await
    .expect_err("a cancelled edit cannot start");
    assert!(matches!(error, EditorProcessError::Cancelled));
    assert!(!candidate.exists());
}
