use std::collections::BTreeMap;
use std::time::SystemTime;

use ego_core::orchestrator::{Mode, SpawnReceipt};
use ego_core::permit::{CanonPath, PrepareCtx};
use ego_tools::fs::OsFs;
use ego_tools::orchestration_events::{ObservationPump, millis};
use ego_tools::orchestrator_active::tool_with_rework_limits;
use serde_json::json;

// Catches: an unrelated tracked child reusing a peer name resumes the older owner's chat.
#[test]
fn reused_peer_name_does_not_resume_the_conversation_that_launched_another_child() {
    let home = tempfile::tempdir().unwrap();
    let state = home.path().join("Gits/.tmp/coordinator");
    let results = home.path().join("Gits/.tmp/results");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::create_dir_all(&results).unwrap();
    // Existing tuic-spawn.sh stores the current tracked child's session in column one.
    std::fs::write(
        state.join("children.tsv"),
        "unrelated-session\tworker\t/workspace\tOther work\t10-06 14:00\n",
    )
    .unwrap();
    std::fs::write(
        results.join("worker-1.md"),
        "RESULT: unrelated work completed",
    )
    .unwrap();
    let receipt = SpawnReceipt {
        session_id: "original-session".into(),
        name: "worker".into(),
        cwd: home.path().into(),
        prompt: "Original work".into(),
        spawned_at_ms: 1,
    };
    let mut pump = ObservationPump::open(home.path(), home.path()).unwrap();
    let now = millis(SystemTime::now());
    pump.supervise_sessions(std::slice::from_ref(&receipt), &json!([]), now)
        .unwrap();
    let cards = pump.ready(now + 20_000).to_vec();
    assert_eq!(cards.len(), 1);
    assert!(
        !pump
            .reserve_resume(&cards[0], "original-chat", &receipt)
            .unwrap(),
        "an unrelated child's result must not reserve a resume in the original chat"
    );
}

// Catches: TUIC's accepted short UUID reference bypasses the two-request rework cap.
#[test]
fn third_rework_cannot_bypass_the_budget_by_using_a_short_session_reference() {
    let home = tempfile::tempdir().unwrap();
    let ctx = PrepareCtx {
        cwd: CanonPath::new(std::fs::canonicalize(home.path()).unwrap()),
        resolver: &OsFs,
    };
    let active = tool_with_rework_limits(
        Mode::Active,
        home.path(),
        Some("parent".into()),
        BTreeMap::from([("11111111-2222-3333-4444-555555555555".into(), 2)]),
    );
    assert!(
        active.prepare(json!({"operation":"say", "session_id":"11111111", "message":"Fix the failing case"}), &ctx).is_err(),
        "a third correction must be refused before TUIC resolves the short reference"
    );
}
