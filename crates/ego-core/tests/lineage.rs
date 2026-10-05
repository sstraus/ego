//! contracts for regression case: durable lineage replay and fork semantics.

use ego_core::event::domain::{DomainEvent, Envelope, EventKind};
use ego_core::event::id::{EventId, ItemId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::generator::types::{ContentBlock, Image, ImageMediaType, Message, Role};
use ego_core::session::{
    ReplayContext, ReplayError, ReplaySource, ReplaySourceError, SessionBlobSource, SessionLineage,
};
use ego_core::state::SessionState;
use ego_core::store::blob::{BlobError, BlobId, BlobRef};
use ego_core::store::session_store::SessionEntry;
use serde_json::json;
use std::collections::{BTreeMap, HashMap};

mod common;

const PNG_1X1: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 4, 0,
    0, 0, 181, 28, 12, 2, 0, 0, 0, 11, 73, 68, 65, 84, 120, 218, 99, 100, 248, 15, 0, 1, 5, 1, 1,
    39, 24, 227, 102, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

fn seq(value: u64) -> Seq {
    serde_json::from_value(json!(value)).unwrap()
}

fn event(at: u64, item: Option<ItemId>, kind: EventKind) -> DomainEvent {
    DomainEvent {
        env: Envelope {
            seq: seq(at),
            id: EventId::new(),
            turn: TurnId::new(),
            item,
            parent: None,
            ts: Timestamp::from_millis(1_000 + at),
            producer: Producer::Main,
        },
        kind,
    }
}

fn prompt(at: u64, item: ItemId, text: &str) -> SessionEntry {
    SessionEntry::Event(event(
        at,
        Some(item),
        EventKind::UserPrompt {
            text: text.to_owned(),
            images: Vec::new(),
            queued_prompt: None,
        },
    ))
}

fn image_prompt(at: u64, item: ItemId, text: &str, reference: BlobRef) -> SessionEntry {
    let image = serde_json::from_value(json!({
        "reference": reference,
        "media_type": "png",
        "width": 1,
        "height": 1
    }))
    .unwrap();
    SessionEntry::Event(event(
        at,
        Some(item),
        EventKind::UserPrompt {
            text: text.to_owned(),
            images: vec![image],
            queued_prompt: None,
        },
    ))
}

#[derive(Default)]
struct FixtureSource {
    lineages: BTreeMap<SessionId, Result<Option<SessionLineage>, ReplaySourceError>>,
    entries: BTreeMap<SessionId, Result<Option<Vec<SessionEntry>>, ReplaySourceError>>,
}

impl ReplaySource for FixtureSource {
    fn lineage(&self, session: &SessionId) -> Result<Option<SessionLineage>, ReplaySourceError> {
        self.lineages.get(session).cloned().unwrap_or(Ok(None))
    }

    fn entries(&self, session: &SessionId) -> Result<Option<Vec<SessionEntry>>, ReplaySourceError> {
        self.entries.get(session).cloned().unwrap_or(Ok(None))
    }
}

#[derive(Default)]
struct FixtureBlobs {
    bytes: HashMap<(SessionId, BlobRef), Vec<u8>>,
}

impl SessionBlobSource for FixtureBlobs {
    fn read(&self, session: &SessionId, reference: &BlobRef) -> Result<Vec<u8>, BlobError> {
        self.bytes
            .get(&(*session, reference.clone()))
            .cloned()
            .ok_or_else(|| BlobError::NotFound {
                reference: reference.id.clone(),
            })
    }
}

fn blob_ref() -> BlobRef {
    BlobRef::new(
        BlobId::parse("431ced6916a2a21a156e38701afe55bbd7f88969fbbfc56d7fe099d47f265460").unwrap(),
        PNG_1X1.len() as u64,
    )
}

#[test]
fn session_lineage_is_internally_tagged_with_only_the_ratified_variants() {
    let source = common::id::<SessionId>("00000000-0000-0000-0000-000000000065");
    let fork = SessionLineage::Fork {
        source,
        at_seq: seq(17),
    };
    let compaction = SessionLineage::Compaction {
        source,
        source_seq: seq(19),
        prompt_revision: 3,
    };

    assert_eq!(
        serde_json::to_value(fork).unwrap(),
        json!({"kind": "fork", "source": source, "at_seq": seq(17)})
    );
    assert_eq!(
        serde_json::to_value(compaction).unwrap(),
        json!({"kind": "compaction", "source": source, "source_seq": seq(19), "prompt_revision": 3})
    );
}

#[test]
fn replay_uses_source_event_seq_cutoff_and_keeps_target_state_local() {
    let root = common::id::<SessionId>("00000000-0000-0000-0000-000000000101");
    let target = common::id::<SessionId>("00000000-0000-0000-0000-000000000102");
    let source_item = common::id::<ItemId>("00000000-0000-0000-0000-000000000103");
    let target_item = common::id::<ItemId>("00000000-0000-0000-0000-000000000104");
    let reference = blob_ref();

    let mut source = FixtureSource::default();
    source.entries.insert(
        root,
        Ok(Some(vec![
            image_prompt(1, source_item, "source prefix", reference.clone()),
            // The run record is the second physical record, but has no place
            // in provider conversation and does not advance semantic seq.
            // RunTransaction has no public constructor, so this fixture uses
            // its existing serde shape rather than a test-local surrogate.
            SessionEntry::RunTransaction(serde_json::from_value(json!({
                "revision": 1,
                "run_id": {"session_id": root, "nonce": "00000000-0000-7000-8000-000000000106"},
                "origin": {"kind": "top_level"},
                "root": "/workspace",
                "selections": {"model": "model/test", "thinking": null, "mode": null, "budget": null, "profile": null},
                "command": {"kind": "command_admitted", "request_id": "00000000-0000-7000-8000-000000000107", "digest": "lineage", "expected_revision": null, "lease": "00000000-0000-7000-8000-000000000108", "executor_id": "00000000-0000-7000-8000-000000000109", "prompt": "run", "model": "model/test", "start": true},
                "status": "running", "observed_at": 2, "pruned_receipts": []
            })).unwrap()),
            prompt(3, ItemId::new(), "must be excluded by semantic cutoff"),
        ])),
    );
    source.lineages.insert(
        target,
        Ok(Some(SessionLineage::Fork {
            source: root,
            at_seq: seq(1),
        })),
    );
    let mut blobs = FixtureBlobs::default();
    blobs
        .bytes
        .insert((root, reference.clone()), PNG_1X1.to_vec());

    let replay = ReplayContext::new(&source, &blobs).replay(&target).unwrap();
    let image = Image::new(PNG_1X1.to_vec(), ImageMediaType::Png).unwrap();
    assert_eq!(
        replay.conversation().messages(),
        [Message {
            role: Role::User,
            content: vec![
                ContentBlock::Image { image },
                ContentBlock::Text {
                    text: "source prefix".to_owned(),
                },
            ],
        }]
    );
    assert_eq!(replay.session_pins(), [root]);
    assert_eq!(replay.blob_pins(), [reference]);

    let target_local = event(
        1,
        Some(target_item),
        EventKind::UserPrompt {
            text: "target-local state".to_owned(),
            images: Vec::new(),
            queued_prompt: None,
        },
    );
    let target_state = SessionState::replay(std::iter::once(target_local));
    assert!(target_state.items.contains_key(&target_item));
    assert!(!target_state.items.contains_key(&source_item));
    assert_eq!(target_state.usage, ego_core::state::Usage::IDENTITY);
    assert!(target_state.live_attempts.is_empty());
}

#[test]
fn nested_forks_follow_edges_iteratively_compaction_is_terminal_and_cycles_fail_closed() {
    let root = common::id::<SessionId>("00000000-0000-0000-0000-000000000201");
    let child = common::id::<SessionId>("00000000-0000-0000-0000-000000000202");
    let target = common::id::<SessionId>("00000000-0000-0000-0000-000000000203");
    let mut source = FixtureSource::default();
    let reference = blob_ref();
    source.entries.insert(
        root,
        Ok(Some(vec![image_prompt(
            1,
            ItemId::new(),
            "root image",
            reference.clone(),
        )])),
    );
    source.entries.insert(
        child,
        Ok(Some(vec![prompt(1, ItemId::new(), "child text")])),
    );
    source.entries.insert(target, Ok(Some(vec![])));
    source.lineages.insert(
        child,
        Ok(Some(SessionLineage::Fork {
            source: root,
            at_seq: seq(1),
        })),
    );
    source.lineages.insert(
        target,
        Ok(Some(SessionLineage::Fork {
            source: child,
            at_seq: seq(1),
        })),
    );
    let mut blobs = FixtureBlobs::default();
    blobs
        .bytes
        .insert((root, reference.clone()), PNG_1X1.to_vec());
    let replay = ReplayContext::new(&source, &blobs).replay(&target).unwrap();
    assert_eq!(replay.session_pins(), [child, root]);
    assert_eq!(
        replay.lineage_root(),
        root,
        "nested forks keep one routing identity"
    );
    let image = Image::new(PNG_1X1.to_vec(), ImageMediaType::Png).unwrap();
    assert_eq!(
        replay.conversation().messages(),
        [Message {
            role: Role::User,
            content: vec![
                ContentBlock::Image { image },
                ContentBlock::Text {
                    text: "root image".to_owned(),
                },
                ContentBlock::Text {
                    text: "child text".to_owned(),
                },
            ],
        }]
    );
    assert_eq!(replay.blob_pins(), [reference]);

    let before_compaction = SessionId::new();
    source.lineages.insert(
        root,
        Ok(Some(SessionLineage::Compaction {
            source: before_compaction,
            source_seq: seq(1),
            prompt_revision: 3,
        })),
    );
    let compacted = ReplayContext::new(&source, &blobs).replay(&target).unwrap();
    assert_eq!(compacted.lineage_root(), before_compaction);
    assert_eq!(
        compacted.conversation(),
        replay.conversation(),
        "compaction ancestors do not add history"
    );

    source.lineages.insert(
        root,
        Ok(Some(SessionLineage::Fork {
            source: target,
            at_seq: seq(1),
        })),
    );
    let error = ReplayContext::new(&source, &blobs)
        .replay(&target)
        .unwrap_err();
    assert!(matches!(error, ReplayError::Cycle { .. }));
}

#[test]
fn missing_session_corruption_unknown_entry_and_blob_failures_are_typed_refusals() {
    let target = common::id::<SessionId>("00000000-0000-0000-0000-000000000301");
    let source_id = common::id::<SessionId>("00000000-0000-0000-0000-000000000302");
    let mut source = FixtureSource::default();
    source.entries.insert(target, Ok(Some(vec![])));
    source.lineages.insert(
        target,
        Ok(Some(SessionLineage::Fork {
            source: source_id,
            at_seq: seq(1),
        })),
    );
    let blobs = FixtureBlobs::default();
    let missing = ReplayContext::new(&source, &blobs)
        .replay(&target)
        .unwrap_err();
    assert!(matches!(missing, ReplayError::MissingSession { .. }));

    // The source adapter reports typed decode failures; ReplayContext must
    // preserve them rather than skipping an unknown referenced record.
    source.lineages.insert(
        target,
        Ok(Some(SessionLineage::Fork {
            source: source_id,
            at_seq: seq(1),
        })),
    );
    source.entries.insert(
        source_id,
        Err(ReplaySourceError::CorruptSession {
            session: source_id,
            reason: "fixture corruption".to_owned(),
        }),
    );
    let corruption = ReplayContext::new(&source, &blobs)
        .replay(&target)
        .unwrap_err();
    assert!(matches!(corruption, ReplayError::CorruptSession { .. }));

    let reference = blob_ref();
    source.lineages.insert(
        target,
        Ok(Some(SessionLineage::Fork {
            source: source_id,
            at_seq: seq(1),
        })),
    );
    source.entries.insert(
        source_id,
        Ok(Some(vec![image_prompt(
            1,
            ItemId::new(),
            "image",
            reference.clone(),
        )])),
    );
    let missing_blob = ReplayContext::new(&source, &blobs)
        .replay(&target)
        .unwrap_err();
    assert!(matches!(missing_blob, ReplayError::MissingBlob { .. }));

    let mut wrong_bytes = FixtureBlobs::default();
    wrong_bytes
        .bytes
        .insert((source_id, reference.clone()), vec![0, 1, 2]);
    let corrupt_blob = ReplayContext::new(&source, &wrong_bytes)
        .replay(&target)
        .unwrap_err();
    assert!(matches!(corrupt_blob, ReplayError::CorruptBlob { .. }));

    source.entries.insert(
        source_id,
        Err(ReplaySourceError::UnknownEntry {
            session: source_id,
            position: seq(2),
        }),
    );
    source.lineages.insert(
        target,
        Ok(Some(SessionLineage::Fork {
            source: source_id,
            at_seq: seq(1),
        })),
    );
    let unknown = ReplayContext::new(&source, &blobs)
        .replay(&target)
        .unwrap_err();
    assert!(matches!(unknown, ReplayError::UnknownEntry { .. }));
}
