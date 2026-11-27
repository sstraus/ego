use super::*;
use ego_core::compact::{CompactionConfig, ContextPercent};
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
    let (exit, requests, used) = pressure_run(244_796, 90);
    assert_eq!(exit, ExitReason::ContextOverflow);
    assert_eq!(requests, 1);
    assert_eq!(used, Some(244_800));
}

// Catches: a configured threshold ignored by the turn loop (still fixed at 90%).
#[test]
fn configured_threshold_moves_the_turn_yield_point() {
    // 136_000 of 272_000 is 50%: it yields at 50 and keeps going at the default.
    let (exit, requests, _) = pressure_run(135_996, 50);
    assert_eq!((exit, requests), (ExitReason::ContextOverflow, 1));
    let (exit, requests, _) = pressure_run(135_996, 90);
    assert_eq!((exit, requests), (ExitReason::Completed, 2));
}

fn pressure_run(input: u64, percent: u8) -> (ExitReason, usize, Option<u64>) {
    let trace = GenerationTrace::default();
    let source = Arc::new(ScriptedGenerationSource::new([
        Ok(profile_with_settings(
            "openai/windowed",
            vec![
                AiEvent::Usage(tokens(input, 0, 0, 4)),
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
        Some(ContextPercent::try_from(percent).unwrap()),
    );
    if exit == ExitReason::ContextOverflow {
        assert!(state.live_attempts.is_empty());
        // Catches: routine pressure recovery being reported as a failure.
        assert!(state.errors.is_empty(), "{:?}", state.errors);
    }
    let requests = trace.requests.lock().unwrap().len();
    (exit, requests, state.context.used_tokens)
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
            ego_core::turn::compaction_due(
                ContextWindow {
                    used_tokens: used,
                    window,
                    provenance: None,
                },
                CompactionConfig::default().threshold
            ),
            expected
        );
    }
}

// Catches: a configured threshold ignored, or its boundary rounded the wrong way.
#[test]
fn compaction_pressure_follows_the_configured_threshold() {
    let at = |percent: u8, used| {
        ego_core::turn::compaction_due(
            ContextWindow {
                used_tokens: Some(used),
                window: Some(1_000),
                provenance: None,
            },
            ContextPercent::try_from(percent).unwrap(),
        )
    };
    assert!(!at(50, 499));
    assert!(at(50, 500));
    assert!(!at(99, 989));
    assert!(at(99, 990));
    assert!(at(1, 10));
}

// ---- Blocked session a16e5f9b (docs/internals/2026-11-26-blocked-session-context-overflow.md) ----

fn varied(len: usize) -> String {
    (0..len)
        .map(|i| char::from(b'a' + ((i * 7 + i / 26) % 26) as u8))
        .collect()
}

fn flood_config(retry: u32) -> TurnConfig {
    TurnConfig {
        cache: CacheRetention::Short,
        system: None,
        identity: None,
        token_ceiling: None,
        retry,
        fallbacks: vec![],
    }
}

fn scripted_profiles(
    scripts: Vec<Vec<AiEvent>>,
    trace: &GenerationTrace,
) -> Arc<ScriptedGenerationSource> {
    Arc::new(ScriptedGenerationSource::new(scripts.into_iter().map(
        |script| {
            Ok(profile_with_settings(
                "openai/windowed",
                script,
                trace,
                Some(1_024),
                Some(272_000),
                ThinkingLevel::Off,
                false,
            ))
        },
    )))
}

fn call(id: &str, args: String) -> AiEvent {
    AiEvent::ToolCall {
        call_id: id.into(),
        name: "read".into(),
        args,
    }
}

fn largest_journaled_args(events: &[DomainEvent]) -> usize {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::ToolCallStarted { args, .. } => Some(args.len()),
            _ => None,
        })
        .max()
        .unwrap_or(0)
}

fn request_bytes(trace: &GenerationTrace, index: usize) -> usize {
    serde_json::to_vec(&trace.requests.lock().unwrap()[index])
        .unwrap()
        .len()
}

// Catches: a whitespace flood streamed as tool arguments being journaled and replayed
// (blocked session: +90k tokens in one request). Adjacent: repetition flood, no retry
// budget, and a large but varied argument that must NOT be treated as a flood.
#[test]
fn degenerate_argument_flood_aborts_generation_retries_and_is_never_stored() {
    let whitespace = format!("{{\"path\":\"/ws/a\",{}\"x\":1}}", "\t ".repeat(150_000));
    let repeated = format!("{{\"path\":\"{}\"}}", "ab".repeat(100_000));
    for flood in [whitespace, repeated] {
        let trace = GenerationTrace::default();
        let source = scripted_profiles(
            vec![
                vec![call("flood", flood), AiEvent::Stop(StopReason::ToolUse)],
                vec![AiEvent::Stop(StopReason::Stop)],
            ],
            &trace,
        );
        let fx = Fixture::new(vec![]);
        let (exit, _, _) = run_scripted(&fx, SessionState::default(), "go", source, flood_config(1));
        assert_eq!(exit, ExitReason::Completed, "the flood is a retryable failure");
        assert_eq!(trace.requests.lock().unwrap().len(), 2);
        assert_eq!(largest_journaled_args(&fx.store.events()), 0, "flood stored");
        assert!(request_bytes(&trace, 1) < 20_000, "flood replayed");
    }
    // Adjacent: no retry budget -> the turn ends and still stores nothing.
    let trace = GenerationTrace::default();
    let source = scripted_profiles(
        vec![vec![
            call("flood", format!("{{\"p\":\"{}\"}}", " ".repeat(300_000))),
            AiEvent::Stop(StopReason::ToolUse),
        ]],
        &trace,
    );
    let fx = Fixture::new(vec![]);
    let (exit, _, state) = run_scripted(&fx, SessionState::default(), "go", source, flood_config(0));
    assert_ne!(exit, ExitReason::Completed);
    assert_eq!(largest_journaled_args(&fx.store.events()), 0);
    assert!(state.items.values().all(|item| item.args.len() < 1_000));
    // Adjacent: varied 100 KB arguments are a call, not a flood.
    let trace = GenerationTrace::default();
    let source = scripted_profiles(
        vec![
            vec![
                call("big", format!("{{\"path\":\"/ws/a\",\"pad\":\"{}\"}}", varied(100_000))),
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![AiEvent::Stop(StopReason::Stop)],
        ],
        &trace,
    );
    let fx = Fixture::new(vec![]);
    let (exit, _, _) = run_scripted(&fx, SessionState::default(), "go", source, flood_config(0));
    assert_eq!(exit, ExitReason::Completed);
    assert!(largest_journaled_args(&fx.store.events()) >= 100_000);
}

// Catches: an interrupted call (stream ended in an HTTP timeout) replaying its whole
// argument blob in the next turn's request. Adjacent: a call just under the replay bound
// stays verbatim; an accepted large call is bounded on replay too.
#[test]
fn interrupted_call_arguments_replay_bounded_with_marker() {
    let huge = format!("{{\"path\":\"/ws/a\",\"pad\":\"{}\"}}", varied(120_000));
    let trace = GenerationTrace::default();
    let source = scripted_profiles(
        vec![vec![
            call("cut", huge.clone()),
            AiEvent::Error {
                message: "HTTP call timed out (chatgpt.com)".into(),
                kind: ErrorKind::Transient,
            },
        ]],
        &trace,
    );
    let fx = Fixture::new(vec![]);
    let (_, _, state) = run_scripted(&fx, SessionState::default(), "go", source, flood_config(0));
    let next = GenerationTrace::default();
    let source = scripted_profiles(vec![vec![AiEvent::Stop(StopReason::Stop)]], &next);
    let fx2 = Fixture::new(vec![]);
    let _ = run_scripted(&fx2, state, "next", source, flood_config(0));
    assert!(request_bytes(&next, 0) < 20_000, "{}", request_bytes(&next, 0));
    let rendered = serde_json::to_string(&next.requests.lock().unwrap()[0].messages).unwrap();
    assert!(rendered.contains("ego_truncated_arguments"));
    assert!(rendered.contains(&format!("original_bytes\\\":{}", huge.len())));
}

// Catches: a call over the accepted size limit being dispatched or replayed whole.
// Adjacent: the refusal is model-visible with cause, retry advice and shape.
#[test]
fn oversize_call_is_refused_visibly_and_replayed_bounded() {
    let huge = format!("{{\"path\":\"/ws/a\",\"pad\":\"{}\"}}", varied(300_000));
    let trace = GenerationTrace::default();
    let source = scripted_profiles(
        vec![
            vec![call("huge", huge), AiEvent::Stop(StopReason::ToolUse)],
            vec![AiEvent::Stop(StopReason::Stop)],
        ],
        &trace,
    );
    let fx = Fixture::new(vec![]);
    let (exit, _, _) = run_scripted(&fx, SessionState::default(), "go", source, flood_config(0));
    assert_eq!(exit, ExitReason::Completed);
    assert!(largest_journaled_args(&fx.store.events()) < 20_000);
    assert!(request_bytes(&trace, 1) < 20_000);
    let rendered = serde_json::to_string(&trace.requests.lock().unwrap()[1].messages).unwrap();
    assert!(rendered.contains("refused again") && rendered.contains("split the content"));
}

// Catches: the guard measuring only after the fact: a measured 100k context plus a huge
// unsent prompt (est. 200k tokens) went to the provider. Adjacent: unknown occupancy
// still sends; a small prompt still sends; automatic mode yields to compaction when the
// projection (not the measurement) crosses the threshold.
#[test]
fn projected_request_size_is_checked_before_sending() {
    let measured = |used| ContextWindow {
        used_tokens: used,
        window: Some(272_000),
        provenance: used.map(|_| OccupancyProvenance::GenerationBoundary),
    };
    let run = |used: Option<u64>, prompt: &str, automatic| {
        let trace = GenerationTrace::default();
        let source = scripted_profiles(vec![vec![AiEvent::Stop(StopReason::Stop)]], &trace);
        let fx = Fixture::new(vec![]);
        let mut state = SessionState::default();
        state.context = measured(used);
        let (exit, _, _) = run_scripted_mode(&fx, state, prompt, source, flood_config(0), automatic);
        let sent = trace.requests.lock().unwrap().len();
        (exit, sent, fx.store.events())
    };
    let big = "x y ".repeat(150_000);
    let (exit, sent, events) = run(Some(100_000), &big, None);
    assert_eq!((exit, sent), (ExitReason::ContextOverflow, 0));
    assert!(events.iter().any(|event| matches!(&event.kind,
        EventKind::Error { message, .. } if message.contains("projected") && message.contains("/compact"))));
    assert_eq!(run(None, &big, None).1, 1);
    assert_eq!(run(Some(100_000), "small", None).1, 1);
    let percent = Some(ContextPercent::try_from(50).unwrap());
    let medium = "x y ".repeat(50_000);
    let (exit, sent, _) = run(Some(100_000), &medium, percent);
    assert_eq!((exit, sent), (ExitReason::ContextOverflow, 0));
}
