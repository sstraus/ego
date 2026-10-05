use ego_core::orchestrator::SpawnReceipt;
use ego_tools::orchestration_events::{ObservationPump, millis};
use serde_json::Value;

// Catches: an old RESULT for a reused peer name suppresses the new child\'s lost-prompt resend.
#[test]
fn old_result_for_reused_name_does_not_cancel_new_spawn_resend() {
    let home = tempfile::tempdir().unwrap();
    let results = home.path().join("Gits/.tmp/results");
    std::fs::create_dir_all(&results).unwrap();
    std::fs::write(
        results.join("worker-previous.md"),
        "RESULT: previous task completed",
    )
    .unwrap();
    let sessions: Value =
        serde_json::from_str(include_str!("fixtures/orchestrator/sessions.json")).unwrap();
    let idle = sessions
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["agent_state"] == "idle")
        .unwrap();
    let spawned_at_ms = millis(std::time::SystemTime::now()) + 1_000;
    let receipt = SpawnReceipt {
        session_id: idle["session_id"].as_str().unwrap().into(),
        name: "worker".into(),
        cwd: home.path().into(),
        prompt: "Complete the new assigned task".into(),
        spawned_at_ms,
    };
    let mut pump = ObservationPump::open(home.path(), home.path()).unwrap();
    let actions = pump
        .supervise_sessions(
            std::slice::from_ref(&receipt),
            &sessions,
            spawned_at_ms + 60_000,
        )
        .unwrap();
    assert_eq!(
        actions,
        vec![receipt],
        "a previous task\'s result must not cancel this child\'s first resend"
    );
}
