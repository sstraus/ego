//! The catalogue: which sessions belong to a workspace, which one is "latest",
//! and what a listing says.
//!
//! Resolve tests use real persisted session logs. Formatting and ordering use
//! literal metadata so their expected policy remains independent of the store.
//!
//! `docs/architecture.md`.

use std::path::{Path, PathBuf};

use ego_cli::catalogue::{self, CATALOGUE_VERSION};
use ego_core::event::domain::DomainEvent;
use ego_core::event::id::SessionId;
use ego_core::event::time::Timestamp;
use ego_core::store::session_store::{SessionMeta, SessionStore};

/// Real log fixtures let explicit lookup exercise per-session authority.
fn real_catalogue(metas: Vec<SessionMeta>) -> (tempfile::TempDir, ego_tools::store::JsonlStore) {
    use ego_core::event::domain::{Envelope, EventKind};
    use ego_core::event::id::{EventId, Producer, Seq, TurnId};
    use ego_core::session_lifecycle::SessionLifecycleStore;
    use ego_tools::store::{JsonlStore, OsIo};
    let root = tempfile::tempdir().unwrap();
    let store = JsonlStore::new(root.path(), "/ws", Box::new(OsIo));
    for meta in metas {
        store
            .publish_empty_session(meta.id, &meta.workspace_root, meta.created)
            .unwrap();
        let mut seq = Seq::ZERO;
        store
            .append(
                &meta.id,
                &DomainEvent {
                    env: Envelope {
                        seq: seq.next(),
                        id: EventId::new(),
                        turn: TurnId::new(),
                        item: None,
                        parent: None,
                        ts: meta.updated,
                        producer: Producer::Main,
                    },
                    kind: EventKind::UserPrompt {
                        text: "fixture".into(),
                        audio: Vec::new(),
                        images: Vec::new(),
                        queued_prompt: None,
                    },
                },
            )
            .unwrap();
    }
    (root, store)
}

/// A session id whose text order is known, so a tie-break can be asserted
/// rather than observed. `0…01` sorts before `0…02` as a string, which is what
/// `by_recency` compares.
fn id(last: u8) -> SessionId {
    let text = format!("00000000-0000-0000-0000-0000000000{last:02}");
    text.parse().expect("a well-formed uuid")
}

fn meta(last: u8, workspace: &str, updated: u64) -> SessionMeta {
    SessionMeta {
        profiles: Default::default(),
        id: id(last),
        workspace_root: PathBuf::from(workspace),
        model: None,
        created: Timestamp::from_millis(1_000),
        updated: Timestamp::from_millis(updated),
        title: None,
        summary: None,
        summary_failures: 0,
        lineage: None,
    }
}

fn here() -> &'static Path {
    Path::new("/ws")
}

// --- which sessions belong here ---

/// The default is this workspace, which is `CLI.md` §2 line 39. A session
/// created somewhere else is not this workspace's business, and the comparison
/// is on the root the header recorded rather than on anything derived.
#[test]
fn a_listing_holds_only_the_sessions_of_this_workspace() {
    let all = vec![
        meta(1, "/ws", 3_000),
        meta(2, "/elsewhere", 9_000),
        meta(3, "/ws", 5_000),
    ];
    let kept = catalogue::in_workspace(all, here());
    assert_eq!(
        kept.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![id(3), id(1)],
        "the other workspace's session is not this workspace's, and 5_000 is \
         more recent than 3_000"
    );
}

/// Most recently updated first, because that is the order `ego resume` selects
/// "latest" by. A list whose first row is not the row the next command would
/// pick is a list that lies about what the next command does.
#[test]
fn the_first_row_is_the_one_resume_would_continue() {
    let all = vec![
        meta(1, "/ws", 3_000),
        meta(2, "/ws", 9_000),
        meta(3, "/ws", 5_000),
    ];
    let (_root, store) = real_catalogue(all.clone());
    let listed = catalogue::in_workspace(all, here());
    let chosen = catalogue::resolve(&store, here(), None).expect("a session belongs here");
    assert_eq!(listed[0].id, chosen);
    assert_eq!(chosen, id(2));
}

/// `updated` is milliseconds and a scripted pair of runs can land inside one,
/// so the order cannot be left to whichever directory the store read first —
/// that would make `ego ls` and `ego resume` disagree about "latest" on
/// exactly the runs a script produces.
#[test]
fn sessions_updated_in_the_same_millisecond_are_still_ordered() {
    let forwards = vec![meta(1, "/ws", 7_000), meta(2, "/ws", 7_000)];
    let backwards = vec![meta(2, "/ws", 7_000), meta(1, "/ws", 7_000)];
    let one = catalogue::in_workspace(forwards, here());
    let other = catalogue::in_workspace(backwards, here());
    assert_eq!(
        one.iter().map(|m| m.id).collect::<Vec<_>>(),
        other.iter().map(|m| m.id).collect::<Vec<_>>(),
        "the order must not depend on the order the store happened to read"
    );
    assert_eq!(one[0].id, id(1));
}

// --- which session resume continues ---

/// A store full of other workspaces' sessions is not "no sessions", and the
/// refusal says which of the two it is. `--all` is on `ls`, not on `resume`:
/// continuing an arbitrary session from somewhere else is refused below.
#[test]
fn resume_with_no_id_and_nothing_here_names_the_workspace() {
    let (_root, store) = real_catalogue(vec![meta(1, "/elsewhere", 9_000)]);
    let refusal = catalogue::resolve(&store, here(), None)
        .expect_err("nothing in this workspace can be resumed")
        .to_string();
    assert!(refusal.contains("/ws"), "{refusal}");
    assert!(refusal.contains("no session belongs"), "{refusal}");
}

/// `--session-id` creates the session when it is not there; `resume` must not.
/// A mistyped id that succeeded by starting an empty session would give the
/// caller a new thing wearing the name of the one they asked to continue.
#[test]
fn resume_refuses_an_id_no_session_has_rather_than_creating_one() {
    let (_root, store) = real_catalogue(vec![meta(1, "/ws", 9_000)]);
    let refusal = catalogue::resolve(&store, here(), Some(id(7)))
        .expect_err("no session has that id")
        .to_string();
    assert!(refusal.contains("no session"), "{refusal}");
    assert!(
        refusal.contains("--all"),
        "the refusal should say how to see what there is: {refusal}"
    );
    names_the_id_and_nothing_rustish(&refusal, id(7));
}

/// A refusal an operator reads must carry the id they can hand back to
/// `ego resume`, and must not carry the name of a Rust type.
///
/// `SessionId` derives `Debug` and implements `Display`, and the two differ:
/// `{id:?}` renders `SessionId(0192…)`. Both compile, both look right in a
/// format string, and only one is a sentence — which is why this is asserted
/// rather than left to whoever writes the next refusal. It was `Debug` in both
/// of `resolve`'s refusals until an operator-eye look at the built binary
/// showed `session SessionId(0192aaaa-…) belongs to /somewhere/else`.
fn names_the_id_and_nothing_rustish(refusal: &str, wanted: SessionId) {
    assert!(
        refusal.contains(&wanted.to_string()),
        "the refusal must name the id: {refusal}"
    );
    assert!(
        !refusal.contains("SessionId("),
        "a Rust type name reached an operator: {refusal}"
    );
}

/// The session's history was recorded against one tree and this run's tools
/// would work in another, so continuing it here is not continuing it. Both
/// paths are named, which is all `-C` needs to fix it.
#[test]
fn resume_refuses_a_session_belonging_to_another_workspace_naming_both() {
    let (_root, store) = real_catalogue(vec![meta(1, "/elsewhere", 9_000)]);
    let refusal = catalogue::resolve(&store, here(), Some(id(1)))
        .expect_err("that session belongs somewhere else")
        .to_string();
    assert!(refusal.contains("/elsewhere"), "{refusal}");
    assert!(refusal.contains("/ws"), "{refusal}");
    assert!(
        refusal.contains("-C /elsewhere"),
        "the refusal should be actionable: {refusal}"
    );
    names_the_id_and_nothing_rustish(&refusal, id(1));
}

/// An explicitly named session of this workspace is simply resolved — this is
/// the ordinary path, and it is asserted so the three refusals above cannot be
/// mistaken for the whole behaviour.
#[test]
fn resume_takes_the_session_it_was_named() {
    let (_root, store) = real_catalogue(vec![meta(1, "/ws", 3_000), meta(2, "/ws", 9_000)]);
    let chosen = catalogue::resolve(&store, here(), Some(id(1))).expect("it belongs here");
    assert_eq!(
        chosen,
        id(1),
        "a named id is taken as named, not replaced by the latest"
    );
}

/// A store that cannot be listed is not "no sessions here". Reporting it as
/// the empty case would tell an operator with a permissions problem to start a
/// session they already have.
#[test]
fn a_store_that_cannot_be_listed_says_so() {
    let root = tempfile::tempdir().unwrap();
    let not_directory = root.path().join("not-a-directory");
    std::fs::write(&not_directory, "file").unwrap();
    let store =
        ego_tools::store::JsonlStore::new(&not_directory, here(), Box::new(ego_tools::store::OsIo));
    let refusal = catalogue::resolve(&store, here(), None)
        .expect_err("the store failed")
        .to_string();
    assert!(refusal.contains("could not be listed"), "{refusal}");
    assert!(refusal.to_lowercase().contains("directory"), "{refusal}");
}

// --- what a listing says ---

/// §4.6: an object with a `sessions` key carrying its own `v`, not a bare
/// array — an array has nowhere to put a version, and a consumer that cannot
/// see the version cannot be told the shape changed.
#[test]
fn the_json_listing_is_an_object_that_carries_its_own_version() {
    let listing = catalogue::Listing::of(&[meta(1, "/ws", 9_000)]);
    let value = serde_json::to_value(&listing).expect("a listing serialises");
    assert_eq!(value["v"], CATALOGUE_VERSION);
    assert!(value["sessions"].is_array(), "{value}");
    assert_eq!(value["sessions"][0]["workspace_root"], "/ws");
    assert_eq!(value["sessions"][0]["created"], 1_000);
    assert_eq!(value["sessions"][0]["updated"], 9_000);
}

/// An omitted key and a key nobody has set are two states with identical
/// bytes, so the fields nothing populates are `null` rather than absent.
#[test]
fn the_fields_nothing_sets_are_null_rather_than_missing() {
    let listing = catalogue::Listing::of(&[meta(1, "/ws", 9_000)]);
    let value = serde_json::to_value(&listing).expect("a listing serialises");
    let row = &value["sessions"][0];
    assert!(
        row.get("model").is_some() && row["model"].is_null(),
        "{row}"
    );
    assert!(
        row.get("title").is_some() && row["title"].is_null(),
        "{row}"
    );
}

/// The empty store is an empty array and not an absent key: a script that gets
/// no `sessions` at all cannot tell an empty store from a shape it does not
/// understand.
#[test]
fn an_empty_store_still_writes_the_sessions_key() {
    let value = serde_json::to_value(catalogue::Listing::of(&[])).expect("it serialises");
    assert_eq!(value["sessions"], serde_json::json!([]));
}

/// The header row is always there, so `ego ls` in an empty store still shows
/// the person who typed it that the command ran.
#[test]
fn the_table_has_a_header_even_with_nothing_to_list() {
    let lines = catalogue::table(&[], false);
    assert_eq!(lines.len(), 1);
    assert!(lines[0].starts_with("ID"), "{:?}", lines[0]);
    assert!(lines[0].contains("UPDATED"), "{:?}", lines[0]);
}

/// Without `--all` every row would carry the same path — the workspace the
/// caller is standing in — which is the definition of a column that says
/// nothing.
#[test]
fn the_workspace_column_appears_only_under_all() {
    let rows = [meta(1, "/ws", 9_000)];
    assert!(!catalogue::table(&rows, false)[0].contains("WORKSPACE"));
    let widened = catalogue::table(&rows, true);
    assert!(widened[0].contains("WORKSPACE"), "{:?}", widened[0]);
    assert!(widened[1].contains("/ws"), "{:?}", widened[1]);
}

/// `updated` is rendered as a UTC calendar time an operator can read, not as
/// the epoch milliseconds the log carries. The value is pinned rather than
/// recomputed by the test: a date function checked against itself checks
/// nothing.
#[test]
fn the_table_renders_a_timestamp_a_person_can_read() {
    // 1_787_206_085_352 ms = 2026-08-20T06:08:05.352Z — the timestamp the
    // golden session log's header carries, so the value under test is one this
    // repository actually writes rather than one invented for a test.
    let rendered = catalogue::table(&[meta(1, "/ws", 1_787_206_085_352)], false);
    assert!(
        rendered[1].contains("2026-08-20 06:08"),
        "{:?}",
        rendered[1]
    );
}

/// A missing timestamp is visibly absent rather than impersonating the Unix
/// epoch. Real timestamps still exercise the leap-day edge of the calendar
/// conversion.
#[test]
fn the_calendar_arithmetic_holds_at_the_edges() {
    let at = |millis| catalogue::table(&[meta(1, "/ws", millis)], false)[1].clone();
    assert!(!at(0).contains("1970-01-01"), "{}", at(0));
    assert!(at(0).contains('—'), "{}", at(0));
    // 2024-02-29T23:59:00Z — a leap day, and the last minute of it.
    assert!(
        at(1_709_251_140_000).contains("2024-02-29 23:59"),
        "{}",
        at(1_709_251_140_000)
    );
    // 2024-03-01T00:00:00Z — the day after, so an off-by-one in the leap
    // correction moves it rather than hiding inside February.
    assert!(
        at(1_709_251_200_000).contains("2024-03-01 00:00"),
        "{}",
        at(1_709_251_200_000)
    );
}

/// Columns are padded to the widest cell, so a long model slug does not push
/// the following columns out of alignment on the rows that do not have one.
#[test]
fn columns_line_up_across_rows_of_different_widths() {
    let mut short = meta(1, "/ws", 9_000);
    short.model = Some("ollama/qwen3".to_string());
    let mut long = meta(2, "/ws", 8_000);
    long.model = Some("openrouter/z-ai/glm-4.7".to_string());

    let lines = catalogue::table(&[short, long], true);
    let at = |line: &str| line.find("/ws").expect("the workspace column is there");
    assert_eq!(
        at(&lines[1]),
        at(&lines[2]),
        "the column after the widest cell must start in the same place:\n{}\n{}",
        lines[1],
        lines[2]
    );
}

/// Trailing spaces on every line are invisible until something diffs them, so
/// the last column is not padded.
#[test]
fn no_row_ends_in_whitespace() {
    let mut row = meta(1, "/ws", 9_000);
    row.title = Some("short".to_string());
    let other = meta(2, "/ws", 8_000);
    for line in catalogue::table(&[row, other], false) {
        assert_eq!(line, line.trim_end(), "{line:?} ends in whitespace");
    }
}
