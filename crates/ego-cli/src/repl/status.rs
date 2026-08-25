//! Immutable facts and pure rendering for the interactive status row.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use ego_core::config::{Mode, ModelRef};
use ego_core::cost::CumulativeCost;
use ego_core::event::id::SessionId;
use ego_core::generator::types::ThinkingLevel;
use ego_core::state::{QueueStatus, SessionState};
use unicode_width::UnicodeWidthStr;

const SEPARATOR: &str = " ";

/// Facts selected by the operator rather than reduced from the event log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionFacts {
    model: ModelRef,
    effort: ThinkingLevel,
    mode: Mode,
}

impl SelectionFacts {
    /// Build one typed selection projection.
    pub fn new(model: ModelRef, effort: ThinkingLevel, mode: Mode) -> Self {
        Self {
            model,
            effort,
            mode,
        }
    }
}

/// Facts owned by the live interactive seat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatFacts {
    queue: QueueStatus,
    drafts: usize,
    cwd: String,
    modal: bool,
}

impl SeatFacts {
    /// Build a seat projection outside a modal selector.
    pub fn new(queue: QueueStatus, drafts: usize, cwd: impl Into<String>) -> Self {
        Self {
            queue,
            drafts,
            cwd: cwd.into(),
            modal: false,
        }
    }

    /// State whether a selector currently owns keyboard focus.
    pub fn with_modal(mut self, modal: bool) -> Self {
        self.modal = modal;
        self
    }
}

/// Git data collected outside the input and rendering paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitFacts {
    branch: String,
    dirty: bool,
}

impl GitFacts {
    /// Build the small Git projection consumed by the status surface.
    pub fn new(branch: impl Into<String>, dirty: bool) -> Self {
        Self {
            branch: branch.into(),
            dirty,
        }
    }

    /// Current branch name, or `detached` when Git reports no branch.
    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// Whether the porcelain snapshot contains a changed path.
    pub fn dirty(&self) -> bool {
        self.dirty
    }

    fn display(&self) -> String {
        if self.dirty {
            format!("{}*", self.branch)
        } else {
            self.branch.clone()
        }
    }
}

impl From<ego_core::config::StatusSegmentName> for StatusSegmentName {
    fn from(name: ego_core::config::StatusSegmentName) -> Self {
        match name {
            ego_core::config::StatusSegmentName::Model => Self::Model,
            ego_core::config::StatusSegmentName::Effort => Self::Effort,
            ego_core::config::StatusSegmentName::Mode => Self::Mode,
            ego_core::config::StatusSegmentName::Context => Self::Context,
            ego_core::config::StatusSegmentName::Cost => Self::Cost,
            ego_core::config::StatusSegmentName::Queue => Self::Queue,
            ego_core::config::StatusSegmentName::Drafts => Self::Drafts,
            ego_core::config::StatusSegmentName::Interrupt => Self::Interrupt,
            ego_core::config::StatusSegmentName::Cwd => Self::Cwd,
            ego_core::config::StatusSegmentName::Git => Self::Git,
        }
    }
}

/// Consumer-owned port for refreshing Git facts away from the input thread.
pub trait GitFactsSource: Send + Sync {
    /// Collect one complete replacement value.
    fn collect(&self) -> Result<GitFacts, String>;
}

/// A fixed, no-shell Git invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCommand {
    program: String,
    arguments: Vec<String>,
    environment: HashMap<String, String>,
}

impl GitCommand {
    /// Build the hardened porcelain-status invocation for `cwd`.
    pub fn status(cwd: impl AsRef<Path>) -> Self {
        let cwd = cwd.as_ref().to_string_lossy().into_owned();
        let arguments = [
            "--no-pager",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-C",
            cwd.as_str(),
            "status",
            "--porcelain=v2",
            "-b",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let environment = [
            ("GIT_OPTIONAL_LOCKS", "0"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_CONFIG_SYSTEM", "/dev/null"),
            ("GIT_PAGER", "cat"),
            ("LC_ALL", "C"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect();
        Self {
            program: "git".to_owned(),
            arguments,
            environment,
        }
    }

    /// The executable, never a shell.
    pub fn program(&self) -> &str {
        &self.program
    }

    /// The complete fixed-shape argument vector.
    pub fn arguments(&self) -> &[String] {
        &self.arguments
    }

    /// Read one explicitly controlled environment value.
    pub fn env(&self, name: &str) -> Option<&str> {
        self.environment.get(name).map(String::as_str)
    }

    /// This adapter never delegates parsing or execution to a shell.
    pub fn uses_shell(&self) -> bool {
        false
    }

    fn execute(&self) -> Result<String, String> {
        let mut command = Command::new(&self.program);
        command.env_clear();
        command.args(&self.arguments);
        for (name, value) in &self.environment {
            command.env(name, value);
        }
        let output = command
            .output()
            .map_err(|error| format!("could not start git: {error}"))?;
        if !output.status.success() {
            return Err(format!("git exited with {}", output.status));
        }
        String::from_utf8(output.stdout).map_err(|error| format!("git returned non-UTF-8: {error}"))
    }
}

impl GitFactsSource for GitCommand {
    fn collect(&self) -> Result<GitFacts, String> {
        let output = self.execute()?;
        let branch = output
            .lines()
            .find_map(|line| line.strip_prefix("# branch.head "))
            .filter(|branch| *branch != "(detached)")
            .unwrap_or("detached");
        let dirty = output.lines().any(|line| {
            line.starts_with("1 ")
                || line.starts_with("2 ")
                || line.starts_with("u ")
                || line.starts_with("? ")
        });
        Ok(GitFacts::new(branch, dirty))
    }
}

/// One immutable set of facts shared by status and action availability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceSnapshot {
    revision: String,
    model: String,
    effort: String,
    mode: String,
    context: String,
    cost: String,
    queue: QueueStatus,
    drafts: usize,
    cwd: String,
    git: Option<GitFacts>,
    git_failed: bool,
    modal: bool,
}

impl SurfaceSnapshot {
    /// Project all status facts once, keeping context and cost owned by state.
    pub fn from_state(
        session: SessionId,
        state: &SessionState,
        selection: SelectionFacts,
        seat: SeatFacts,
        git_branch: Option<String>,
    ) -> Self {
        Self::from_state_with_git(
            session,
            state,
            selection,
            seat,
            git_branch.map(|branch| GitFacts::new(branch, false)),
        )
    }

    /// Project a snapshot from the typed Git adapter result.
    pub fn from_state_with_git(
        session: SessionId,
        state: &SessionState,
        selection: SelectionFacts,
        seat: SeatFacts,
        git: Option<GitFacts>,
    ) -> Self {
        let context = state
            .context
            .used_tokens
            .map_or_else(|| "unknown".to_owned(), |tokens| tokens.to_string());
        let cost = match &state.cost {
            CumulativeCost::Unstarted => "unstarted".to_owned(),
            CumulativeCost::Unknown(_) => "unknown".to_owned(),
            CumulativeCost::Known { total } => format!("{} {}", total.amount, total.currency),
        };
        Self {
            revision: session.to_string(),
            model: selection.model.0,
            effort: selection.effort.as_str().to_owned(),
            mode: selection.mode.as_str().to_owned(),
            context,
            cost,
            queue: seat.queue,
            drafts: seat.drafts,
            cwd: seat.cwd,
            git,
            git_failed: false,
            modal: seat.modal,
        }
    }

    /// Replace the live queue projection before publishing a new snapshot.
    pub fn set_queue(&mut self, queue: QueueStatus) {
        self.queue = queue;
    }

    /// Replace the local draft count before publishing a new snapshot.
    pub fn set_drafts(&mut self, drafts: usize) {
        self.drafts = drafts;
    }

    /// Replace the live generation choice without re-deriving session facts.
    pub fn set_selection(&mut self, model: ModelRef, effort: ThinkingLevel) {
        self.model = model.0;
        self.effort = effort.as_str().to_owned();
    }

    /// Replace selector focus before publishing a new surface projection.
    pub fn set_modal(&mut self, modal: bool) {
        self.modal = modal;
    }

    /// Replace the working directory before publishing a new snapshot.
    pub fn set_cwd(&mut self, cwd: impl Into<String>) {
        self.cwd = cwd.into();
    }

    /// Install the latest successful asynchronous Git projection.
    pub fn set_git(&mut self, git: GitFacts) {
        self.git = Some(git);
        self.git_failed = false;
    }

    /// Record a failed asynchronous Git projection without moving other facts.
    pub fn set_git_failed(&mut self) {
        self.git = None;
        self.git_failed = true;
    }

    /// Return a replacement snapshot recording a failed Git refresh.
    pub fn without_git(mut self) -> Self {
        self.git = None;
        self.git_failed = true;
        self
    }

    /// Return a replacement snapshot carrying a successful Git refresh.
    pub fn with_git(mut self, git: GitFacts) -> Self {
        self.git = Some(git);
        self.git_failed = false;
        self
    }

    /// Selected model spelling.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Selected reasoning effort spelling.
    pub fn effort(&self) -> &str {
        &self.effort
    }

    /// Selected permission mode spelling.
    pub fn mode(&self) -> &str {
        &self.mode
    }

    /// Authoritative context occupancy spelling.
    pub fn context(&self) -> &str {
        &self.context
    }

    /// Authoritative cumulative cost spelling.
    pub fn cost(&self) -> &str {
        &self.cost
    }

    /// Number of accepted pending steers.
    pub fn pending_steers(&self) -> usize {
        self.queue.pending_steers
    }

    /// Whether an interruption is durably pending.
    pub fn interrupt_pending(&self) -> bool {
        self.queue.interrupt_pending
    }

    /// Project action availability from these exact facts.
    pub fn action_snapshot(&self) -> ActionSnapshot {
        ActionSnapshot {
            busy: self.queue.pending_steers > 0
                || self.queue.queued > 0
                || self.queue.interrupt_pending,
            modal: self.modal,
        }
    }

    fn value(&self, name: &StatusSegmentName) -> Option<SegmentValue> {
        match name {
            StatusSegmentName::Model => Some(SegmentValue::text(&self.model)),
            StatusSegmentName::Effort => Some(SegmentValue::text(&self.effort)),
            StatusSegmentName::Mode => Some(SegmentValue::text(&self.mode)),
            StatusSegmentName::Context => Some(SegmentValue::text(&self.context)),
            StatusSegmentName::Cost => Some(SegmentValue::text(&self.cost)),
            StatusSegmentName::Queue => Some(SegmentValue::text(format!(
                "{}/{}",
                self.queue.pending_steers, self.queue.queued
            ))),
            StatusSegmentName::Drafts => Some(SegmentValue::text(self.drafts.to_string())),
            StatusSegmentName::Interrupt => {
                Some(SegmentValue::text(if self.queue.interrupt_pending {
                    "yes"
                } else {
                    "no"
                }))
            }
            StatusSegmentName::Cwd => Some(SegmentValue::text(&self.cwd)),
            StatusSegmentName::Git => self
                .git
                .as_ref()
                .map(|facts| SegmentValue::text(facts.display())),
            StatusSegmentName::Custom(_) => None,
        }
    }

    fn active(&self, name: &StatusSegmentName) -> bool {
        match name {
            StatusSegmentName::Queue => self.queue.pending_steers > 0 || self.queue.queued > 0,
            StatusSegmentName::Drafts => self.drafts > 0,
            StatusSegmentName::Interrupt => self.queue.interrupt_pending,
            StatusSegmentName::Cost => self.cost != "unstarted",
            StatusSegmentName::Git => self.git.is_some(),
            _ => self.value(name).is_some(),
        }
    }
}

/// Action availability derived from one surface snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionSnapshot {
    busy: bool,
    modal: bool,
}

impl ActionSnapshot {
    /// Whether the named action may dispatch against this snapshot.
    pub fn can_dispatch(&self, action: &str) -> bool {
        if self.modal {
            return matches!(action, "answer" | "cancel");
        }
        match action {
            "model" | "statusline" | "help" | "usage" => true,
            "quit" | "new" | "resume" | "compact" => !self.busy,
            _ => !self.busy,
        }
    }
}

/// A configured built-in or extension status segment.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum StatusSegmentName {
    Model,
    Effort,
    Mode,
    Context,
    Cost,
    Queue,
    Drafts,
    Interrupt,
    Cwd,
    Git,
    Custom(String),
}

impl StatusSegmentName {
    /// Build an extension segment name without restricting its vocabulary.
    pub fn custom(name: impl Into<String>) -> Self {
        Self::Custom(name.into())
    }

    fn label(&self) -> &str {
        match self {
            Self::Model => "model",
            Self::Effort => "effort",
            Self::Mode => "mode",
            Self::Context => "context",
            Self::Cost => "cost",
            Self::Queue => "queue",
            Self::Drafts => "drafts",
            Self::Interrupt => "interrupt",
            Self::Cwd => "cwd",
            Self::Git => "git",
            Self::Custom(name) => name,
        }
    }

    fn critical(&self) -> bool {
        matches!(self, Self::Queue | Self::Drafts | Self::Interrupt)
    }

    fn optional_priority(&self) -> usize {
        match self {
            Self::Git | Self::Custom(_) => 0,
            Self::Cost => 1,
            Self::Cwd => 2,
            Self::Mode => 3,
            Self::Effort => 4,
            Self::Context => 5,
            Self::Model => usize::MAX,
            Self::Queue | Self::Drafts | Self::Interrupt => usize::MAX,
        }
    }
}

/// A complete semantic segment value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentValue(String);

impl SegmentValue {
    /// Build a text value that is either retained whole or omitted whole.
    pub fn text(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

/// A status-rendering failure that does not stop the interactive seat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusDiagnostic {
    WidthUnavailable,
    GitUnavailable,
    CriticalOverflow,
}

/// Conversion accepted by the pure renderer for known and unknown widths.
pub trait StatusWidth {
    /// Convert into a known display-cell count, or `None` when unavailable.
    fn status_width(self) -> Option<usize>;
}

impl StatusWidth for usize {
    fn status_width(self) -> Option<usize> {
        Some(self)
    }
}

impl StatusWidth for u16 {
    fn status_width(self) -> Option<usize> {
        Some(usize::from(self))
    }
}

impl StatusWidth for i32 {
    fn status_width(self) -> Option<usize> {
        usize::try_from(self).ok()
    }
}

impl StatusWidth for Option<usize> {
    fn status_width(self) -> Option<usize> {
        self
    }
}

#[derive(Debug, Clone)]
struct RenderedSegment {
    name: StatusSegmentName,
    text: String,
    active: bool,
}

/// One width-bounded status row plus omission evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusLine {
    text: String,
    width: usize,
    omitted: Vec<StatusSegmentName>,
    diagnostics: Vec<StatusDiagnostic>,
    snapshot_revision: String,
}

impl StatusLine {
    /// Render built-ins in configured order from one immutable snapshot.
    pub fn render<W: StatusWidth>(
        snapshot: &SurfaceSnapshot,
        configured: &[StatusSegmentName],
        width: W,
    ) -> Self {
        Self::render_with_values(snapshot, configured, &[], width)
    }

    /// Render built-ins and caller-supplied extension values through one path.
    pub fn render_with_values<W: StatusWidth>(
        snapshot: &SurfaceSnapshot,
        configured: &[StatusSegmentName],
        values: &[(StatusSegmentName, SegmentValue)],
        width: W,
    ) -> Self {
        let mut diagnostics = Vec::new();
        let Some(limit) = width.status_width().filter(|width| *width > 0) else {
            diagnostics.push(StatusDiagnostic::WidthUnavailable);
            return Self::empty(snapshot, configured.to_vec(), diagnostics);
        };

        let mut omitted = Vec::new();
        let mut segments = Vec::new();
        for name in configured {
            let value = snapshot.value(name).or_else(|| {
                values
                    .iter()
                    .find(|(candidate, _)| candidate == name)
                    .map(|(_, value)| value.clone())
            });
            match value {
                Some(value) => segments.push(RenderedSegment {
                    name: name.clone(),
                    text: format!("{}={}", name.label(), value.0),
                    active: snapshot.active(name) || matches!(name, StatusSegmentName::Custom(_)),
                }),
                None => omitted.push(name.clone()),
            }
        }
        if snapshot.git_failed && configured.contains(&StatusSegmentName::Git) {
            diagnostics.push(StatusDiagnostic::GitUnavailable);
        }

        if segments_width(&segments) > limit {
            remove_inactive(&mut segments, &mut omitted, limit);
        }
        if segments_width(&segments) > limit {
            remove_optional(&mut segments, &mut omitted, limit);
        }
        let active_critical_before = segments
            .iter()
            .any(|segment| segment.active && segment.name.critical());
        if segments_width(&segments) > limit {
            remove_critical(&mut segments, &mut omitted, limit);
        }
        if segments_width(&segments) > limit {
            remove_remaining(&mut segments, &mut omitted, limit);
        }
        let active_critical_omitted = active_critical_before
            && omitted.iter().any(|name| {
                name.critical()
                    && configured.iter().any(|configured| configured == name)
                    && snapshot.active(name)
            });
        if active_critical_omitted {
            diagnostics.push(StatusDiagnostic::CriticalOverflow);
        }

        let text = segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect::<Vec<_>>()
            .join(SEPARATOR);
        Self {
            width: UnicodeWidthStr::width(text.as_str()),
            text,
            omitted,
            diagnostics,
            snapshot_revision: snapshot.revision.clone(),
        }
    }

    fn empty(
        snapshot: &SurfaceSnapshot,
        omitted: Vec<StatusSegmentName>,
        diagnostics: Vec<StatusDiagnostic>,
    ) -> Self {
        Self {
            text: String::new(),
            width: 0,
            omitted,
            diagnostics,
            snapshot_revision: snapshot.revision.clone(),
        }
    }

    /// Rendered text without a trailing newline.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Unicode display-cell width of [`Self::text`].
    pub fn width(&self) -> usize {
        self.width
    }

    /// Whether a configured segment was omitted as a whole.
    pub fn omitted(&self, name: StatusSegmentName) -> bool {
        self.omitted.contains(&name)
    }

    /// Non-fatal diagnostics raised while producing this row.
    pub fn diagnostics(&self) -> &[StatusDiagnostic] {
        &self.diagnostics
    }

    /// Number of critical-overflow diagnostics, always zero or one.
    pub fn overflow_diagnostics(&self) -> usize {
        self.diagnostics
            .iter()
            .filter(|diagnostic| matches!(diagnostic, StatusDiagnostic::CriticalOverflow))
            .count()
    }

    /// Identity of the immutable snapshot used for this render.
    pub fn snapshot_revision(&self) -> &str {
        &self.snapshot_revision
    }
}

fn segments_width(segments: &[RenderedSegment]) -> usize {
    let contents = segments
        .iter()
        .map(|segment| UnicodeWidthStr::width(segment.text.as_str()))
        .sum::<usize>();
    contents.saturating_add(segments.len().saturating_sub(1))
}

fn remove_inactive(
    segments: &mut Vec<RenderedSegment>,
    omitted: &mut Vec<StatusSegmentName>,
    limit: usize,
) {
    let mut index = segments.len();
    while index > 0 && segments_width(segments) > limit {
        index -= 1;
        if !segments[index].active && !matches!(segments[index].name, StatusSegmentName::Model) {
            omitted.push(segments.remove(index).name);
        }
    }
}

fn remove_optional(
    segments: &mut Vec<RenderedSegment>,
    omitted: &mut Vec<StatusSegmentName>,
    limit: usize,
) {
    let mut priorities = segments
        .iter()
        .map(|segment| segment.name.optional_priority())
        .filter(|priority| *priority != usize::MAX)
        .collect::<Vec<_>>();
    priorities.sort_unstable();
    priorities.dedup();
    for priority in priorities {
        let mut index = segments.len();
        while index > 0 && segments_width(segments) > limit {
            index -= 1;
            if segments[index].name.optional_priority() == priority {
                omitted.push(segments.remove(index).name);
            }
        }
    }
}

fn remove_critical(
    segments: &mut Vec<RenderedSegment>,
    omitted: &mut Vec<StatusSegmentName>,
    limit: usize,
) {
    for name in [
        StatusSegmentName::Drafts,
        StatusSegmentName::Queue,
        StatusSegmentName::Interrupt,
    ] {
        if segments_width(segments) <= limit {
            break;
        }
        if let Some(index) = segments.iter().position(|segment| segment.name == name) {
            omitted.push(segments.remove(index).name);
        }
    }
}

fn remove_remaining(
    segments: &mut Vec<RenderedSegment>,
    omitted: &mut Vec<StatusSegmentName>,
    limit: usize,
) {
    while segments_width(segments) > limit {
        let Some(segment) = segments.pop() else {
            break;
        };
        omitted.push(segment.name);
    }
}
