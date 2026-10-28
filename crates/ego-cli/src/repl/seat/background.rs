//! Disposable durable-child projection; Reedline remains the only terminal writer.

use super::*;
use ego_core::event::id::SubagentInvocationId;
use std::collections::BTreeMap;
use std::sync::Weak;

#[derive(Default)]
pub(super) struct BackgroundRows {
    owner: Weak<()>,
    pub(super) rows: BTreeMap<SubagentInvocationId, BackgroundRow>,
}

pub(super) struct BackgroundRow {
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

impl BackgroundTaskSurface {
    fn attach(status: &SharedStatus) -> Self {
        let owner = Arc::new(());
        status
            .lock()
            .expect("surface status lock")
            .background_children = BackgroundRows {
            owner: Arc::downgrade(&owner),
            rows: BTreeMap::new(),
        };
        repaint_status(status);
        Self {
            owner,
            status: Arc::downgrade(status),
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
        self.handle(SubagentInvocationId::new())
            .change(BTreeMap::clear);
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

    pub(crate) fn settled(&self) {
        self.change(|rows| {
            rows.remove(&self.invocation);
        });
    }
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
