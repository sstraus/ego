//! ACP consumes the same durable reduction as the terminal, live and on replay.
use ego_acp::project::{project_acp_updates_v1, project_replay_v1};
use ego_core::event::id::TurnId;
use ego_core::generator::capabilities::*;
use ego_core::plan::{PlanStep, PlanUpdate, StepStatus};
use ego_core::public::PublicEvent;
use ego_core::session_lifecycle::ReplayFrame;
use ego_core::state::SessionState;

#[test]
fn live_and_replay_replace_the_same_plan_including_clear_and_blocked_metadata() {
    // Catches: replay drops plan progress, ACP invents IDs, or clearing retains old entries.
    let root = std::path::Path::new("/workspace");
    let turn = TurnId::new();
    let mut state = SessionState::default();
    let mut frames = Vec::new();
    let updates = [
        PlanUpdate::Replace {
            steps: vec![
                PlanStep {
                    id: "one".into(),
                    text: "First".into(),
                    status: StepStatus::Completed,
                },
                PlanStep {
                    id: "two".into(),
                    text: "Second".into(),
                    status: StepStatus::InProgress,
                },
            ],
        },
        PlanUpdate::Step {
            id: "two".into(),
            status: StepStatus::Blocked,
        },
        PlanUpdate::Replace { steps: vec![] },
    ];
    let mut live = Vec::new();
    for update in updates {
        state.plan.apply(&update).unwrap();
        let event = PublicEvent::PlanUpdated { turn, update };
        // Usage capabilities are irrelevant to plan projection.
        let projected = project_acp_updates_v1(
            &event,
            &state,
            &ModelCapabilities {
                schema_version: ModelCapabilities::SCHEMA_VERSION,
                audio_input: ego_core::generator::capabilities::AudioInputSupport::Unknown,
                max_context_tokens: None,
                max_output_tokens: None,
                max_tools: None,
                supports_parallel_tool_calls: true,
                reasoning_carryover: ReasoningCarryover::Portable,
                reasoning_efforts: ReasoningEffortSupport::Any,
                reasoning_state_dialect: None,
                cancel: CancelSemantics::Cooperative,
                cache: CacheRetention::Short,
                transport: Transport::Sse,
                image_input: ImageInputSupport::Unknown,
                default_reasoning_effort: None,
            },
            root,
        );
        live.extend(projected);
        frames.push(ReplayFrame {
            event,
            state: state.clone(),
            inherited: false,
        });
    }
    assert_eq!(project_replay_v1(&frames, None, root), live);
    let wire = serde_json::to_value(&live).unwrap();
    assert_eq!(wire[0]["entries"][0]["status"], "completed");
    assert_eq!(wire[1]["entries"][1]["status"], "pending");
    assert_eq!(wire[1]["entries"][1]["_meta"]["ego"]["status"], "blocked");
    assert_eq!(wire[1]["entries"][1]["_meta"]["ego"]["stepId"], "two");
    assert_eq!(wire[2]["entries"], serde_json::json!([]));
}
