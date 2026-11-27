//! Disposable durable-child projection; Reedline remains the only terminal writer.

use super::*;
use ego_core::event::id::SubagentInvocationId;
use std::collections::BTreeMap;
use std::sync::Weak;

#[derive(Default)]
pub(super) struct BackgroundRows {
    owner: Weak<()>,
    pub(super) rows: BTreeMap<SubagentInvocationId, BackgroundRow>,
    inputs: BTreeMap<SubagentInvocationId, TaskInput>,
    attached: Option<SubagentInvocationId>,
    parent_drafts: Option<SessionId>,
}

struct TaskInput {
    alias: String,
    ingress: InteractiveIngress,
    transcript: String,
    settled: bool,
}

pub(super) struct BackgroundRow {
    pub(super) profile: Option<ego_core::model_catalogue::ExecutionProfile>,
    pub(super) name: String,
    pub(super) activity: String,
    pub(super) started: Instant,
    pub(super) elapsed_before: Duration,
    pub(super) output_tokens: u64,
}

/// The session epoch, not a child or turn, owns this attachment.
pub(crate) struct BackgroundTaskSurface {
    owner: Arc<()>,
    status: Weak<Mutex<SurfaceStatus>>,
}

/// A child can replace an existing row, but cannot register or revive one.
#[derive(Clone)]
pub(crate) struct BackgroundActivity {
    owner: Weak<()>,
    status: Weak<Mutex<SurfaceStatus>>,
    invocation: SubagentInvocationId,
}

fn bounded_text(text: &str, limit: usize) -> String {
    let input: String = text.chars().take(limit).collect();
    ego_core::permit::terminal_safe(&input.replace(['\r', '\n', '\t'], " "))
        .chars()
        .take(limit)
        .collect()
}

impl BackgroundRows {
    pub(super) fn attached_alias(&self) -> Option<&str> {
        self.attached.and_then(|id| self.alias(&id))
    }

    pub(super) fn alias(&self, invocation: &SubagentInvocationId) -> Option<&str> {
        self.inputs
            .get(invocation)
            .map(|input| input.alias.as_str())
    }
}

impl BackgroundTaskSurface {
    fn attach(status: &SharedStatus) -> Self {
        let owner = Arc::new(());
        status
            .lock()
            .expect("surface status lock")
            .background_children = BackgroundRows {
            owner: Arc::downgrade(&owner),
            rows: BTreeMap::new(),
            inputs: BTreeMap::new(),
            attached: None,
            parent_drafts: None,
        };
        repaint_status(status);
        Self {
            owner,
            status: Arc::downgrade(status),
        }
    }

    #[cfg(test)]
    pub(crate) fn test_surface() -> (Self, impl FnMut(&str) -> bool) {
        let status = Arc::new(Mutex::new(SurfaceStatus::default()));
        let surface = Self::attach(&status);
        let mut drafts = DraftStash::new(SessionId::new());
        (surface, move |arguments| {
            attachment_command(&status, arguments, &mut drafts)
        })
    }

    pub(crate) fn register_input(
        &self,
        invocation: SubagentInvocationId,
        alias: String,
        ingress: InteractiveIngress,
    ) {
        if let Some(status) = self.status.upgrade() {
            let mut locked = status.lock().expect("surface status lock");
            if Weak::ptr_eq(
                &locked.background_children.owner,
                &Arc::downgrade(&self.owner),
            ) {
                locked
                    .background_children
                    .inputs
                    .entry(invocation)
                    .or_insert(TaskInput {
                        alias,
                        ingress,
                        transcript: String::new(),
                        settled: false,
                    });
            }
        }
    }

    pub(crate) fn seed_conversation(&self, invocation: SubagentInvocationId, state: &SessionState) {
        let mut items: Vec<_> = state.items.values().collect();
        items.sort_by_key(|item| item.seq);
        let transcript = items
            .into_iter()
            .filter_map(|item| match item.kind {
                ego_core::state::ItemKind::UserPrompt => Some(format!("You: {}\n", item.text)),
                ego_core::state::ItemKind::Assistant => Some(format!("{}\n", item.text)),
                ego_core::state::ItemKind::ToolCall => item
                    .tool_name
                    .as_ref()
                    .map(|name| format!("[tool {name}]\n")),
                _ => None,
            })
            .collect::<String>();
        if let Some(status) = self.status.upgrade() {
            let mut locked = status.lock().expect("surface status lock");
            if Weak::ptr_eq(
                &locked.background_children.owner,
                &Arc::downgrade(&self.owner),
            ) && let Some(input) = locked.background_children.inputs.get_mut(&invocation)
            {
                input.transcript = transcript;
            }
        }
    }

    pub(crate) fn handle(&self, invocation: SubagentInvocationId) -> BackgroundActivity {
        BackgroundActivity {
            owner: Arc::downgrade(&self.owner),
            status: self.status.clone(),
            invocation,
        }
    }

    pub(crate) fn register(
        &self,
        invocation: SubagentInvocationId,
        name: &str,
        elapsed: Duration,
        output_tokens: u64,
        recovered: bool,
    ) {
        let handle = self.handle(invocation);
        handle.change(|rows| {
            rows.entry(invocation).or_insert_with(|| BackgroundRow {
                profile: None,
                name: bounded_text(name, 80),
                activity: if recovered { "Recovering" } else { "Working" }.into(),
                started: Instant::now(),
                elapsed_before: elapsed,
                output_tokens,
            });
        });
    }
}

impl Drop for BackgroundTaskSurface {
    fn drop(&mut self) {
        if let Some(status) = self.status.upgrade() {
            let mut locked = status.lock().expect("surface status lock");
            if Weak::ptr_eq(
                &locked.background_children.owner,
                &Arc::downgrade(&self.owner),
            ) {
                locked.background_children = BackgroundRows::default();
            }
            drop(locked);
            repaint_status(&status);
        }
    }
}

impl BackgroundActivity {
    fn change(&self, change: impl FnOnce(&mut BTreeMap<SubagentInvocationId, BackgroundRow>)) {
        let Some(_owner) = self.owner.upgrade() else {
            return;
        };
        let Some(status) = self.status.upgrade() else {
            return;
        };
        {
            let mut locked = status.lock().expect("surface status lock");
            if !Weak::ptr_eq(&locked.background_children.owner, &self.owner) {
                return;
            }
            change(&mut locked.background_children.rows);
        }
        // Never hold the status lock while requesting a paint.
        repaint_status(&status);
    }

    pub(crate) fn profile(&self, profile: &ego_core::model_catalogue::ExecutionProfile) {
        self.change(|rows| {
            if let Some(row) = rows.get_mut(&self.invocation) {
                row.profile = Some(profile.clone());
            }
        });
    }

    pub(crate) fn progress(&self, activity: Option<&str>, output_tokens: u64) {
        self.change(|rows| {
            if let Some(row) = rows.get_mut(&self.invocation) {
                if let Some(activity) = activity {
                    row.activity = bounded_text(activity, 256);
                }
                row.output_tokens = output_tokens;
            }
        });
    }

    pub(crate) fn conversation(&self, event: &ego_core::public::PublicEvent) {
        use ego_core::public::{ItemKind, PublicEvent};
        let text = match event {
            PublicEvent::ItemCompleted {
                kind: ItemKind::UserMessage,
                text: Some(text),
                ..
            } => format!("\nYou: {text}\n"),
            PublicEvent::ItemDelta {
                kind: ItemKind::AgentMessage,
                text,
                ..
            } => text.clone(),
            PublicEvent::ItemStarted {
                kind: ItemKind::Tool,
                name: Some(name),
                ..
            } => {
                format!("\n[tool {name}]\n")
            }
            _ => return,
        };
        let Some(status) = self.status.upgrade() else {
            return;
        };
        let visible = {
            let mut locked = status.lock().expect("surface status lock");
            if !Weak::ptr_eq(&locked.background_children.owner, &self.owner) {
                return;
            }
            let rows = &mut locked.background_children;
            let visible = rows.attached == Some(self.invocation);
            if let Some(input) = rows.inputs.get_mut(&self.invocation) {
                input.transcript.push_str(&text);
                visible.then(|| input.alias.clone())
            } else {
                None
            }
        };
        if let Some(alias) = visible {
            commit_to_transcript(
                &status,
                &format!("[{alias}] {}", ego_core::permit::terminal_safe(&text)),
            );
        }
    }

    pub(crate) fn settled(&self) {
        if let Some(status) = self.status.upgrade() {
            let mut locked = status.lock().expect("surface status lock");
            if Weak::ptr_eq(&locked.background_children.owner, &self.owner)
                && let Some(input) = locked.background_children.inputs.get_mut(&self.invocation)
            {
                input.settled = true;
            }
        }
        self.change(|rows| {
            rows.remove(&self.invocation);
        });
    }
}

/// Only the terminal owner changes the input target. Settlement never redirects
/// a prompt back into the parent: the user must explicitly detach.
pub(super) fn attached_input(status: &SharedStatus) -> Option<InteractiveIngress> {
    let locked = status.lock().expect("surface status lock");
    let rows = &locked.background_children;
    rows.attached
        .and_then(|id| rows.inputs.get(&id))
        .map(|input| input.ingress.clone())
}

/// Returns true only for a terminal-local attachment command. Listing and
/// cancellation remain with the durable task owner.
pub(super) fn attachment_command(
    status: &SharedStatus,
    arguments: &str,
    drafts: &mut DraftStash,
) -> bool {
    let words: Vec<_> = arguments.split_whitespace().collect();
    let [selected] = words.as_slice() else {
        return false;
    };
    let message = {
        let mut locked = status.lock().expect("surface status lock");
        let rows = &mut locked.background_children;
        if *selected == "/detach" {
            rows.attached = None;
            if let Some(parent) = rows.parent_drafts.take() {
                drafts.activate(parent);
            }
            "Returned to parent; child execution is unchanged.".to_owned()
        } else {
            let found = rows
                .inputs
                .iter()
                .find(|(id, input)| input.alias == *selected || id.to_string() == *selected)
                .map(|(id, input)| {
                    (
                        *id,
                        input.alias.clone(),
                        input.transcript.clone(),
                        input.settled,
                    )
                });
            let Some((id, alias, transcript, settled)) = found else {
                return false;
            };
            if settled {
                format!("{alias} completed; no live input.\n{transcript}")
            } else {
                if rows.parent_drafts.is_none() {
                    rows.parent_drafts = Some(drafts.active());
                }
                drafts.activate(
                    id.to_string()
                        .parse()
                        .expect("invocation UUID is a session draft namespace"),
                );
                rows.attached = Some(id);
                format!(
                    "Attached to {alias}; messages go only to this child. /detach returns to parent.\n{transcript}"
                )
            }
        }
    };
    commit_to_transcript(status, &ego_core::permit::terminal_safe(&message));
    repaint_status(status);
    true
}

impl Seat {
    pub(crate) fn background_task_surface(&self) -> Option<BackgroundTaskSurface> {
        match &self.kind {
            SeatKind::Live(live) => Some(BackgroundTaskSurface::attach(&live.status)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ego_core::event::id::TurnId;
    use ego_core::public::PublicEvent;
    fn render(status: &SharedStatus) -> String {
        Waiting::new(Arc::clone(status))
            .render_prompt_indicator(PromptEditMode::Emacs)
            .into_owned()
    }

    // Catches: child settlement redirecting later human text into the parent,
    // or attachment selecting the parent's saved composer drafts.
    #[test]
    fn settled_attachment_keeps_closed_child_target_until_explicit_detach() {
        let status = Arc::new(Mutex::new(SurfaceStatus::default()));
        let surface = BackgroundTaskSurface::attach(&status);
        let parent = SessionId::new();
        let id = SubagentInvocationId::new();
        let (ingress, control) = ego_core::turn::interactive_control();
        surface.register_input(id, "t1".into(), ingress);
        let mut drafts = DraftStash::new(parent);
        drafts.push_active(ComposerDraft {
            images: crate::repl::ImageDraft::default(),
            text: "parent draft".into(),
            cursor: 12,
            origin: InputOrigin::Typed,
        });
        assert!(attachment_command(&status, "t1", &mut drafts));
        assert_ne!(drafts.active(), parent);
        assert_eq!(drafts.active_count(), 0);
        surface.handle(id).settled();
        drop(control);
        let selected =
            attached_input(&status).expect("settlement cannot return the input seat to parent");
        assert_eq!(
            futures::executor::block_on(selected.submit("child-only message".into(), Vec::new())),
            Err(ego_core::turn::ControlError::Closed)
        );
        assert!(attachment_command(&status, "/detach", &mut drafts));
        assert!(attached_input(&status).is_none());
        assert_eq!(drafts.active(), parent);
        assert_eq!(drafts.pop_active().unwrap().text, "parent draft");
    }

    // Catches: an acknowledged human directive replayed after its answer by
    // terminal-thread acknowledgement timing instead of child journal order.
    #[test]
    fn attachment_replays_durable_user_and_child_output_in_order() {
        let status = Arc::new(Mutex::new(SurfaceStatus::default()));
        let surface = BackgroundTaskSurface::attach(&status);
        let id = SubagentInvocationId::new();
        let (ingress, _control) = ego_core::turn::interactive_control();
        surface.register_input(id, "t1".into(), ingress);
        let child = surface.handle(id);
        let turn = TurnId::new();
        for event in [
            PublicEvent::ItemCompleted {
                turn,
                item: ItemId::new(),
                kind: ego_core::public::ItemKind::UserMessage,
                ok: true,
                text: Some("directive".into()),
            },
            PublicEvent::ItemDelta {
                turn,
                item: ItemId::new(),
                kind: ego_core::public::ItemKind::AgentMessage,
                text: "answer".into(),
            },
        ] {
            ego_core::sink::EventSink::publish(&child, &event, &SessionState::default());
        }
        assert_eq!(
            status.lock().unwrap().background_children.inputs[&id].transcript,
            "\nYou: directive\nanswer"
        );
    }

    // Catches: parent settlement removes background rows, or same-name child usage is merged.
    #[test]
    fn durable_activity_keeps_identity_usage_and_elapsed_across_parent_settlement() {
        let status = Arc::new(Mutex::new(SurfaceStatus {
            width: 100,
            ..SurfaceStatus::default()
        }));
        let surface = BackgroundTaskSurface::attach(&status);
        let first = SubagentInvocationId::new();
        let second = SubagentInvocationId::new();
        surface.register(first, "Same child", Duration::from_secs(86), 9, true);
        surface.register(second, "Same child", Duration::ZERO, 0, false);
        let first = surface.handle(first);
        let second = surface.handle(second);
        first.progress(Some("read ledger.rs"), 51_200);
        second.progress(Some("Thinking"), 7);
        {
            let mut state = status.lock().unwrap();
            state.begin_activity();
            state.observe_generation_progress(&PublicEvent::TurnCompleted {
                turn: TurnId::new(),
                ok: false,
                exit_reason: ego_core::event::domain::ExitReason::Cancelled(CancelCause::User),
            });
            state.activity = false;
        }
        let rendered = render(&status);
        let rows: Vec<_> = rendered
            .lines()
            .filter(|line| line.contains("Same child"))
            .collect();
        assert_eq!(rows.len(), 2, "{rendered}");
        assert!(
            rows.iter()
                .any(|line| line.contains("read ledger.rs")
                    && line.ends_with("1m 26s · ↓ 51.2k tokens")),
            "{rendered}"
        );
        assert!(
            rows.iter()
                .any(|line| line.contains("Thinking") && line.ends_with("0s · ↓ 7 tokens")),
            "{rendered}"
        );
        assert_ne!(rows[0].split(']').next(), rows[1].split(']').next());
        assert!(!rendered.contains('\x1b'), "plain output has escapes");
        first.progress(None, 3); // corrected cumulative usage replaces, not adds.
        second.settled();
        let corrected = render(&status);
        assert!(
            corrected.contains("↓ 3 tokens") && !corrected.contains("51.2k"),
            "{corrected}"
        );
        assert_eq!(
            corrected
                .lines()
                .filter(|line| line.contains("Same child"))
                .count(),
            1
        );
    }

    // Catches: delayed progress resurrects a settled row or a stale session attachment.
    #[test]
    fn durable_activity_rejects_late_progress_after_settlement_and_reattachment() {
        let status = Arc::new(Mutex::new(SurfaceStatus {
            width: 80,
            ..SurfaceStatus::default()
        }));
        let old = BackgroundTaskSurface::attach(&status);
        let id = SubagentInvocationId::new();
        old.register(id, "Old child", Duration::ZERO, 0, false);
        let delayed = old.handle(id);
        delayed.settled();
        delayed.progress(Some("late"), 99);
        assert!(!render(&status).contains("Old child"));
        let next = BackgroundTaskSurface::attach(&status);
        next.register(id, "Recovered child", Duration::from_secs(5), 2, true);
        delayed.progress(Some("wrong attachment"), 99);
        drop(old);
        let rendered = render(&status);
        assert!(
            rendered.contains("Recovered child")
                && rendered.contains("Recovering")
                && rendered.contains("5s · ↓ 2 tokens"),
            "{rendered}"
        );
        drop(next);
        assert!(!render(&status).contains("Recovered child"));
    }
    // Catches: many children consume the composer area or untrusted labels emit terminal controls.
    #[test]
    fn durable_activity_viewport_is_bounded_and_plain_text_is_terminal_safe() {
        let status = Arc::new(Mutex::new(SurfaceStatus {
            width: 40,
            ..SurfaceStatus::default()
        }));
        let surface = BackgroundTaskSurface::attach(&status);
        for _ in 0..32 {
            let id = SubagentInvocationId::new();
            surface.register(id, "child\x1b[2J\n界", Duration::ZERO, 4, false);
            surface
                .handle(id)
                .progress(Some(&"reading 界\r\n".repeat(100)), 4);
        }
        let rendered = render(&status);
        assert!(
            rendered.contains("more background tasks") && rendered.contains("/tasks"),
            "{rendered}"
        );
        let height = crossterm::terminal::size()
            .map(|(_, height)| usize::from(height))
            .unwrap_or(24);
        assert!(rendered.lines().count() <= height / 2 + 1, "{rendered}");
        assert!(
            !rendered.contains('\x1b') && !rendered.contains('\r'),
            "{rendered:?}"
        );
        for line in rendered.lines() {
            assert!(crate::render::Line::plain(line).width() <= 39, "{line:?}");
        }
    }
}
