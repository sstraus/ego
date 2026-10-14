use super::*;
use ego_core::event::domain::OccupancyProvenance;
use ego_core::state::ContextWindow;

fn run_window(
    used: Option<u64>,
    reported: Option<Usage>,
) -> (ExitReason, SessionState, usize, Vec<DomainEvent>) {
    let trace = GenerationTrace::default();
    let mut script = Vec::new();
    if let Some(usage) = reported {
        script.push(AiEvent::Usage(usage));
    }
    script.push(AiEvent::Stop(StopReason::Stop));
    let source = Arc::new(ScriptedGenerationSource::new([Ok(profile_with_settings(
        "openai/windowed",
        script,
        &trace,
        Some(1_024),
        Some(272_000),
        ThinkingLevel::Off,
        false,
    ))]));
    let fx = Fixture::new(vec![]);
    let mut state = SessionState::default();
    state.context = ContextWindow {
        used_tokens: used,
        window: Some(272_000),
        provenance: used.map(|_| OccupancyProvenance::GenerationBoundary),
    };
    let (exit, _, state) = run_scripted(
        &fx,
        state,
        "continue",
        source,
        TurnConfig {
            cache: CacheRetention::Short,
            system: None,
            identity: None,
            token_ceiling: None,
            retry: 0,
            fallbacks: vec![],
        },
    );
    let requests = trace.requests.lock().unwrap().len();
    (exit, state, requests, fx.store.events())
}

// Catches: a resumed known-full context sent to the provider again.
#[test]
fn known_full_context_refuses_before_provider_request() {
    for used in [272_000, 356_048] {
        let (exit, state, requests, events) = run_window(Some(used), None);
        assert_eq!(
            requests, 0,
            "known-full context must not reach the provider"
        );
        assert_eq!(exit, ExitReason::ContextOverflow);
        assert_eq!(state.context.used_tokens, Some(used));
        assert!(events.iter().any(|event| matches!(&event.kind,
            EventKind::Error { message, kind: ErrorKind::ContextOverflow }
            if message.contains("/compact") && message.contains(&used.to_string()))));
    }
}

// Catches: failing a delivered answer or clamping over-window provider accounting.
#[test]
fn over_window_report_completes_and_preserves_provider_accounting() {
    let usage = tokens(100_000, 250_000, 0, 6_048);
    let (exit, state, requests, _) = run_window(None, Some(usage));
    assert_eq!(exit, ExitReason::Completed);
    assert_eq!(requests, 1);
    assert_eq!(state.usage, usage);
    assert_eq!(state.context.used_tokens, Some(356_048));
    assert_eq!(state.context.window, Some(272_000));
}

// Catches: unknown occupancy or a still-fitting context refused as already full.
#[test]
fn unknown_and_below_window_context_can_generate() {
    for used in [None, Some(271_999)] {
        let (exit, _, requests, _) = run_window(used, Some(tokens(10, 20, 0, 3)));
        assert_eq!(exit, ExitReason::Completed);
        assert_eq!(requests, 1);
    }
}

// Catches: an opted-in tool turn sends another request after measured pressure.
#[test]
fn automatic_context_pressure_yields_before_next_tool_generation() {
    let trace = GenerationTrace::default();
    let source = Arc::new(ScriptedGenerationSource::new([
        Ok(profile_with_settings(
            "openai/windowed",
            vec![
                AiEvent::Usage(tokens(244_796, 0, 0, 4)),
                AiEvent::ToolCall {
                    call_id: "pressure-read".into(),
                    name: "read".into(),
                    args: r#"{"path":"/ws/notes.md"}"#.into(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            &trace,
            Some(1024),
            Some(272_000),
            ThinkingLevel::Off,
            false,
        )),
        Ok(profile_with_settings(
            "openai/windowed",
            vec![AiEvent::Stop(StopReason::Stop)],
            &trace,
            Some(1024),
            Some(272_000),
            ThinkingLevel::Off,
            false,
        )),
    ]));
    let fx = Fixture::new(vec![]);
    let (exit, _, state) = run_scripted_mode(
        &fx,
        SessionState::default(),
        "continue",
        source,
        TurnConfig {
            cache: CacheRetention::Short,
            system: None,
            identity: None,
            token_ceiling: None,
            retry: 0,
            fallbacks: vec![],
        },
        true,
    );
    assert_eq!(exit, ExitReason::ContextOverflow);
    assert_eq!(trace.requests.lock().unwrap().len(), 1);
    assert_eq!(state.context.used_tokens, Some(244_800));
    assert!(state.live_attempts.is_empty());
}

// Catches: rounding, overflow, or unknown occupancy inventing a pressure trigger.
#[test]
fn compaction_pressure_uses_only_measured_ninety_percent_boundary() {
    for (used, window, expected) in [
        (None, Some(272_000), false),
        (Some(244_799), Some(272_000), false),
        (Some(244_800), Some(272_000), true),
        (Some(272_000), Some(272_000), true),
        (Some(u64::MAX), Some(u64::MAX), true),
        (Some(1), Some(0), false),
    ] {
        assert_eq!(
            ego_core::turn::compaction_due(ContextWindow {
                used_tokens: used,
                window,
                provenance: None,
            }),
            expected
        );
    }
}
