// Critic attacks on the known-full context refusal (regression case).
use super::*;

const CODEX_WINDOW: u64 = 272_000;

fn answer(usage: Usage) -> Vec<AiEvent> {
    vec![
        AiEvent::TextDelta {
            text: "done".to_owned(),
        },
        AiEvent::Usage(usage),
        AiEvent::Stop(StopReason::Stop),
    ]
}

fn one_model(
    slug: &str,
    script: Vec<AiEvent>,
    window: Option<u64>,
) -> Arc<ScriptedGenerationSource> {
    let trace = GenerationTrace::default();
    Arc::new(
        ScriptedGenerationSource::new([Ok(profile_with_settings(
            slug,
            script,
            &trace,
            Some(1_024),
            window,
            ThinkingLevel::Off,
            false,
        ))])
        .with_trace(trace),
    )
}

fn config() -> TurnConfig {
    TurnConfig {
        cache: ego_core::generator::capabilities::CacheRetention::Short,
        system: None,
        identity: None,
        token_ceiling: None,
        retry: 0,
        fallbacks: Vec::new(),
    }
}

// Catches: a final answer that was delivered, with no further provider request
// to prevent, ends its turn as `context_overflow` (failed exit) because the
// check runs before the stop reason is read.
#[test]
fn a_delivered_final_answer_over_the_window_completes_its_turn() {
    let fx = Fixture::new(vec![]);
    let (exit, ok, state) = run_scripted(
        &fx,
        SessionState::default(),
        "answer once",
        one_model(
            "openai-codex/gpt-6-sol",
            answer(tokens(300_000, 0, 0, 1_000)),
            Some(CODEX_WINDOW),
        ),
        config(),
    );
    assert_eq!(
        (exit, ok),
        (ExitReason::Completed, true),
        "the answer was delivered and nothing else was sent"
    );
    assert_eq!(state.context.used_tokens, Some(301_000));
}
