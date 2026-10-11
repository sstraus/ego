//! Non-effect tool rows rendered from real persisted turn events.
mod support;

use ego_cli::render::{
    Capabilities,
    tool_row::{Finished, record},
};
use ego_core::{
    event::id::SessionId, session::ReplaySource, state::SessionState,
    store::session_store::SessionEntry,
};
use ego_tools::store::{JsonlStore, OsIo};
use support::{FIXTURE_SESSION_ID, Places, Plan, Reply, Stub, text, tool_call};

/// Catches history and skill dispatch returning before execution timing is measured.
#[test]
fn history_and_skill_real_turn_rows_have_execution_duration() {
    let places = Places::new();
    let history_alias = format!(
        "__ego_{}",
        &ego_core::digest::sha256_hex(b"history.search")[..58]
    );
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call(
                "history",
                &history_alias,
                r#"{"query":"earlier"}"#,
            )),
            Reply::Sse(tool_call(
                "skill",
                "skill",
                r#"{"kind":"invoke","name":"alpha","arguments":""}"#,
            )),
            Reply::Sse(text("done")),
        ],
        ..Plan::default()
    });
    places.ego(&stub).home_file(
        "skills/alpha/SKILL.md",
        "---\nname: alpha\ndescription: timing fixture\n---\nAnswer briefly.\n",
    );
    let finished = places
        .ego(&stub)
        .args([
            "--sandbox",
            "off",
            "--model",
            "ollama/qwen3",
            "--session-id",
            FIXTURE_SESSION_ID,
            "use both tools",
        ])
        .run();
    assert_eq!(finished.code, Some(0), "{}", finished.stderr);
    let store = JsonlStore::new(
        places.home_path().join("sessions"),
        places.workspace_path(),
        Box::new(OsIo),
    );
    let state = SessionState::replay(
        store
            .entries(&FIXTURE_SESSION_ID.parse::<SessionId>().unwrap())
            .unwrap()
            .unwrap()
            .into_iter()
            .filter_map(|entry| match entry {
                SessionEntry::Event(event) => Some(event),
                _ => None,
            }),
    );
    for name in ["history.search", "skill"] {
        let item = state
            .items
            .values()
            .find(|item| item.tool_name.as_deref() == Some(name))
            .expect("real tool call");
        assert_eq!(item.ok, Some(true), "{name}: {:?}", item.tool_result);
        let result = item.tool_result.as_ref().unwrap().model_text();
        let row = record(
            &Finished {
                name,
                args: &item.args,
                ok: true,
                result: &result,
                timing: item.tool_timing,
            },
            Capabilities::plain(),
        )
        .rows;
        assert!(!row.contains("duration unavailable"), "{row}");
        assert!(item.tool_timing.unwrap().execution_ms.is_some(), "{row}");
    }
}
