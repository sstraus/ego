//! RED contract for the ACP operator seat (regression case, criterion 1).
//!
//! The wire half lives in `ego-acp`; this is the half that decides what a
//! host may be offered at all. The rules it has to keep: never offer a model
//! the catalogue calls unavailable, never offer an effort the model does not
//! accept, never offer `yolo`, never offer `auto` without a judge, and never
//! report a chosen value the operator did not choose.

use ego_ai::catalogue::{
    FactFreshness, ModelAvailability, ModelAvailabilityCause, ModelCatalogueEntry,
    ModelCatalogueSnapshot, ModelFactProvenance, ModelSlug, SuppliedAuthority,
};
use ego_cli::acp_controls::AcpSessionControls;
use ego_core::config::{Mode, ModelRef};
use ego_core::control::{ControlError, SessionControlChange, SessionControls};
use ego_core::event::id::SessionId;
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ImageInputSupport, ModelCapabilities, ReasoningCarryover,
    ReasoningEffortSupport, Transport,
};
use ego_core::generator::types::ThinkingLevel;
use ego_core::public::MeteringKind;

fn entry(
    slug: &str,
    efforts: ReasoningEffortSupport,
    availability: ModelAvailability,
) -> ModelCatalogueEntry {
    ModelCatalogueEntry {
        slug: ModelSlug::parse(slug).expect("a provider/model slug"),
        capabilities: ModelCapabilities {
            schema_version: ModelCapabilities::SCHEMA_VERSION,
            max_context_tokens: None,
            max_output_tokens: None,
            max_tools: None,
            supports_parallel_tool_calls: false,
            reasoning_carryover: ReasoningCarryover::Unsupported,
            reasoning_efforts: efforts,
            reasoning_state_dialect: None,
            cancel: CancelSemantics::Unsupported,
            cache: CacheRetention::None,
            transport: Transport::Sse,
            image_input: ImageInputSupport::Unknown,
        },
        metering: MeteringKind::Unmetered,
        provenance: ModelFactProvenance::supplied(SuppliedAuthority::System),
        freshness: FactFreshness::Current,
        availability,
    }
}

fn catalogue() -> ModelCatalogueSnapshot {
    ModelCatalogueSnapshot {
        v: 1,
        models: vec![
            entry(
                "ollama/gemma4",
                ReasoningEffortSupport::Only([ThinkingLevel::Off, ThinkingLevel::Low].into()),
                ModelAvailability::Available,
            ),
            entry(
                "anthropic/claude-sonnet-5",
                ReasoningEffortSupport::Any,
                ModelAvailability::Available,
            ),
            entry(
                "openai/gpt-5",
                ReasoningEffortSupport::Any,
                ModelAvailability::Unavailable {
                    cause: ModelAvailabilityCause::MissingCredential,
                    message: "no credential".to_owned(),
                },
            ),
        ],
        sources: Vec::new(),
    }
}

fn seat(judge: bool) -> AcpSessionControls {
    AcpSessionControls::seeded(
        ModelRef("ollama/gemma4".to_owned()),
        ThinkingLevel::Low,
        Mode::Default,
        judge,
        Some(catalogue()),
    )
}

/// The configured model and effort are what the run would use, and neither is
/// reported as a choice: nobody picked them.
#[test]
fn an_untouched_session_reports_configuration_without_calling_it_a_choice() {
    let state = seat(false)
        .state(SessionId::new())
        .expect("a readable seat");
    assert_eq!(state.model.current, ModelRef("ollama/gemma4".to_owned()));
    assert_eq!(state.model.chosen, None);
    assert_eq!(state.effort.current, ThinkingLevel::Low);
    assert_eq!(state.effort.chosen, None);
    assert_eq!(state.mode.current, Mode::Default);
    assert_eq!(state.mode.chosen, None);
}

/// A model `ego models` calls unavailable is not offered. Offering it would
/// let a host pick something the run then refuses to resolve.
#[test]
fn an_unavailable_model_is_never_offered() {
    let state = seat(false)
        .state(SessionId::new())
        .expect("a readable seat");
    assert!(
        state
            .model
            .available
            .contains(&ModelRef("anthropic/claude-sonnet-5".to_owned()))
    );
    assert!(
        !state
            .model
            .available
            .contains(&ModelRef("openai/gpt-5".to_owned())),
        "an unavailable model must not be offered: {:?}",
        state.model.available
    );
}

/// Effort is a property of the model, so the offer has to move when the model
/// does. `gemma4` accepts two levels; sonnet accepts all seven.
#[test]
fn the_effort_offer_follows_the_model() {
    let seat = seat(false);
    let session = SessionId::new();
    let before = seat.state(session).expect("a readable seat");
    assert_eq!(
        before.effort.available,
        vec![ThinkingLevel::Off, ThinkingLevel::Low]
    );
    let after = seat
        .stage(
            session,
            SessionControlChange::Model(ModelRef("anthropic/claude-sonnet-5".to_owned())),
        )
        .expect("an offered model is accepted");
    assert_eq!(after.effort.available.len(), 7);
    assert_eq!(
        after.model.chosen,
        Some(ModelRef("anthropic/claude-sonnet-5".to_owned()))
    );
}

/// Moving to a model that accepts fewer levels must not leave the session on
/// an effort that model rejects; the next generation would fail on it.
#[test]
fn a_model_that_rejects_the_current_effort_moves_the_effort_too() {
    let seat = AcpSessionControls::seeded(
        ModelRef("anthropic/claude-sonnet-5".to_owned()),
        ThinkingLevel::Max,
        Mode::Default,
        false,
        Some(catalogue()),
    );
    let session = SessionId::new();
    let after = seat
        .stage(
            session,
            SessionControlChange::Model(ModelRef("ollama/gemma4".to_owned())),
        )
        .expect("an offered model is accepted");
    assert!(
        after.effort.available.contains(&after.effort.current),
        "the effort must land inside what the new model accepts: {after:?}"
    );
    assert_ne!(after.effort.current, ThinkingLevel::Max);
}

/// An effort the current model does not accept is refused even though the
/// enum can hold it.
#[test]
fn an_effort_the_model_rejects_is_refused() {
    let refusal = seat(false)
        .stage(
            SessionId::new(),
            SessionControlChange::Effort(ThinkingLevel::Max),
        )
        .expect_err("an unsupported effort must be refused");
    assert_eq!(
        refusal,
        ControlError::Unavailable {
            control: "effort",
            value: "max".to_owned()
        }
    );
}

/// `yolo` is the mode that stops asking. The terminal seat reaches it only by
/// typing the word; `session/set_config_option` carries a value id and no
/// confirmation, so this seat does not offer it at all.
#[test]
fn yolo_is_neither_offered_nor_accepted_over_the_wire() {
    let seat = seat(true);
    let state = seat.state(SessionId::new()).expect("a readable seat");
    assert!(!state.mode.available.contains(&Mode::Yolo));
    let refusal = seat
        .stage(SessionId::new(), SessionControlChange::Mode(Mode::Yolo))
        .expect_err("yolo must never be staged from the wire");
    assert!(matches!(
        refusal,
        ControlError::Refused {
            control: "mode",
            ..
        }
    ));
}

/// `auto` delegates to a judge model. Without one the run refuses at startup,
/// so offering it would be offering a mode that cannot be entered.
#[test]
fn auto_is_offered_only_where_a_judge_is_configured() {
    assert!(
        !seat(false)
            .state(SessionId::new())
            .expect("a readable seat")
            .mode
            .available
            .contains(&Mode::Auto)
    );
    assert!(
        seat(true)
            .state(SessionId::new())
            .expect("a readable seat")
            .mode
            .available
            .contains(&Mode::Auto)
    );
}

/// One connection holds many sessions. A pick in one is not a pick in any
/// other, or a host with two windows would retune both by touching one.
#[test]
fn a_pick_in_one_session_does_not_reach_another() {
    let seat = seat(false);
    let touched = SessionId::new();
    let untouched = SessionId::new();
    seat.stage(touched, SessionControlChange::Mode(Mode::Plan))
        .expect("plan is offered");
    assert_eq!(
        seat.state(touched).expect("a readable seat").mode.current,
        Mode::Plan
    );
    assert_eq!(
        seat.state(untouched).expect("a readable seat").mode.current,
        Mode::Default
    );
    assert_eq!(
        seat.state(untouched).expect("a readable seat").mode.chosen,
        None
    );
}

/// Without a catalogue the seat still works, and says so honestly: one model,
/// one effort, and every mode. Inventing a list it cannot vouch for would be
/// worse than a short one.
#[test]
fn a_seat_with_no_catalogue_offers_only_what_it_can_vouch_for() {
    let seat = AcpSessionControls::seeded(
        ModelRef("ollama/gemma4".to_owned()),
        ThinkingLevel::Low,
        Mode::Default,
        false,
        None,
    );
    let state = seat.state(SessionId::new()).expect("a readable seat");
    assert_eq!(
        state.model.available,
        vec![ModelRef("ollama/gemma4".to_owned())]
    );
    assert_eq!(state.effort.available, vec![ThinkingLevel::Low]);
    assert!(state.mode.available.contains(&Mode::Plan));
}
