//! contract for the immutable, width-aware interactive status surface.

use ego_cli::repl::status::{
    GitFactsSource, SeatFacts, SegmentValue, SelectionFacts, StatusLine, StatusSegmentName,
    SurfaceSnapshot,
};
use ego_core::config::{Mode, ModelRef, Trust};
use ego_core::cost::{CumulativeCost, UnknownCost};
use ego_core::event::id::SessionId;
use ego_core::generator::types::ThinkingLevel;
use ego_core::state::{QueueStatus, SessionState};
use unicode_width::UnicodeWidthStr;

struct FailingGit;

impl GitFactsSource for FailingGit {
    fn collect(&self) -> Result<ego_cli::repl::status::GitFacts, String> {
        Err("git exited 128".into())
    }
}

fn snapshot() -> SurfaceSnapshot {
    let mut state = SessionState::default();
    state.context.used_tokens = Some(42_000);
    SurfaceSnapshot::from_state(
        SessionId::new(),
        &state,
        SelectionFacts::new(
            ModelRef("openai/gpt-5.6".into()),
            ThinkingLevel::High,
            Mode::Default,
        ),
        SeatFacts::new(QueueStatus::default(), 0, "/workspace/ego", Trust::Trusted),
        Some("main".into()),
    )
}

#[test]
fn builtins_and_arbitrary_configured_segments_render_in_user_order() {
    let names = [
        StatusSegmentName::Model,
        StatusSegmentName::Effort,
        StatusSegmentName::Mode,
        StatusSegmentName::Context,
        StatusSegmentName::Cost,
        StatusSegmentName::Queue,
        StatusSegmentName::Drafts,
        StatusSegmentName::Interrupt,
        StatusSegmentName::Cwd,
        StatusSegmentName::Git,
    ];
    let mut surface = snapshot();
    surface.set_queue(QueueStatus {
        pending_steers: 1,
        queued: 0,
        interrupt_pending: false,
    });
    surface.set_drafts(1);
    let line = StatusLine::render(&surface, &names, 240);
    assert_eq!(line.diagnostics(), &[]);
    assert!(line.text().find("gpt-5.6").unwrap() < line.text().find("high").unwrap());
    assert!(line.text().find("steer ").unwrap() < line.text().find("drafts=").unwrap());
    assert!(line.text().find("/w/ego").unwrap() < line.text().find("⎇").unwrap());

    let custom = StatusSegmentName::custom("latency");
    let custom_line = StatusLine::render_with_values(
        &snapshot(),
        &[
            custom.clone(),
            StatusSegmentName::Model,
            StatusSegmentName::Queue,
        ],
        &[(custom, SegmentValue::text("17ms"))],
        120,
    );
    assert!(custom_line.text().starts_with("latency=17ms · "));
}

// Catches ambiguous queue ratios, swapped counts, or newly visible idle counters.
#[test]
fn rendered_queue_labels_distinguish_steers_from_next_turn() {
    let mut surface = snapshot();
    for (pending_steers, queued, expected) in [
        (1, 2, "steer 1 · next 2"),
        (1, 0, "steer 1 · next 0"),
        (0, 2, "steer 0 · next 2"),
        (0, 0, ""),
    ] {
        surface.set_queue(QueueStatus {
            pending_steers,
            queued,
            interrupt_pending: false,
        });
        let rendered = StatusLine::render(&surface, &[StatusSegmentName::Queue], 80);
        assert_eq!(rendered.text(), expected);
        assert_eq!(rendered.width(), UnicodeWidthStr::width(expected));
        if !expected.is_empty() {
            let narrow =
                StatusLine::render(&surface, &[StatusSegmentName::Queue], rendered.width() - 1);
            assert_eq!(narrow.text(), "");
            assert!(narrow.omitted(StatusSegmentName::Queue));
        }
    }
}

#[test]
fn parked_draft_count_is_visible_in_the_native_status_row() {
    let mut surface = snapshot();
    surface.set_drafts(1);
    let rendered = StatusLine::render(
        &surface,
        &[StatusSegmentName::Model, StatusSegmentName::Drafts],
        80,
    );

    assert!(rendered.text().contains("drafts=1"), "{rendered:?}");
}

#[test]
fn inactive_and_optional_segments_collapse_as_whole_segments() {
    let mut surface = snapshot();
    surface.set_queue(QueueStatus {
        pending_steers: 2,
        queued: 1,
        interrupt_pending: true,
    });
    surface.set_drafts(3);
    let configured = [
        StatusSegmentName::Model,
        StatusSegmentName::Cost,
        StatusSegmentName::Queue,
        StatusSegmentName::Drafts,
        StatusSegmentName::Interrupt,
        StatusSegmentName::Git,
    ];
    let rendered = StatusLine::render(&surface, &configured, 30);
    assert!(rendered.text().contains("gpt-5.6"));
    assert!(!rendered.text().contains("cost"));
    assert!(!rendered.text().contains("gi"));
    assert!(!rendered.text().contains("steer 2"));
    assert!(!rendered.text().contains("drafts=3"));
    assert!(rendered.text().contains("interrupt="));
    assert!(
        rendered
            .text()
            .split_whitespace()
            .all(|segment| { !segment.contains("dra") && !segment.contains("que") })
    );
    assert!(rendered.width() <= 30);
}

#[test]
fn unicode_display_cells_are_counted_and_segments_are_never_truncated() {
    let mut surface = snapshot();
    surface.set_cwd("日本語🙂");
    let configured = [StatusSegmentName::Cwd, StatusSegmentName::Model];
    let rendered = StatusLine::render(&surface, &configured, 16);
    assert!(rendered.width() <= 16);
    assert!(rendered.text().contains("日本語🙂") || rendered.omitted(StatusSegmentName::Cwd));
    assert!(!rendered.text().contains("日本") || rendered.text().contains("日本語🙂"));
    assert_eq!(UnicodeWidthStr::width(rendered.text()), rendered.width());
}

#[test]
fn unknown_zero_width_and_resize_are_safe_and_recompute_from_one_snapshot() {
    let surface = snapshot();
    let configured = [StatusSegmentName::Model, StatusSegmentName::Context];
    let unknown = StatusLine::render(&surface, &configured, None);
    assert!(unknown.text().is_empty());
    assert_eq!(unknown.diagnostics().len(), 1);
    let zero = StatusLine::render(&surface, &configured, Some(0));
    assert!(zero.text().is_empty());
    assert_eq!(zero.diagnostics().len(), 1);
    let wide = StatusLine::render(&surface, &configured, Some(120));
    let narrow = StatusLine::render(&surface, &configured, Some(20));
    assert_eq!(wide.snapshot_revision(), narrow.snapshot_revision());
    assert_eq!(surface.model(), "openai/gpt-5.6");
}

#[test]
fn impossible_critical_width_retains_interrupt_then_queue_then_drafts() {
    let mut surface = snapshot();
    surface.set_queue(QueueStatus {
        pending_steers: 100,
        queued: 100,
        interrupt_pending: true,
    });
    surface.set_drafts(100);
    let configured = [
        StatusSegmentName::Drafts,
        StatusSegmentName::Queue,
        StatusSegmentName::Interrupt,
    ];
    let rendered = StatusLine::render(&surface, &configured, 8);
    assert!(!rendered.text().contains("drafts="));
    assert!(!rendered.text().contains("steer "));
    assert!(
        rendered.text().contains("interrupt=") || rendered.omitted(StatusSegmentName::Interrupt)
    );
    assert_eq!(rendered.overflow_diagnostics(), 1);
    assert!(rendered.width() <= 8);
}

#[test]
fn context_and_cost_are_authoritative_session_facts_not_rederived_from_selection() {
    let mut state = SessionState::default();
    state.context.used_tokens = Some(99_999);
    state.cost = CumulativeCost::Unknown(UnknownCost::ProviderReportedNoUsage);
    let surface = SurfaceSnapshot::from_state(
        SessionId::new(),
        &state,
        SelectionFacts::new(
            ModelRef("openai/gpt-5.6".into()),
            ThinkingLevel::Off,
            Mode::Plan,
        ),
        SeatFacts::new(QueueStatus::default(), 0, "/tmp", Trust::Trusted),
        None,
    );
    assert_eq!(surface.context(), "99999");
    assert_eq!(surface.cost(), "unknown");
    assert_eq!(surface.model(), "openai/gpt-5.6");
    assert_eq!(surface.effort(), "off");
    assert_eq!(surface.mode(), "plan");
}

#[test]
fn selection_and_interruption_facts_share_the_same_surface_snapshot() {
    let mut surface = snapshot();
    surface.set_queue(QueueStatus {
        pending_steers: 1,
        queued: 0,
        interrupt_pending: true,
    });
    assert_eq!(surface.pending_steers(), 1);
    assert!(surface.interrupt_pending());
    assert!(surface.action_snapshot().can_dispatch("model"));
    assert!(!surface.action_snapshot().can_dispatch("quit"));
}

#[test]
fn interactive_painter_is_the_sole_writer_and_preserves_transcript_interleaving() {
    let painter = ego_cli::repl::surface::InteractivePainter::new();
    painter.paint_transcript("assistant row\n");
    painter.paint_status(StatusLine::render(
        &snapshot(),
        &[StatusSegmentName::Model],
        120,
    ));
    assert_eq!(painter.writer_count(), 1);
    assert_eq!(painter.transcript(), "assistant row\n");
    assert!(painter.status_is_ephemeral());
    assert!(!painter.uses_alternate_screen());
    assert!(!painter.captures_mouse());
}

#[test]
fn asynchronous_git_failure_omits_only_git_and_adds_one_diagnostic_without_blocking() {
    let source = FailingGit;
    let initial = source.collect();
    assert!(initial.is_err());
    let surface = snapshot().without_git();
    let line = StatusLine::render(
        &surface,
        &[StatusSegmentName::Model, StatusSegmentName::Git],
        120,
    );
    assert!(line.text().contains("gpt-5.6"));
    assert!(!line.text().contains("git="));
    assert_eq!(line.diagnostics().len(), 1);
}

#[test]
fn git_adapter_never_executes_shell_or_user_configuration() {
    let command = ego_cli::repl::status::GitCommand::status("/workspace/ego");
    assert_eq!(command.program(), "git");
    assert!(command.arguments().windows(5).any(|args| {
        args.iter().map(String::as_str).eq([
            "-C",
            "/workspace/ego",
            "status",
            "--porcelain=v2",
            "-b",
        ])
    }));
    assert!(command.arguments().windows(2).any(|args| {
        args.iter()
            .map(String::as_str)
            .eq(["-c", "core.hooksPath=/dev/null"])
    }));
    assert!(command.arguments().windows(2).any(|args| {
        args.iter()
            .map(String::as_str)
            .eq(["-c", "core.fsmonitor=false"])
    }));
    assert!(command.arguments().iter().any(|arg| arg == "--no-pager"));
    assert_eq!(command.env("GIT_OPTIONAL_LOCKS"), Some("0"));
    assert_eq!(command.env("GIT_CONFIG_NOSYSTEM"), Some("1"));
    assert_eq!(command.env("GIT_CONFIG_GLOBAL"), Some("/dev/null"));
    assert_eq!(command.env("GIT_CONFIG_SYSTEM"), Some("/dev/null"));
    assert!(!command.uses_shell());
}
