//! Compact row contract: selection remains legible while optional fields drop.

use ego_cli::repl::status::{
    GitFacts, SeatFacts, SelectionFacts, StatusLine, StatusSegmentName as S, SurfaceSnapshot,
};
use ego_core::config::{Mode, ModelRef, Trust};
use ego_core::cost::{Amount, CumulativeCost, CurrencyCode, Money};
use ego_core::event::id::SessionId;
use ego_core::generator::types::ThinkingLevel;
use ego_core::state::{QueueStatus, SessionState};
use unicode_width::UnicodeWidthChar;

fn surface(used: Option<u64>) -> SurfaceSnapshot {
    let mut state = SessionState::default();
    state.context.used_tokens = used;
    state.context.window = Some(272_000);
    state.cost = CumulativeCost::Known {
        total: Money::new(
            "0.661".parse::<Amount>().unwrap(),
            CurrencyCode::new("USD").unwrap(),
        ),
    };
    SurfaceSnapshot::from_state_with_git(
        SessionId::new(),
        &state,
        SelectionFacts::new(
            ModelRef("openai/gpt-6-sol".into()),
            ThinkingLevel::Medium,
            Mode::Yolo,
        ),
        SeatFacts::new(
            QueueStatus::default(),
            0,
            "~/workspace/mdkb",
            Trust::Trusted,
        ),
        Some(GitFacts::new("feat/configurable-embedder", true)),
    )
}

const SEGMENTS: &[S] = &[
    S::Model,
    S::Effort,
    S::Mode,
    S::Context,
    S::Cost,
    S::Cwd,
    S::Git,
];

// Catches: labels and long paths consume the cells needed by the context meter.
#[test]
fn compact_layout_keeps_selection_and_context_at_sixty_columns() {
    let snapshot = surface(Some(58_000));
    let wide = StatusLine::render(&snapshot, SEGMENTS, 120);
    assert_eq!(
        wide.text(),
        "gpt-6-sol · medium · yolo ▰▰▱▱▱▱▱▱▱▱ 58k/272k $0.66 ~/G/p/mdkb ⎇ feat/configurable-embedder●"
    );
    for character in wide.text().chars() {
        assert!(u32::from(character) <= 0xffff);
        assert_eq!(character.width(), Some(1), "{character}");
    }
    for (width, absent) in [
        (90, vec![S::Git]),
        (60, vec![S::Git, S::Cwd]),
        (50, vec![S::Git, S::Cwd, S::Cost]),
    ] {
        let line = StatusLine::render(&snapshot, SEGMENTS, width);
        for name in SEGMENTS {
            assert_eq!(
                line.omitted(name.clone()),
                absent.contains(name),
                "width={width} {line:?}"
            );
        }
        assert!(
            line.text()
                .starts_with("gpt-6-sol · medium · yolo ▰▰▱▱▱▱▱▱▱▱ 58k/272k")
        );
        assert!(line.width() <= width);
    }
}

// Catches: an absent measurement looks like measured zero occupancy.
#[test]
fn unknown_occupancy_keeps_the_window_without_claiming_a_measurement() {
    assert_eq!(
        StatusLine::render(&surface(None), &[S::Context], 60).text(),
        "▱▱▱▱▱▱▱▱▱▱ ?/272k"
    );
    assert_eq!(
        StatusLine::render(&surface(Some(0)), &[S::Context], 60).text(),
        "▱▱▱▱▱▱▱▱▱▱ 0/272k"
    );
}

// Catches: threshold rounding paints 59.99% yellow or 84.99% red.
#[test]
fn context_colour_uses_exact_thresholds_and_obeys_no_colour() {
    use ego_cli::render::Capabilities;
    use ego_cli::render::caps::Depth;
    let mut caps = Capabilities::plain();
    caps.depth = Depth::TrueColor;
    for (used, colour) in [
        (163_199, "151;207;167"),
        (163_200, "217;190;129"),
        (231_199, "217;190;129"),
        (231_200, "221;151;158"),
    ] {
        let line = StatusLine::render(&surface(Some(used)), &[S::Context], 60);
        assert!(line.terminal_text(&caps).contains(colour));
        assert_eq!(line.terminal_text(&Capabilities::plain()), line.text());
    }
}

// Catches: home-prefix replacement abbreviates a sibling path as home.
#[test]
fn cwd_abbreviates_components_but_not_a_home_prefix_collision() {
    let home = std::env::var("HOME").unwrap();
    let mut snapshot = surface(None);
    snapshot.set_cwd(format!("{home}/workspace/mdkb"));
    assert_eq!(
        StatusLine::render(&snapshot, &[S::Cwd], 120).text(),
        "~/G/p/mdkb"
    );
    snapshot.set_cwd(format!("{home}-other/Gits/mdkb"));
    let line = StatusLine::render(&snapshot, &[S::Cwd], 120);
    assert!(!line.text().starts_with('~'));
    assert!(line.text().ends_with("/G/mdkb"));
    snapshot.set_cwd(&home);
    assert_eq!(StatusLine::render(&snapshot, &[S::Cwd], 120).text(), "~");
}
