//! contract for the generic session blob store and reachability GC.

use ego_core::event::domain::{DomainEvent, Envelope, EventKind};
use ego_core::event::id::{EventId, ItemId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::generator::history::conversation_from_with_blobs;
use ego_core::generator::types::{ImageMediaType, ImageRef};
use ego_core::state::SessionState;
use ego_core::store::blob::{BlobRead, BlobRef, BlobStore};
use ego_tools::store::SessionBlobStore;
use futures::executor::block_on;
use tempfile::tempdir;

fn fixture() -> (tempfile::TempDir, SessionId, SessionBlobStore) {
    let root = tempdir().expect("blob root");
    let session = SessionId::new();
    let store =
        SessionBlobStore::open(root.path(), session, 64 * 1024).expect("session blob store");
    (root, session, store)
}

fn image_prompt(reference: &str) -> DomainEvent {
    let reference: BlobRef = reference.parse().expect("valid blob reference");
    let image = ImageRef::new(reference, ImageMediaType::Png, 1, 1).expect("image metadata");
    DomainEvent {
        env: Envelope {
            id: EventId::new(),
            seq: Seq::default(),
            ts: Timestamp::from_millis(1),
            turn: TurnId::new(),
            item: Some(ItemId::new()),
            parent: None,
            producer: Producer::Main,
        },
        kind: EventKind::UserPrompt {
            text: "describe".to_string(),
            images: vec![image],
            queued_prompt: None,
        },
    }
}

#[test]
fn blob_commit_is_content_addressed_deduplicated_and_verified_on_read() {
    let (_root, _session, store) = fixture();
    let bytes = b"immutable session image bytes";
    let mut first = store.begin().expect("first transaction");
    first.append(bytes).expect("append first bytes");
    let first_ref = first.commit().expect("publish first blob");
    let mut second = store.begin().expect("second transaction");
    second.append(bytes).expect("append duplicate bytes");
    let second_ref = second.commit().expect("deduplicate duplicate blob");
    assert_eq!(first_ref, second_ref);
    let page = block_on(store.read_page(&BlobRead::new(first_ref.clone(), 0).unwrap()))
        .expect("verified blob read");
    assert_eq!(page.bytes, bytes);
}

#[test]
fn reopening_a_session_does_not_delete_an_active_blob_transaction() {
    let (root, session, first) = fixture();
    let mut writer = first.begin().expect("active blob transaction");
    writer.append(b"live bytes").expect("stage live bytes");

    let second = SessionBlobStore::open(root.path(), session, 64 * 1024)
        .expect("a second session handle may open");
    let reference = writer
        .commit()
        .expect("the second open preserved the first writer's staging file");
    let page = block_on(second.read_page(&BlobRead::new(reference, 0).unwrap()))
        .expect("the committed blob remains readable through the second handle");
    assert_eq!(page.bytes, b"live bytes");

    let evidence = root.path().join(session.to_string()).join("evidence");
    assert!(
        std::fs::read_dir(evidence).unwrap().all(|entry| entry
            .unwrap()
            .path()
            .extension()
            .is_none_or(|extension| extension != "staging")),
        "committed transaction left staging residue"
    );
}

#[test]
fn blob_hard_cap_is_checked_incrementally_and_staging_is_removed_on_abort() {
    let (_root, _session, store) = fixture();
    let mut writer = store.begin().expect("bounded transaction");
    writer.append(&[0_u8; 64 * 1024]).expect("exact cap");
    assert!(
        writer.append(&[1]).is_err(),
        "blob accepted bytes above hard cap"
    );
    writer.abort().expect("abort removes staging");
}

#[test]
fn blob_read_rejects_missing_corrupt_and_wrong_length_references_without_origin_fallback() {
    let (root, session, store) = fixture();
    let mut writer = store.begin().unwrap();
    writer.append(b"known bytes").unwrap();
    let reference = writer.commit().unwrap();
    let missing =
        serde_json::from_value::<ego_core::store::blob::BlobRef>(serde_json::json!(format!(
            "{}:11",
            "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
        )))
        .unwrap();
    assert!(block_on(store.read_page(&BlobRead::new(missing, 0).unwrap())).is_err());
    let path = root
        .path()
        .join(session.to_string())
        .join("evidence")
        .join(format!("{}.blob", reference.id));
    std::fs::write(&path, b"tampered").expect("test blob is writable");
    assert!(block_on(store.read_page(&BlobRead::new(reference.clone(), 0).unwrap())).is_err());
    let mut wrong_length = reference;
    wrong_length.bytes -= 1;
    assert!(block_on(store.read_page(&BlobRead::new(wrong_length, 0).unwrap())).is_err());
}

#[test]
fn unreachable_blobs_are_collectable_but_referenced_evidence_and_images_are_retained() {
    let (_root, _session, store) = fixture();
    let mut writer = store.begin().unwrap();
    writer.append(b"reachable").unwrap();
    let reachable = writer.commit().unwrap();
    let mut orphan = store.begin().unwrap();
    orphan.append(b"orphan").unwrap();
    let orphan = orphan.commit().unwrap();
    let removed = store
        .collect_garbage(std::slice::from_ref(&reachable))
        .expect("GC is session scoped");
    assert_eq!(removed, 1);
    assert!(block_on(store.read_page(&BlobRead::new(reachable, 0).unwrap())).is_ok());
    assert!(block_on(store.read_page(&BlobRead::new(orphan, 0).unwrap())).is_err());
}

#[test]
fn replay_hydration_fails_for_missing_or_corrupt_referenced_image_blobs() {
    for damage in ["missing", "corrupt"] {
        let (root, session, store) = fixture();
        let mut writer = store.begin().expect("image blob transaction");
        writer.append(b"image bytes").expect("image bytes");
        let reference = writer.commit().expect("publish image blob");
        let durable_reference = reference.to_string();
        let event = image_prompt(&durable_reference);
        let round_tripped = serde_json::to_value(&event).expect("image event round-trips");
        let extracted_reference = round_tripped["kind"]["images"][0]["reference"].clone();
        let _: ego_core::store::blob::BlobRef = serde_json::from_value(extracted_reference)
            .expect("round-tripped event contains valid durable reference syntax");
        let state = SessionState::replay([event]);

        if damage == "missing" {
            store
                .collect_garbage(&[])
                .expect("collect unreachable image blob");
        } else {
            let path = root
                .path()
                .join(session.to_string())
                .join("evidence")
                .join(format!("{}.blob", reference.id));
            std::fs::write(path, b"tampered").expect("corrupt image blob");
        }

        // The replay/materialization seam must hydrate and verify every
        // durable image reference before returning provider history.
        assert!(
            conversation_from_with_blobs(&state, &store).is_err(),
            "replay resumed after {damage} referenced image blob"
        );
    }
}
