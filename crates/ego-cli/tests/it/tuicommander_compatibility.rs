//! What the pinned TUICommander client is told when it initializes real `ego`.
//!
//! keeps the client in its own repository, sharing no crate and
//! reading no ego session file. That is the right boundary and it has one cost:
//! nothing on either side had both halves in view, and the two came apart
//! without a single test going red. TUICommander looked for the pause extension
//! under the response's own `_meta`, as two separately versioned `pause` and
//! `resume` entries; ego advertises it under `agentCapabilities._meta` as one
//! `hold` entry naming both methods. Every test in both repositories passed,
//! and no host could have found `_ego/pause`.
//!
//! So this drives the packaged binary the way that client does — same
//! subcommand, same argv shape, the recorded `initialize` params byte for byte —
//! and pins the answer as a committed golden. It deliberately spawns
//! `CARGO_BIN_EXE_ego` rather than composing a router in-process: the
//! advertisement depends on which ports `ego-cli` wires up, so a fixture that
//! built its own router would prove what a test composed, not what ships.
//!
//! This is the ego half of the compatibility fixture. Updating either
//! protocol participant requires reviewing the captured request and response.
//!
//! The additive `steer` v1 advertisement was reviewed deliberately: the pinned
//! client at `49db2e63`, `src-tauri/src/acp/mod.rs::capability_snapshot`, reads
//! only known fields and the named `hold` and `compact` metadata entries via
//! `ego_extensions`. It ignores the extra `steer` entry; the exact golden still
//! catches removal or reshaping of the capabilities that client uses.
//!
//! adds `sessionCapabilities.delete._meta.ego.force=true` for
//! explicit uncertain-run deletion. The same pinned client checks only
//! `session.delete.is_some()` in `capability_snapshot`, so the additive
//! metadata preserves its deletion availability and existing negotiation.

//! deliberately advertises legacy SSE support. The pinned client
//! copies `mcp.sse` into its capability snapshot; the regenerated golden
//! enables that operation without changing any existing lifecycle extension.

//! enables durable audio input. The pinned client's
//! `capability_snapshot` copies `prompt.audio` directly into `prompt_audio`;
//! this deliberate false-to-true change exposes the implemented surface and
//! does not reshape lifecycle methods or extensions.

//! The child activity v1 advertisement is reconciled with the pinned client.
//! Advertising support is not subscribing the client: only initialize requests
//! with `clientCapabilities._meta.ego.childActivity.version=1` enable updates.
//! The pinned host's `capability_snapshot` reads named hold/compact entries via
//! `ego_extensions` and ignores this extra metadata key. The recorded client
//! request remains unchanged.

use std::io::{BufRead, BufReader, Write};

use crate::support;

use support::Ego;

/// The `initialize` params TUICommander sends, recorded from its client.
///
/// Recorded from TUICommander's `build_initialize_request` in
/// `src-tauri/src/acp/mod.rs`. `clientInfo` is absent in that request; adding it
/// would change the recording rather than describe it.
fn recorded_client_initialize() -> serde_json::Value {
    serde_json::json!({
        "protocolVersion": 1,
        "clientCapabilities": {
            "fs": {"readTextFile": false, "writeTextFile": false},
            "terminal": false,
        },
    })
}

fn golden_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/goldens/tuicommander/initialize.json")
}

/// Ask the real `ego acp` the recorded question and hand back its whole answer.
fn answered_initialize() -> serde_json::Value {
    let mut running = Ego::nowhere().subcommand("acp").spawn_conversing();
    let mut inbox = BufReader::new(running.child.stdout.take().expect("stdout was piped"));
    let mut outbox = running.child.stdin.take().expect("stdin was piped");

    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": recorded_client_initialize(),
    });
    writeln!(outbox, "{request}").expect("the agent is reading");
    outbox.flush().expect("the agent is reading");

    let answer = loop {
        let mut line = String::new();
        let read = inbox.read_line(&mut line).expect("the agent writes UTF-8");
        assert!(read > 0, "`ego acp` closed stdout without answering");
        let frame: serde_json::Value =
            serde_json::from_str(&line).unwrap_or_else(|error| panic!("{line:?}: {error}"));
        if frame.get("id").and_then(serde_json::Value::as_u64) == Some(1) {
            break frame;
        }
    };

    drop(outbox);
    running.finish();
    answer
}

/// Everything the client negotiates on, which is not `agentInfo`.
///
/// `agentInfo.version` moves with every ego release and no host branches on it,
/// so pinning it would turn a release into a golden diff and teach whoever hits
/// it to regenerate this file without reading it.
fn negotiated(answer: &serde_json::Value) -> serde_json::Value {
    let result = answer
        .get("result")
        .unwrap_or_else(|| panic!("initialize was refused: {answer}"));
    serde_json::json!({
        "protocolVersion": result["protocolVersion"],
        "agentCapabilities": result["agentCapabilities"],
    })
}

#[test]
fn the_packaged_agent_answers_the_pinned_client_exactly_what_it_answered_before() {
    let path = golden_path();
    let committed: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display())),
    )
    .expect("the golden is JSON");

    assert_eq!(
        negotiated(&answered_initialize()),
        committed,
        "what `ego acp` advertises changed. A host reads this to decide which \
         operations exist, so read the diff against TUICommander's \
         `capability_snapshot` before regenerating it: \
         cargo test -p ego-cli --test tuicommander_compatibility -- --ignored"
    );
}

/// The two things the pinned client actually looks at, named on their own.
///
/// The golden above would go red for these, but it goes red for any change at
/// all and says only that something moved. This says which thing moved and why
/// it matters, and it is the exact mistake that shipped: metadata read from the
/// wrong object, or an entry shaped the way the other repository's plan guessed
/// rather than the way this one sends it.
#[test]
fn the_hold_extension_is_where_the_client_looks_and_nowhere_else() {
    let answer = answered_initialize();
    let result = &answer["result"];

    let hold = &result["agentCapabilities"]["_meta"]["ego"]["hold"];
    assert_eq!(hold["version"], 1, "hold advertisement: {answer}");
    assert_eq!(hold["pause"], "_ego/pause");
    assert_eq!(hold["resume"], "_ego/resume");

    assert!(
        result.get("_meta").is_none(),
        "the response carries a second `_meta`; a client reading that one \
         instead would find no extension and silently offer no pause: {answer}"
    );
}

/// Not a test — the way the golden above is written.
#[test]
#[ignore = "writes the golden; run deliberately after reading the diff"]
fn regenerate() {
    let path = golden_path();
    std::fs::create_dir_all(path.parent().expect("a golden lives in a directory"))
        .expect("a writable fixture directory");
    let mut text = serde_json::to_string_pretty(&negotiated(&answered_initialize()))
        .expect("the answer serializes");
    text.push('\n');
    std::fs::write(&path, text).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    println!("wrote {}", path.display());
}
