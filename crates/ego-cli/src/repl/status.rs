//! Immutable facts and pure rendering for the interactive status row.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use ego_core::config::{Mode, ModelRef, Trust};
use ego_core::cost::{CostAttribution, CostDelta, CumulativeCost, Money, SubscriptionEquivalent};
use ego_core::event::id::SessionId;
use ego_core::generator::types::ThinkingLevel;
use ego_core::state::{QueueStatus, SessionState};
use unicode_width::UnicodeWidthStr;

use crate::render::line::{Line, Span};
use crate::render::style::{Palette, Style};

const SEPARATOR: &str = " · ";

fn compact_cwd(cwd: &str) -> String {
    let home = std::env::var_os("HOME");
    let path = Path::new(cwd);
    let shown = home
        .as_deref()
        .and_then(|home| path.strip_prefix(Path::new(home)).ok())
        .map(|suffix| {
            if suffix.as_os_str().is_empty() {
                "~".to_owned()
            } else {
                format!("~/{}", suffix.display())
            }
        })
        .unwrap_or_else(|| cwd.to_owned());
    let mut parts = shown.split('/').collect::<Vec<_>>();
    let last = parts.len().saturating_sub(1);
    for part in &mut parts[..last] {
        if *part != "~" {
            *part = part
                .char_indices()
                .nth(1)
                .map_or(*part, |(end, _)| &part[..end]);
        }
    }
    parts.join("/")
}

/// Facts selected by the operator rather than reduced from the event log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionFacts {
    fast: bool,
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
            fast: false,
        }
    }
}

impl SelectionFacts {
    pub fn with_fast(mut self, fast: bool) -> Self {
        self.fast = fast;
        self
    }
}

/// Facts owned by the live interactive seat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatFacts {
    queue: QueueStatus,
    drafts: usize,
    cwd: String,
    trust: Trust,
    modal: bool,
}

impl SeatFacts {
    /// Build a seat projection outside a modal selector.
    ///
    /// `trust` is a launch fact rather than a live one — it is answered once,
    /// before the seat exists, and never moves — but it is carried here
    /// because the status row is the only place a silently untrusted session
    /// gets to say so.
    pub fn new(queue: QueueStatus, drafts: usize, cwd: impl Into<String>, trust: Trust) -> Self {
        Self {
            queue,
            drafts,
            cwd: cwd.into(),
            trust,
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
            format!("⎇ {}●", self.branch)
        } else {
            format!("⎇ {}", self.branch)
        }
    }
}

impl From<ego_core::config::StatusSegmentName> for StatusSegmentName {
    fn from(name: ego_core::config::StatusSegmentName) -> Self {
        match name {
            ego_core::config::StatusSegmentName::Model => Self::Model,
            ego_core::config::StatusSegmentName::Effort => Self::Effort,
            ego_core::config::StatusSegmentName::Fast => Self::Fast,
            ego_core::config::StatusSegmentName::Mode => Self::Mode,
            ego_core::config::StatusSegmentName::Context => Self::Context,
            ego_core::config::StatusSegmentName::Cost => Self::Cost,
            ego_core::config::StatusSegmentName::Queue => Self::Queue,
            ego_core::config::StatusSegmentName::Drafts => Self::Drafts,
            ego_core::config::StatusSegmentName::Interrupt => Self::Interrupt,
            ego_core::config::StatusSegmentName::Cwd => Self::Cwd,
            ego_core::config::StatusSegmentName::Git => Self::Git,
            ego_core::config::StatusSegmentName::Trust => Self::Trust,
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
    fast: bool,
    revision: String,
    model: String,
    effort: String,
    mode: String,
    context: String,
    cost: String,
    used_tokens: Option<u64>,
    window: Option<u64>,
    cost_display: Option<String>,
    queue: QueueStatus,
    drafts: usize,
    cwd: String,
    trust: Trust,
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
            fast: selection.fast,
            revision: session.to_string(),
            model: selection.model.0,
            effort: selection.effort.as_str().to_owned(),
            mode: selection.mode.as_str().to_owned(),
            context,
            cost,
            used_tokens: state.context.used_tokens,
            window: state.context.window,
            cost_display: visible_cost(state),
            queue: seat.queue,
            drafts: seat.drafts,
            cwd: compact_cwd(&seat.cwd),
            trust: seat.trust,
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
        self.cwd = compact_cwd(&cwd.into());
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

    /// Fast status variable: empty when standard, `fast` when enabled.
    pub fn fast(&self) -> &str {
        if self.fast { "fast" } else { "" }
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

    fn context_display(&self) -> String {
        let filled = match (self.used_tokens, self.window) {
            (Some(used), Some(window)) if window > 0 => {
                ((u128::from(used) * 10 / u128::from(window)).min(10)) as usize
            }
            _ => 0,
        };
        let used = self
            .used_tokens
            .map(compact_tokens)
            .unwrap_or_else(|| "?".into());
        let window = self
            .window
            .filter(|window| *window > 0)
            .map(compact_tokens)
            .unwrap_or_else(|| "?".into());
        format!(
            "{}{} {used}/{window}",
            "▰".repeat(filled),
            "▱".repeat(10 - filled)
        )
    }

    fn context_style(&self) -> Style {
        match (self.used_tokens, self.window) {
            (Some(used), Some(window)) if window > 0 => {
                let percent = u128::from(used) * 100;
                let window = u128::from(window);
                let colour = if percent >= window * 85 {
                    Palette::ERROR
                } else if percent >= window * 60 {
                    Palette::WARNING
                } else {
                    Palette::ADDED_FG
                };
                Style::fg(colour)
            }
            _ => Style::PLAIN,
        }
    }

    fn value(&self, name: &StatusSegmentName) -> Option<SegmentValue> {
        match name {
            StatusSegmentName::Model => Some(SegmentValue::text(
                self.model.rsplit('/').next().unwrap_or(&self.model),
            )),
            StatusSegmentName::Effort => Some(SegmentValue::text(&self.effort)),
            StatusSegmentName::Fast => self.fast.then(|| SegmentValue::text("fast")),
            StatusSegmentName::Mode => Some(SegmentValue::text(&self.mode)),
            StatusSegmentName::Context => Some(SegmentValue::text(self.context_display())),
            // An unpriced or unstarted session has no figure to show; a
            // `$0` here would claim the spend is known to be nothing.
            StatusSegmentName::Cost => self.cost_display.as_ref().map(SegmentValue::text),
            // Idle counters carry no news, so they do not take a row cell.
            StatusSegmentName::Queue => self.active(name).then(|| {
                SegmentValue::text(format!(
                    "{}/{}",
                    self.queue.pending_steers, self.queue.queued
                ))
            }),
            StatusSegmentName::Drafts => self
                .active(name)
                .then(|| SegmentValue::text(self.drafts.to_string())),
            StatusSegmentName::Interrupt => self.active(name).then(|| SegmentValue::text("yes")),
            StatusSegmentName::Cwd => Some(SegmentValue::text(&self.cwd)),
            // Trusted is the uninteresting answer: the row speaks only when
            // the session runs without ambient authority.
            StatusSegmentName::Trust => {
                (self.trust == Trust::Untrusted).then(|| SegmentValue::text("untrusted"))
            }
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
            StatusSegmentName::Git => self.git.is_some(),
            StatusSegmentName::Trust => self.trust == Trust::Untrusted,
            _ => self.value(name).is_some(),
        }
    }
}

// Policy zeros are budget accounting, not a known per-token currency price.
fn visible_cost(state: &SessionState) -> Option<String> {
    let mut equivalent: Option<Money> = None;
    let mut policies = 0;
    let mut unpriced = matches!(state.cost, CumulativeCost::Unknown(_));
    let mut monetary = false;
    for charge in &state.charges {
        match &charge.cost {
            CostDelta::Known {
                attribution:
                    CostAttribution::MeteringPolicy {
                        equivalent: estimate,
                        ..
                    },
                ..
            } => {
                policies += 1;
                match estimate.as_deref() {
                    Some(SubscriptionEquivalent::V1 { amount, .. }) => {
                        equivalent = match equivalent {
                            Some(total) => match total.checked_add(amount) {
                                Ok(total) => Some(total),
                                Err(_) => {
                                    unpriced = true;
                                    None
                                }
                            },
                            None => Some(*amount),
                        };
                    }
                    None => unpriced = true,
                }
            }
            CostDelta::Known { .. } => monetary = true,
            CostDelta::Unknown(_) => unpriced = true,
        }
    }
    if policies > 0 {
        let estimate = equivalent.map(|amount| format!("≈{}", money_display(&amount)));
        return Some(if unpriced || monetary {
            estimate.map_or_else(|| "unpriced".to_owned(), |text| format!("{text} (partial)"))
        } else {
            estimate.unwrap_or_else(|| "unpriced".to_owned())
        });
    }
    match &state.cost {
        CumulativeCost::Unstarted => None,
        CumulativeCost::Unknown(_) => Some("unpriced".to_owned()),
        CumulativeCost::Known { total } => Some(money_display(total)),
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
    Fast,
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
    Trust,
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
            Self::Fast => "fast",
            Self::Mode => "mode",
            Self::Context => "context",
            Self::Cost => "cost",
            Self::Queue => "queue",
            Self::Drafts => "drafts",
            Self::Interrupt => "interrupt",
            Self::Cwd => "cwd",
            Self::Git => "git",
            Self::Trust => "trust",
            Self::Custom(name) => name,
        }
    }

    /// Whether the value reads on its own, without a `label=` prefix.
    fn self_describing(&self) -> bool {
        matches!(
            self,
            Self::Model
                | Self::Fast
                | Self::Effort
                | Self::Mode
                | Self::Context
                | Self::Cost
                | Self::Cwd
                | Self::Git
        )
    }

    fn critical(&self) -> bool {
        matches!(self, Self::Queue | Self::Drafts | Self::Interrupt)
    }

    fn optional_priority(&self) -> usize {
        match self {
            Self::Git | Self::Custom(_) => 0,
            Self::Cwd => 1,
            Self::Cost => 2,
            Self::Mode => 3,
            Self::Effort | Self::Fast => 4,
            // An untrusted `trust` segment outranks every convenience but the
            // context share: regression case rules that the context field is never
            // the one width pressure removes while another optional field
            // remains, so trust goes first once only the two are left.
            Self::Trust => 5,
            Self::Context => 6,
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
    styled: Line,
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
                    text: if name.self_describing() {
                        value.0
                    } else {
                        format!("{}={}", name.label(), value.0)
                    },
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

        let mut styled = Line::empty();
        for (index, segment) in segments.iter().enumerate() {
            if index > 0 {
                styled.push(Span::plain(segment_separator(
                    &segments[index - 1].name,
                    &segment.name,
                )));
            }
            let style = if segment.name == StatusSegmentName::Context {
                snapshot.context_style()
            } else {
                Style::PLAIN
            };
            styled.push(Span::new(&segment.text, style));
        }
        let text = styled.text();
        Self {
            width: styled.width(),
            text,
            styled,
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
            styled: Line::empty(),
            omitted,
            diagnostics,
            snapshot_revision: snapshot.revision.clone(),
        }
    }

    /// Rendered text without a trailing newline.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Paint the retained context segment using the terminal colour policy.
    pub fn terminal_text(&self, capabilities: &crate::render::Capabilities) -> String {
        self.styled.render(capabilities.depth)
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
    contents.saturating_add(
        segments
            .windows(2)
            .map(|pair| UnicodeWidthStr::width(segment_separator(&pair[0].name, &pair[1].name)))
            .sum::<usize>(),
    )
}

fn segment_separator(left: &StatusSegmentName, right: &StatusSegmentName) -> &'static str {
    use StatusSegmentName::{Context, Cost, Cwd, Git};
    if matches!(left, Context | Cost | Cwd | Git) || matches!(right, Context | Cost | Cwd | Git) {
        " "
    } else {
        SEPARATOR
    }
}

/// `1_000_000` → `1M`, `2_482` → `2.5k`: one decimal, trailing `.0` dropped.
fn compact_tokens(tokens: u64) -> String {
    let (scaled, suffix) = match tokens {
        1_000_000.. => (tokens as f64 / 1_000_000.0, "M"),
        1_000.. => (tokens as f64 / 1_000.0, "k"),
        _ => return tokens.to_string(),
    };
    let text = format!("{scaled:.1}");
    format!("{}{suffix}", text.strip_suffix(".0").unwrap_or(&text))
}

/// Display-only: the exact amount stays in the log.
fn money_display(money: &Money) -> String {
    let amount = money.amount.to_f64_lossy();
    if amount > 0.0 && amount < 0.0005 && money.currency.as_str() == "USD" {
        return "<$0.001".to_owned();
    }
    let places = if amount < 0.1 { 3 } else { 2 };
    match money.currency.as_str() {
        "USD" => format!("${amount:.places$}"),
        code => format!("{amount:.places$} {code}"),
    }
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
