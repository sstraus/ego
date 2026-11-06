//! Immutable image replay after the source is tombstoned and recovered.

use std::sync::Arc;
use std::time::Duration;

use ego_core::event::domain::{DomainEvent, Envelope, EventKind, ExitReason};
use ego_core::event::id::{EventId, ItemId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::{Clock, Timestamp};
use ego_core::generator::types::{ContentBlock, ImageMediaType, ImageRef};
use ego_core::run::RequestId;
use ego_core::session::ReplayContext;
use ego_core::session_lifecycle::{
    ActivityFuture, CloseFuture, ClosedSession, HoldSignalFuture, SessionActivity,
    SessionLifecycle, SessionLifecycleStore, SessionRunLifecycle,
};
use ego_core::store::blob::BlobStore;
use ego_core::store::session_store::SessionStore;
use ego_tools::store::{JsonlStore, LeaseInputs, OsIo};

struct NoRuns;
impl SessionRunLifecycle for NoRuns {
    fn activity(&self, _: SessionId) -> ActivityFuture<'_> {
        Box::pin(std::future::ready(Ok(SessionActivity::Inactive)))
    }
    fn cancel_and_settle(&self, id: SessionId) -> CloseFuture<'_> {
        Box::pin(std::future::ready(Ok(ClosedSession {
            id,
            cancelled: true,
        })))
    }
    fn hold(&self, _: SessionId, _: RequestId) -> HoldSignalFuture<'_> {
        Box::pin(std::future::ready(Ok(false)))
    }
}

struct FixedClock;
impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_millis(100)
    }
}

// Catches: source tombstoning/recovery collects an image still needed by a fork.
#[test]
fn deleting_and_recovering_the_source_preserves_the_fork_image_bytes() {
    // A valid 1x1 PNG, also used by the pre-existing lineage contract.
    const PNG: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 4,
        0, 0, 0, 181, 28, 12, 2, 0, 0, 0, 11, 73, 68, 65, 84, 120, 218, 99, 100, 248, 15, 0, 1, 5,
        1, 1, 39, 24, 227, 102, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];
    let sessions = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = Arc::new(JsonlStore::new(
        sessions.path(),
        workspace.path(),
        Box::new(OsIo),
    ));
    let source = SessionId::new();
    store
        .publish_empty_session(source, workspace.path(), Timestamp::from_millis(1))
        .unwrap();
    let reference = {
        let open = store
            .open_writer(
                &source,
                LeaseInputs {
                    pid: std::process::id(),
                    now: Timestamp::from_millis(10),
                    stale_after: Duration::from_secs(60),
                    is_alive: &|_| true,
                },
            )
            .unwrap();
        let evidence = open.writer.evidence_store(4096).unwrap();
        let mut blob = BlobStore::begin(&evidence).unwrap();
        blob.append(PNG).unwrap();
        let reference = blob.commit().unwrap();
        let mut seq = Seq::ZERO;
        let turn = TurnId::new();
        let answer = ItemId::new();
        for (item, kind) in [
            (
                Some(ItemId::new()),
                EventKind::UserPrompt {
                    audio: Vec::new(),
                    text: "describe this picture".into(),
                    images: vec![
                        ImageRef::new(reference.clone(), ImageMediaType::Png, 1, 1).unwrap(),
                    ],
                    queued_prompt: None,
                },
            ),
            (
                Some(answer),
                EventKind::AssistantMessage {
                    text: "one pixel".into(),
                },
            ),
            (
                None,
                EventKind::TurnCompleted {
                    ok: true,
                    exit_reason: ExitReason::Completed,
                },
            ),
        ] {
            open.writer
                .append(
                    &source,
                    &DomainEvent {
                        env: Envelope {
                            seq: seq.next(),
                            id: EventId::new(),
                            turn,
                            item,
                            parent: None,
                            ts: Timestamp::from_millis(11),
                            producer: Producer::Main,
                        },
                        kind,
                    },
                )
                .unwrap();
        }
        (reference, answer)
    };
    let service = SessionLifecycle::new(Arc::clone(&store), Arc::new(NoRuns), Arc::new(FixedClock));
    let child = futures::executor::block_on(service.fork_at(source, reference.1))
        .unwrap()
        .id;
    let before = ReplayContext::new(store.as_ref(), store.as_ref())
        .replay(&child)
        .unwrap();
    futures::executor::block_on(service.delete(source)).unwrap();
    {
        let open = store
            .open_writer(
                &source,
                LeaseInputs {
                    pid: std::process::id(),
                    now: Timestamp::from_millis(110),
                    stale_after: Duration::from_secs(60),
                    is_alive: &|_| true,
                },
            )
            .unwrap();
        assert!(open.state.deleted);
        // This invokes production blob GC using reachability from the journal.
        open.writer.evidence_store(4096).unwrap();
    }
    let after = ReplayContext::new(store.as_ref(), store.as_ref())
        .replay(&child)
        .unwrap();
    assert_eq!(after.conversation(), before.conversation());
    let image = after.conversation().messages()[0]
        .content
        .iter()
        .find_map(|block| match block {
            ContentBlock::Image { image } => Some(image),
            _ => None,
        })
        .expect("the fork still has its image");
    assert_eq!(image.bytes(), PNG);
}
