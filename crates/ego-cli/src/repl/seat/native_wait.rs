//! A native wait exists only while the terminal owns a human presentation.

use super::*;
use crate::render::osc::State;
use ego_core::event::id::TurnId;
use serde_json::json;

pub(super) struct Wait {
    session: SessionId,
    turn: Option<TurnId>,
    id: ItemId,
}

impl Wait {
    fn open(
        surface: &ModalSurface<'_>,
        id: ItemId,
        data: serde_json::Value,
        tool: Option<&str>,
    ) -> Self {
        let mut wait = Self {
            session: surface.drafts.active(),
            turn: None,
            id,
        };
        tell_host_current(surface.status, |osc, quiet, current| {
            wait.turn = current.native_turn;
            let state = tool.map_or_else(
                || osc.state(State::Awaiting),
                |tool| osc.awaiting(tool, !quiet),
            );
            [
                state,
                osc.agent_event(wait.session, wait.turn, "modal_open", Some(id), data),
            ]
            .concat()
        });
        wait
    }

    pub(super) fn permission(surface: &ModalSurface<'_>, request: &PermissionRequest) -> Self {
        Self::open(
            surface,
            ItemId::new(),
            json!({"kind": "permission", "label": request.tool().chars().filter(|ch| !ch.is_control()).take(256).collect::<String>()}),
            Some(request.tool()),
        )
    }

    pub(super) fn question(
        surface: &ModalSurface<'_>,
        id: ItemId,
        request: &QuestionRequest,
    ) -> Self {
        let mut question = request.clone();
        if let QuestionMode::Round(round) = &mut question.mode {
            round
                .requests
                .retain(|key, _| !round.completed.contains_key(key));
            round.completed.clear();
        }
        Self::open(
            surface,
            id,
            json!({"kind": "question", "question": question}),
            None,
        )
    }

    pub(super) fn settle(self, status: &SharedStatus, outcome: &str) {
        // Turn completion has already retracted every wait in that turn.
        // Do not emit a second settlement or resurrect its coarse activity.
        tell_host_current(status, |osc, _, current| {
            if self.turn.is_some() && current.native_turn != self.turn {
                return String::new();
            }
            [
                osc.agent_event(
                    self.session,
                    self.turn,
                    "modal_end",
                    Some(self.id),
                    json!({"outcome": outcome}),
                ),
                osc.state(if current.activity {
                    State::Busy
                } else {
                    State::Idle
                }),
            ]
            .concat()
        });
    }
}
