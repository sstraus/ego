//! Independent critic cases for story 1136-25e6: queued previews reconcile
//! against `SessionState::unacknowledged_prompts`, the durable authority.
//!
//! Each state below is built through the production reducer. Each preview
//! is staged and settled through the production seat methods. The recall
//! handler in `action_execution.rs` uses the same reconciliation method with
//! the authoritative pending snapshot returned by the recall acknowledgement.

use super::*;
use ego_core::event::domain::{DomainEvent, Envelope, EventKind};
use ego_core::event::id::{EventId, Producer, QueuedPromptId, Seq, TurnId};
use ego_core::event::time::Timestamp;

struct Journal {
    turn: TurnId,
    seq: Seq,
    state: SessionState,
}

impl Journal {
    fn new() -> Self {
        Self {
            turn: TurnId::new(),
            seq: Seq::default(),
            state: SessionState::default(),
        }
    }

    fn apply(&mut self, kind: EventKind) -> SessionState {
        self.state.apply(&DomainEvent {
            env: Envelope {
                seq: self.seq.next(),
                id: EventId::new(),
                turn: self.turn,
                item: Some(ItemId::new()),
                parent: None,
                ts: Timestamp::from_millis(1_787_206_085_352),
                producer: Producer::Main,
            },
            kind,
        });
        self.state.clone()
    }

    fn queue(&mut self, text: &str) -> QueuedPromptId {
        let id = QueuedPromptId::new();
        self.apply(EventKind::PromptQueued {
            audio: Vec::new(),
            id,
            text: text.into(),
            images: Vec::new(),
        });
        id
    }

    fn consume(&mut self, id: QueuedPromptId, text: &str) -> SessionState {
        self.apply(EventKind::UserPrompt {
            audio: Vec::new(),
            text: text.into(),
            images: Vec::new(),
            queued_prompt: Some(id),
        })
    }
}

fn user_message(text: &str) -> ego_core::public::PublicEvent {
    ego_core::public::PublicEvent::ItemCompleted {
        turn: TurnId::new(),
        item: ItemId::new(),
        kind: ego_core::public::ItemKind::UserMessage,
        ok: true,
        text: Some(text.into()),
    }
}

fn admit(status: &Arc<Mutex<SurfaceStatus>>, text: &str) {
    let mut current = status.lock().unwrap();
    current.stage_queue_preview(text.into(), 0);
    current.settle_queue_preview(true);
}

fn shown(waiting: &Waiting, text: &str) -> usize {
    waiting
        .render_prompt_indicator(PromptEditMode::Emacs)
        .matches(&format!("> {text}\n"))
        .count()
}

// Catches: repeated projection of a post-retraction state removes a sibling
// that remains durably pending. This is reconciliation idempotence, not a
// test of the live editor's recall handler.
#[test]
fn repeated_post_retraction_reconciliation_keeps_the_pending_sibling() {
    let status = Arc::new(Mutex::new(SurfaceStatus::default()));
    let waiting = Waiting::new(Arc::clone(&status));
    let mut journal = Journal::new();
    journal.queue("keep me");
    let recalled = journal.queue("recall me");
    admit(&status, "keep me");
    admit(&status, "recall me");
    let state = journal.apply(EventKind::PromptRetracted { id: recalled });
    assert_eq!(state.unacknowledged_prompts().len(), 1);

    // Driver thread: TurnCompleted refresh with the post-retraction state.
    status
        .lock()
        .unwrap()
        .reconcile_queue_previews(state.unacknowledged_prompts());
    // Editor thread: a late recall reply reconciles the same durable snapshot.
    status
        .lock()
        .unwrap()
        .reconcile_queue_previews(state.unacknowledged_prompts());

    assert_eq!(
        shown(&waiting, "keep me"),
        1,
        "a durably pending prompt lost its preview"
    );
    assert_eq!(shown(&waiting, "recall me"), 0);
}

// Catches: per-event reconciliation retires the next queued preview before
// its own UserMessage receipt is observed, so its transcript echo is lost.
// The journal publishes each event with the state after that event, so the
// first receipt sees the second prompt still pending.
#[test]
fn consecutive_deliveries_each_keep_their_transcript_receipt() {
    let status = Arc::new(Mutex::new(SurfaceStatus::default()));
    let waiting = Waiting::new(Arc::clone(&status));
    let mut journal = Journal::new();
    let first = journal.queue("first");
    let second = journal.queue("second");
    admit(&status, "orphan");
    admit(&status, "first");
    admit(&status, "second");

    let after_first = journal.consume(first, "first");
    let receipt = {
        let mut current = status.lock().unwrap();
        let receipt = current.consume_queue_preview(&user_message("first"));
        current.reconcile_queue_previews(after_first.unacknowledged_prompts());
        receipt
    };
    assert_eq!(receipt.as_deref(), Some("> first"));
    assert_eq!(shown(&waiting, "orphan"), 0, "orphan outlived reconcile");
    assert_eq!(shown(&waiting, "second"), 1, "pending prompt was hidden");

    let after_second = journal.consume(second, "second");
    let receipt = {
        let mut current = status.lock().unwrap();
        let receipt = current.consume_queue_preview(&user_message("second"));
        assert!(!current.reconcile_queue_previews(after_second.unacknowledged_prompts()));
        receipt
    };
    assert_eq!(receipt.as_deref(), Some("> second"));
    assert!(status.lock().unwrap().queued_previews.is_empty());
}

// Catches: an orphan whose text equals a genuinely pending prompt is shown
// beside it as two pending copies, or its consumption leaves a stale copy.
#[test]
fn orphan_sharing_text_with_a_pending_prompt_shows_one_copy_then_none() {
    let status = Arc::new(Mutex::new(SurfaceStatus::default()));
    let waiting = Waiting::new(Arc::clone(&status));
    let mut journal = Journal::new();
    admit(&status, "same text");
    let id = journal.queue("same text");
    admit(&status, "same text");
    let pending = journal.state.clone();

    status
        .lock()
        .unwrap()
        .reconcile_queue_previews(pending.unacknowledged_prompts());
    assert_eq!(shown(&waiting, "same text"), 1);

    let consumed = journal.consume(id, "same text");
    let receipt = {
        let mut current = status.lock().unwrap();
        let receipt = current.consume_queue_preview(&user_message("same text"));
        current.reconcile_queue_previews(consumed.unacknowledged_prompts());
        receipt
    };
    assert_eq!(receipt.as_deref(), Some("> same text"));
    assert_eq!(shown(&waiting, "same text"), 0);
}

// Catches: the position search consumes a staged, not yet admitted draft
// with repeated text instead of the older admitted preview, or reconcile
// drops the staged draft before its admission receipt.
#[test]
fn repeated_text_consumes_the_admitted_preview_before_a_staged_draft() {
    let status = Arc::new(Mutex::new(SurfaceStatus::default()));
    let waiting = Waiting::new(Arc::clone(&status));
    let mut journal = Journal::new();
    let first = journal.queue("again");
    admit(&status, "again");
    status
        .lock()
        .unwrap()
        .stage_queue_preview("again".into(), 0);

    let consumed = journal.consume(first, "again");
    {
        let mut current = status.lock().unwrap();
        assert_eq!(
            current
                .consume_queue_preview(&user_message("again"))
                .as_deref(),
            Some("> again")
        );
        current.reconcile_queue_previews(consumed.unacknowledged_prompts());
        assert_eq!(current.queued_previews.len(), 1, "staged draft was lost");
        assert!(!current.queued_previews[0].admitted);
    }
    // The staged draft is now accepted durably and then settled.
    let pending = {
        journal.queue("again");
        journal.state.clone()
    };
    status.lock().unwrap().settle_queue_preview(true);
    assert!(
        !status
            .lock()
            .unwrap()
            .reconcile_queue_previews(pending.unacknowledged_prompts())
    );
    assert_eq!(shown(&waiting, "again"), 1);
}
