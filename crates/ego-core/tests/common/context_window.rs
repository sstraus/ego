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
