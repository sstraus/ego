//! Native terminal metadata projected from the existing public event stream.

use ego_core::event::domain::ExitReason;
use ego_core::event::id::{SessionId, TurnId};
use ego_core::public::{ChildPhase, ItemKind, PublicEvent};
use serde_json::json;

use crate::render::osc::Osc;

#[derive(Default)]
pub(super) struct NativeEvents {
    session: Option<SessionId>,
    turn: Option<TurnId>,
}

impl NativeEvents {
    pub(super) fn observe(&mut self, osc: Osc, event: &PublicEvent) -> String {
        match event {
            PublicEvent::SessionStarted { session, .. } => {
                self.session = Some(*session);
                self.turn = None;
                return osc.agent_event(*session, None, "session_start", None, json!({}));
            }
            PublicEvent::TurnStarted { turn } => {
                if self.turn == Some(*turn) {
                    return String::new();
                }
                self.turn = Some(*turn);
                return self.frame(osc, "turn_start", None, json!({}));
            }
            _ => {}
        }
        let (turn, event_name, id, data) = match event {
            PublicEvent::ItemStarted {
                turn,
                item,
                kind,
                name,
                ..
            } if *kind == ItemKind::Tool => (
                *turn,
                "tool_start",
                Some(*item),
                json!({"label": name.as_deref().map(label)}),
            ),
            PublicEvent::ItemCompleted {
                turn,
                item,
                kind,
                ok,
                ..
            } if *kind == ItemKind::Tool => (
                *turn,
                "tool_end",
                Some(*item),
                json!({"outcome": if *ok { "completed" } else { "failed" }}),
            ),
            PublicEvent::ChildLifecycle {
                turn,
                item,
                child,
                phase,
            } => {
                let (event, outcome) = match phase {
                    ChildPhase::Started => ("child_start", None),
                    ChildPhase::Settled { outcome } => ("child_end", Some(outcome)),
                };
                (
                    *turn,
                    event,
                    Some(*item),
                    json!({"child": child, "outcome": outcome}),
                )
            }
            PublicEvent::ChildProgress {
                turn,
                item,
                activity,
                output_tokens,
            } => (
                *turn,
                "child_activity",
                Some(*item),
                json!({"label": label(activity), "output_tokens": output_tokens}),
            ),
            PublicEvent::Error {
                turn, item, kind, ..
            } => (*turn, "error", *item, json!({"kind": kind})),
            PublicEvent::TurnCompleted {
                turn,
                ok,
                exit_reason,
            } => {
                let outcome = match exit_reason {
                    ExitReason::Cancelled(_) => "interrupted",
                    ExitReason::Paused => "paused",
                    ExitReason::Completed if *ok => "completed",
                    _ => "failed",
                };
                (
                    *turn,
                    "turn_end",
                    None,
                    json!({"outcome": outcome, "reason": exit_reason}),
                )
            }
            _ => return String::new(),
        };
        if self.turn != Some(turn) {
            return String::new();
        }
        let frame = self.frame(osc, event_name, id, data);
        if event_name == "turn_end" {
            self.turn = None;
        }
        frame
    }

    fn frame(
        &self,
        osc: Osc,
        event: &str,
        id: Option<ego_core::event::id::ItemId>,
        data: serde_json::Value,
    ) -> String {
        self.session.map_or_else(String::new, |session| {
            osc.agent_event(session, self.turn, event, id, data)
        })
    }

    pub(super) fn finish(&mut self, osc: Osc, outcome: &str) -> String {
        let turn = self
            .turn
            .map(|_| self.frame(osc, "turn_end", None, json!({"outcome": outcome})))
            .unwrap_or_default();
        self.turn = None;
        let session = self
            .session
            .take()
            .map(|session| {
                osc.agent_event(
                    session,
                    None,
                    "session_end",
                    None,
                    json!({"outcome": outcome}),
                )
            })
            .unwrap_or_default();
        [turn, session].concat()
    }
}

fn label(text: &str) -> String {
    text.chars()
        .filter(|ch| !ch.is_control())
        .take(256)
        .collect()
}
