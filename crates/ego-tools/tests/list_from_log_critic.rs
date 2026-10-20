//! Critic cases for regression case: `JsonlStore::list` projects a session
//! from its log when the manifest is missing, unreadable or corrupt.

use std::path::{Path, PathBuf};

use ego_core::event::domain::{DomainEvent, Envelope, EventKind, ExitReason};
use ego_core::event::id::{EventId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::store::session_store::{SessionMeta, SessionStore};
use ego_tools::store::{JsonlStore, OsIo};
use tempfile::{TempDir, tempdir};

fn dir_name(id: &SessionId) -> String {
    serde_json::to_value(id)
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned()
}

fn ev(seq: Seq, turn: TurnId, kind: EventKind, ts: u64) -> DomainEvent {
    DomainEvent {
        env: Envelope {
            seq,
            id: EventId::new(),
            turn,
            item: None,
            parent: None,
            ts: Timestamp::from_millis(ts),
            producer: Producer::Main,
        },
        kind,
    }
}

struct World {
    _dir: TempDir,
    root: PathBuf,
    store: JsonlStore,
}

impl World {
    fn new() -> Self {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let store = JsonlStore::new(&root, "/ws", Box::new(OsIo));
        Self {
            _dir: dir,
            root,
            store,
        }
    }

    /// One finished turn; `base` offsets the timestamps so sessions differ.
    fn session(&self, base: u64) -> SessionId {
        let id = SessionId::new();
        let turn = TurnId::new();
        let mut seq = Seq::default();
        for (kind, ts) in [
            (
                EventKind::UserPrompt {
                    audio: Vec::new(),
                    text: "hello".to_owned(),
                    images: Vec::new(),
                    queued_prompt: None,
                },
                base + 1,
            ),
            (
                EventKind::AssistantMessage {
                    text: "hi".to_owned(),
                },
                base + 2,
            ),
            (
                EventKind::TurnCompleted {
                    ok: true,
                    exit_reason: ExitReason::Completed,
                },
                base + 3,
            ),
        ] {
            self.store
                .append(&id, &ev(seq.next(), turn, kind, ts))
                .unwrap();
        }
        id
    }

    fn dir(&self, id: &SessionId) -> PathBuf {
        self.root.join(dir_name(id))
    }

    fn manifest(&self, id: &SessionId) -> PathBuf {
        self.dir(id).join("manifest.json")
    }

    fn log(&self, id: &SessionId) -> PathBuf {
        self.dir(id).join("events.jsonl")
    }

    fn listed(&self) -> Vec<SessionMeta> {
        let mut metas = self.store.list().unwrap();
        metas.sort_by_key(|meta| dir_name(&meta.id));
        metas
    }
}

fn append_raw(path: &Path, bytes: &[u8]) {
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}

/// Catches: a session listed twice or dropped when manifests are present for
/// some sessions, missing for others and corrupt for a third.
#[test]
fn mixed_manifest_states_list_every_session_exactly_once() {
    let w = World::new();
    let healthy = w.session(0);
    let missing = w.session(10_000);
    let corrupt = w.session(20_000);
    let expected = w.listed();
    assert_eq!(expected.len(), 3);
    std::fs::remove_file(w.manifest(&missing)).unwrap();
    std::fs::write(w.manifest(&corrupt), b"{\"half").unwrap();

    let after = w.listed();

    assert_eq!(after, expected, "every session once, with unchanged facts");
    let ids: Vec<_> = after.iter().map(|m| m.id).collect();
    assert!(ids.contains(&healthy) && ids.contains(&missing) && ids.contains(&corrupt));
}

/// Catches: a manifest that parses but names another session's id (a copied
/// or mangled cache) hiding a session whose log is intact.
#[test]
fn a_manifest_naming_another_session_does_not_hide_an_intact_log() {
    let w = World::new();
    let a = w.session(0);
    let b = w.session(10_000);
    std::fs::copy(w.manifest(&b), w.manifest(&a)).unwrap();

    let ids: Vec<_> = w.listed().iter().map(|m| m.id).collect();

    assert!(
        ids.contains(&a),
        "session A has an intact log and must stay listed"
    );
    assert!(ids.contains(&b));
}

/// Catches: a manifest path that is unreadable (a directory) failing the
/// whole call or hiding the session instead of falling back to the log.
#[test]
fn an_unreadable_manifest_path_falls_back_to_the_log() {
    let w = World::new();
    let a = w.session(0);
    let b = w.session(10_000);
    let expected = w.listed();
    std::fs::remove_file(w.manifest(&a)).unwrap();
    std::fs::create_dir(w.manifest(&a)).unwrap();

    assert_eq!(w.listed(), expected);
    assert!(w.listed().iter().any(|m| m.id == b));
}

/// Catches: a log being appended by a live session (torn final line) and no
/// manifest making the listing fail, drop the session, or report the torn
/// event's timestamp; and listing truncating the log it only reads.
#[test]
fn a_torn_tail_is_ignored_and_never_truncated_by_listing() {
    let w = World::new();
    let id = w.session(0);
    let expected = w.listed();
    std::fs::remove_file(w.manifest(&id)).unwrap();
    append_raw(&w.log(&id), b"{\"v\":1,\"seq\":99,\"half a li");
    let before = std::fs::read(w.log(&id)).unwrap();

    assert_eq!(w.listed(), expected);

    assert_eq!(
        std::fs::read(w.log(&id)).unwrap(),
        before,
        "list must not edit the log"
    );
    assert!(!w.manifest(&id).exists(), "list must not write a manifest");
}

/// Catches: a corrupt record in the middle of a log letting later, trustworthy
/// -looking lines advance `updated` past the valid prefix.
#[test]
fn a_corrupt_middle_line_stops_the_projection_at_the_valid_prefix() {
    let w = World::new();
    let id = w.session(0);
    std::fs::remove_file(w.manifest(&id)).unwrap();
    let valid_updated = w.listed()[0].updated;
    append_raw(&w.log(&id), b"garbage line\n");
    // A well-formed later event written after the gap.
    let other = w.session(50_000);
    let later = std::fs::read(w.log(&other)).unwrap();
    let later_line = later.split(|&b| b == b'\n').nth(1).unwrap().to_vec();
    append_raw(&w.log(&id), &[later_line, b"\n".to_vec()].concat());

    let metas = w.listed();
    let meta = metas.iter().find(|m| m.id == id).unwrap();

    assert_eq!(
        meta.updated, valid_updated,
        "lines after a gap are not trustworthy"
    );
}

/// Catches: a log copied under another directory name (header id differs from
/// the directory) being listed under an id that `read`/`append` cannot reach.
#[test]
fn a_log_whose_header_names_another_session_is_not_listed_under_that_directory() {
    let w = World::new();
    let id = w.session(0);
    let stray = SessionId::new();
    std::fs::create_dir_all(w.dir(&stray)).unwrap();
    std::fs::copy(w.log(&id), w.log(&stray)).unwrap();

    let ids: Vec<_> = w.listed().iter().map(|m| m.id).collect();

    assert_eq!(
        ids,
        vec![id],
        "the stray directory must not surface or duplicate the session"
    );
}

/// Catches: a session deleted (tombstoned in its log) reappearing in the
/// listing once its manifest is gone.
#[test]
fn a_deleted_session_stays_hidden_without_its_manifest() {
    let w = World::new();
    let id = SessionId::new();
    let turn = TurnId::new();
    let mut seq = Seq::default();
    w.store
        .append(
            &id,
            &ev(
                seq.next(),
                turn,
                EventKind::UserPrompt {
                    audio: Vec::new(),
                    text: "x".to_owned(),
                    images: Vec::new(),
                    queued_prompt: None,
                },
                1,
            ),
        )
        .unwrap();
    w.store
        .append(&id, &ev(seq.next(), turn, EventKind::SessionDeleted, 2))
        .unwrap();
    // `append` leaves the manifest at the pre-delete state; recovery
    // refreshes it so the precondition is a manifest that says deleted.
    w.store.recover(&id).unwrap();
    assert!(
        w.listed().is_empty(),
        "precondition: hidden with a manifest"
    );
    std::fs::remove_file(w.manifest(&id)).unwrap();

    assert!(w.listed().is_empty());
}

/// Catches: a header-only session (what `publish_empty_session` writes, and
/// what recovery rebuilds a manifest for) vanishing from the listing when its
/// manifest is gone, so `ego ls` and `ego resume` disagree about it.
#[test]
fn a_header_only_session_stays_listed_without_its_manifest() {
    use ego_core::session_lifecycle::SessionLifecycleStore;
    let w = World::new();
    let id = SessionId::new();
    w.store
        .publish_empty_session(id, Path::new("/ws"), Timestamp::from_millis(1))
        .unwrap();
    let with_manifest = w.listed();
    std::fs::remove_file(w.manifest(&id)).unwrap();

    assert_eq!(w.listed(), with_manifest);
    assert_eq!(with_manifest.len(), 1);
}

/// Catches: the listing projection and the recovery projection disagreeing
/// on title/summary/updated, so a session changes shape after being opened.
#[test]
fn listing_from_the_log_matches_the_manifest_recovery_writes() {
    let w = World::new();
    let id = w.session(0);
    std::fs::remove_file(w.manifest(&id)).unwrap();
    let from_log = w.listed();

    w.store.recover(&id).unwrap();

    assert!(w.manifest(&id).exists());
    assert_eq!(w.listed(), from_log);
}

/// Catches: a log whose only content is an unterminated header (a session
/// still being created) being listed or failing the call.
#[test]
fn an_unterminated_header_is_skipped_without_failing_siblings() {
    let w = World::new();
    let id = w.session(0);
    let ghost = SessionId::new();
    std::fs::create_dir_all(w.dir(&ghost)).unwrap();
    let header = std::fs::read(w.log(&id)).unwrap();
    let first = header.split(|&b| b == b'\n').next().unwrap();
    std::fs::write(w.log(&ghost), first).unwrap();

    let ids: Vec<_> = w.listed().iter().map(|m| m.id).collect();

    assert_eq!(ids, vec![id]);
}

// Catches: an absent optional evidence collection rejects a durable append
// even though an entirely absent evidence cache already defaults to empty.
#[test]
fn manifest_missing_evidence_collections_still_loads() {
    for field in ["run_contexts", "generations"] {
        let w = World::new();
        let id = w.session(0);
        let path = w.manifest(&id);
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        manifest["evidence"].as_object_mut().unwrap().remove(field);
        std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let mut seq = Seq::ZERO;
        for _ in 0..3 {
            seq.next();
        }
        w.store
            .append(
                &id,
                &ev(
                    seq.next(),
                    TurnId::new(),
                    EventKind::TurnCompleted {
                        ok: true,
                        exit_reason: ExitReason::Completed,
                    },
                    4,
                ),
            )
            .unwrap_or_else(|error| panic!("missing evidence.{field}: {error}"));
        assert_eq!(w.listed()[0].updated, Timestamp::from_millis(4));
        let (state, _) = w.store.resume(&id).unwrap();
        assert_eq!(state.last_seq, seq);
        assert_eq!(state.known_events.len(), 4);
    }
}

// Catches: one unusable manifest and log abort catalogue enumeration instead
// of identifying that session and preserving the other session's resume.
#[test]
fn one_bad_manifest_does_not_hide_other_sessions() {
    let w = World::new();
    let healthy = w.session(0);
    let broken = w.session(10_000);
    std::fs::write(w.manifest(&broken), b"{}").unwrap();
    std::fs::write(w.log(&broken), b"not a log\n").unwrap();
    let mut errors = Vec::new();
    let listed = w
        .store
        .list_readable(&mut |id, error| errors.push((id, error.to_string())))
        .unwrap();
    assert_eq!(
        listed.iter().map(|meta| meta.id).collect::<Vec<_>>(),
        vec![healthy]
    );
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].0, broken);
    assert!(!errors[0].1.is_empty());
    w.store.resume(&healthy).unwrap();
}

// Catches: making absent collections optional also accepts null, wrong types,
// or incomplete evidence records as an empty cache.
#[test]
fn manifest_evidence_defaults_do_not_accept_malformed_present_collections() {
    for field in ["run_contexts", "generations"] {
        for invalid in [
            serde_json::Value::Null,
            serde_json::json!({}),
            serde_json::json!([{}]),
        ] {
            let mut projection = serde_json::json!({"run_contexts": [], "generations": []});
            projection[field] = invalid;
            assert!(
                serde_json::from_value::<ego_core::evidence::ManifestEvidence>(projection).is_err(),
                "malformed {field} must not be defaulted"
            );
        }
    }
}
