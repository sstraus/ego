//! Deterministic observation. This module has no generator or effect port.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::orchestrator::bounded_line;

pub const SCAN_INTERVAL_MS: u64 = 10_000;
pub const BATCH_WINDOW_MS: u64 = 10_000;
pub const STALL_MS: u64 = 20 * 60 * 1_000;

#[derive(Debug, Clone)]
pub struct Observation {
    pub text: String,
    pub at_ms: u64,
    pub result: Option<PathBuf>,
    pub peer: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Salience {
    Activity,
    Card,
}

/// Unknown events stay visible; only deterministic quiet classes are suppressed.
pub fn classify(text: &str) -> Salience {
    let text = text.trim();
    if text.starts_with("RESULT") || text.starts_with("BLOCKED") {
        return Salience::Card;
    }
    if let Some(rest) = text.strip_prefix("BG DONE") {
        return if rest.split_whitespace().any(|word| word == "exit=0") {
            Salience::Activity
        } else {
            Salience::Card
        };
    }
    if text.is_empty()
        || text
            .split_whitespace()
            .next()
            .is_some_and(|word| word.eq_ignore_ascii_case("ack"))
        || text.eq_ignore_ascii_case("heartbeat")
        || text.to_ascii_lowercase().starts_with("ack:")
    {
        Salience::Activity
    } else {
        Salience::Card
    }
}

/// Content identity is shared by result-file and mailbox representations.
pub fn content_id(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.trim().as_bytes()))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Card {
    pub id: String,
    pub text: String,
    pub result: Option<PathBuf>,
    pub at_ms: u64,
}

/// The only persisted pump state, including an outbox for disconnected seats.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Cursor {
    pub observer: String,
    pub inbox_since: u64,
    pub files: BTreeMap<PathBuf, String>,
    seen: BTreeSet<String>,
    completed: BTreeSet<String>,
    idle_since: BTreeMap<String, u64>,
    stalled: BTreeSet<String>,
    pending_since: Option<u64>,
    pending: Vec<Card>,
}

impl Cursor {
    /// A started child or a reserved resend is never submitted again after restart.
    pub fn settle_spawn(&mut self, session_id: &str) -> bool {
        self.seen.insert(format!("spawn-supervised-{session_id}"))
    }

    /// Rebind a result file to its durable native spawn after observer restart.
    pub fn complete_child(&mut self, name: &str) {
        self.completed.insert(name.to_owned());
    }

    pub fn completed(&self, name: &str) -> bool {
        self.completed.contains(name)
    }

    pub fn spawn_settled(&self, session_id: &str) -> bool {
        self.seen
            .contains(&format!("spawn-supervised-{session_id}"))
    }

    pub fn observe(&mut self, event: Observation, now_ms: u64) -> Salience {
        // File and mail copies share the tracked child and event content;
        // identical completions from different children remain distinct.
        let id = match event.peer.as_deref() {
            Some(peer) => content_id(&format!("{peer}\0{}", event.text.trim())),
            None => content_id(&event.text),
        };
        if event.text.trim().starts_with("RESULT")
            && let Some(peer) = event.peer
        {
            self.completed.insert(peer);
        }
        if !self.seen.insert(id.clone()) {
            if let Some(card) = self
                .pending
                .iter_mut()
                .find(|card| card.id == format!("orchestration-{id}"))
                && card.result.is_none()
            {
                card.result = event.result;
            }
            return Salience::Activity;
        }
        let salience = classify(&event.text);
        if salience == Salience::Card {
            self.pending_since.get_or_insert(now_ms);
            self.pending.push(Card {
                id: format!("orchestration-{id}"),
                text: bounded_line(&event.text),
                result: event.result,
                at_ms: event.at_ms,
            });
        }
        salience
    }

    pub fn child_state(&mut self, peer: &str, idle: bool, now_ms: u64) {
        if !idle {
            if self.idle_since.remove(peer).is_some() {
                self.completed.remove(peer);
            }
            self.stalled.remove(peer);
            return;
        }
        let since = *self.idle_since.entry(peer.to_owned()).or_insert(now_ms);
        if now_ms.saturating_sub(since) > STALL_MS
            && !self.completed.contains(peer)
            && self.stalled.insert(peer.to_owned())
        {
            // Include this idle interval so a later, separate stall can be visible.
            self.observe(
                Observation {
                    text: format!(
                        "BLOCKED: {peer} idle over 20 minutes without RESULT (since {since})"
                    ),
                    at_ms: now_ms,
                    result: None,
                    peer: Some(peer.to_owned()),
                },
                now_ms,
            );
        }
    }

    /// Repeated access returns the same ordered batch until its delivery commits.
    pub fn ready(&self, now_ms: u64) -> &[Card] {
        if self
            .pending_since
            .is_some_and(|since| now_ms.saturating_sub(since) >= BATCH_WINDOW_MS)
        {
            &self.pending
        } else {
            &[]
        }
    }

    pub fn acknowledge(&mut self) {
        self.pending.clear();
        self.pending_since = None;
    }
}
