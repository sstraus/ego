//! Actual CLI process; TUIC response bytes come from the recorded host fixture.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[test]
fn brief_reads_recorded_status_without_a_model_or_consuming_mail() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let state = root.join("Gits/.tmp/coordinator");
    let results = root.join("Gits/.tmp/results");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::create_dir_all(&results).unwrap();
    let children = include_str!("../../ego-tools/tests/fixtures/orchestrator/children.tsv");
    std::fs::write(state.join("children.tsv"), children).unwrap();
    std::fs::write(
        root.join("sessions.json"),
        include_str!("../../ego-tools/tests/fixtures/orchestrator/sessions.json"),
    )
    .unwrap();
    // This fake executes only the recorded read; unexpected inbox or model work fails.
    let tuic = root.join("tuic");
    std::fs::write(&tuic, "#!/bin/sh\n[ \"$1\" = mcp ] && [ \"$2\" = session ] && [ \"$3\" = '{\"action\":\"list\"}' ] || exit 99\ncat \"$HOME/sessions.json\"\n").unwrap();
    std::fs::set_permissions(&tuic, std::fs::Permissions::from_mode(0o700)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ego"))
        .args(["orchestrator", "brief"])
        .env("HOME", root)
        .env("TUIC_SOCKET", root.join("recorded.sock"))
        .env("PATH", format!("{}:/usr/bin:/bin", root.display()))
        .env_remove("EGO_PROFILE")
        .env_remove("EGO_HOME")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("Children:\n"));
    assert!(text.contains("example-worker-a"));
    assert!(text.contains("Unread results:\n"));
    assert!(!text.contains("foreground_process"));
    assert_eq!(
        std::fs::read_to_string(state.join("children.tsv")).unwrap(),
        children
    );
    assert!(
        !root.join(".ego").exists(),
        "inspection must not resolve credentials or write a session"
    );
}

#[test]
fn brief_refuses_missing_socket_instead_of_falling_back_to_a_model() {
    let output = Command::new(env!("CARGO_BIN_EXE_ego"))
        .args(["orchestrator", "brief"])
        .env_remove("TUIC_SOCKET")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("set TUIC_SOCKET to the TUICommander socket path")
    );
}
