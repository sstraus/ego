//! contract for the ACP operator seat (regression case, criterion 1).
//!
//! The wire half lives in `ego-acp`; this is the half that decides what a
//! host may be offered at all. The rules it has to keep: never offer a model
//! the catalogue calls unavailable, never offer an effort the model does not
//! accept, offer `yolo` only by explicit host selection, never offer `auto` without a judge, and never
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
use ego_core::permit::Confinement;
use ego_core::permit::{
    BackendIdentity, ExecCaps, ExecObservation, ExecRequirements, MissingExecCaps, PolicyDigest,
};
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
            default_reasoning_effort: None,
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

fn catalogue_with_default(default: ThinkingLevel) -> ModelCatalogueSnapshot {
    let mut catalogue = catalogue();
    let row = catalogue
        .models
        .iter_mut()
        .find(|entry| entry.slug.to_string() == "ollama/gemma4")
        .expect("catalogue fixture includes ollama/gemma4");
    row.capabilities.reasoning_efforts = ReasoningEffortSupport::Only(
        [
            ThinkingLevel::Off,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
        ]
        .into(),
    );
    row.capabilities.default_reasoning_effort = Some(default);
    catalogue
}

fn seat(judge: bool) -> AcpSessionControls {
    ceilinged(judge, Confinement::Workspace)
}

/// The same seat with a stated confinement ceiling, for the tests that are
/// about the ceiling rather than about the generation controls.
fn ceilinged(judge: bool, ceiling: Confinement) -> AcpSessionControls {
    AcpSessionControls::seeded(
        Some(ModelRef("ollama/gemma4".to_owned())),
        Some(ThinkingLevel::Low),
        Mode::Default,
        judge,
        ceiling,
        Some(catalogue()),
    )
}

fn observation(confinement: Confinement) -> ExecObservation {
    let capabilities = match confinement {
        Confinement::Ro => ExecCaps::READ_SCOPED.union(ExecCaps::WRITE_DENIED),
        Confinement::Workspace => ExecCaps::READ_SCOPED.union(ExecCaps::WRITE_SCOPED),
        Confinement::Off => ExecCaps::empty(),
    };
    ExecObservation::new(
        BackendIdentity::new("fixture", "regression_case"),
        capabilities,
        PolicyDigest::try_from("0000000000000000000000000000000000000000000000000000000000000000")
            .expect("a valid fixture policy digest"),
    )
}

fn missing(confinement: Confinement) -> MissingExecCaps {
    ExecRequirements::new(confinement.required_caps())
        .check(ExecCaps::empty())
        .expect_err("the fixture deliberately lacks confinement capabilities")
        .from_backend(BackendIdentity::new("fixture", "regression_case"))
}

fn admit_all(seat: &AcpSessionControls, session: SessionId) {
    seat.seed_confinement(
        session,
        vec![
            (Confinement::Ro, observation(Confinement::Ro)),
            (Confinement::Workspace, observation(Confinement::Workspace)),
            (Confinement::Off, observation(Confinement::Off)),
        ],
        Vec::new(),
    )
    .expect("the fixture's measured rows are unique and complete");
}

#[test]
fn uninitialized_controls_refuse_state_and_stage() {
    let seat = seat(false);
    let session = SessionId::new();
    assert!(matches!(
        seat.state(session),
        Err(ControlError::Internal(message))
            if message.contains("before confinement initialization")
    ));
    assert!(matches!(
        seat.stage(session, SessionControlChange::Mode(Mode::Plan)),
        Err(ControlError::Internal(message))
            if message.contains("before confinement initialization")
    ));
}

/// The configured model and effort are what the run would use, and neither is
/// reported as a choice: nobody picked them.
#[test]
fn an_untouched_session_reports_configuration_without_calling_it_a_choice() {
    let session = SessionId::new();
    let seat = seat(false);
    admit_all(&seat, session);
    let state = seat.state(session).expect("a readable seat");
    assert_eq!(
        state.model.current,
        Some(ModelRef("ollama/gemma4".to_owned()))
    );
    assert_eq!(state.model.chosen, None);
    assert_eq!(state.effort.current, Some(ThinkingLevel::Low));
    assert_eq!(state.effort.chosen, None);
    assert_eq!(state.mode.current, Some(Mode::Default));
    assert_eq!(state.mode.chosen, None);
}

/// A model `ego models` calls unavailable is not offered. Offering it would
/// let a host pick something the run then refuses to resolve.
#[test]
fn an_unavailable_model_is_never_offered() {
    let session = SessionId::new();
    let seat = seat(false);
    admit_all(&seat, session);
    let state = seat.state(session).expect("a readable seat");
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
    admit_all(&seat, session);
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

/// Moving models recomputes a resolved default when the operator has not
/// selected an explicit effort. The next generation must not inherit the
/// previous model's default by accident.
#[test]
fn a_model_change_recomputes_a_resolved_default_without_an_explicit_effort() {
    let mut catalogue = catalogue_with_default(ThinkingLevel::Low);
    catalogue
        .models
        .iter_mut()
        .find(|entry| entry.slug.to_string() == "anthropic/claude-sonnet-5")
        .expect("catalogue fixture includes anthropic/claude-sonnet-5")
        .capabilities
        .default_reasoning_effort = Some(ThinkingLevel::Max);
    let seat = AcpSessionControls::seeded(
        Some(ModelRef("anthropic/claude-sonnet-5".to_owned())),
        None,
        Mode::Default,
        false,
        Confinement::Workspace,
        Some(catalogue),
    );
    let session = SessionId::new();
    admit_all(&seat, session);
    assert_eq!(
        seat.state(session)
            .expect("the base model default is resolved")
            .effort
            .current,
        Some(ThinkingLevel::Max)
    );
    let after = seat
        .stage(
            session,
            SessionControlChange::Model(ModelRef("ollama/gemma4".to_owned())),
        )
        .expect("an offered model is accepted");
    assert_eq!(after.effort.current, Some(ThinkingLevel::Low));
    assert_eq!(after.effort.chosen, None);
}

/// An effort the current model does not accept is refused even though the
/// enum can hold it.
#[test]
fn an_effort_the_model_rejects_is_refused() {
    let session = SessionId::new();
    let seat = seat(false);
    admit_all(&seat, session);
    let refusal = seat
        .stage(session, SessionControlChange::Effort(ThinkingLevel::Max))
        .expect_err("an unsupported effort must be refused");
    assert_eq!(
        refusal,
        ControlError::Unavailable {
            control: "effort",
            value: "max".to_owned()
        }
    );
}

#[test]
fn session_mode_picker_offers_yolo_and_applies_it_to_the_next_generation() {
    let seat = seat(false);
    let session = SessionId::new();
    admit_all(&seat, session);
    let before = seat.state(session).expect("session controls");
    assert!(before.mode.available.contains(&Mode::Yolo));
    let selected = seat
        .stage(session, SessionControlChange::Mode(Mode::Yolo))
        .expect("the host selected the offered mode");
    assert_eq!(selected.mode.current, Some(Mode::Yolo));
    assert_eq!(selected.mode.chosen, Some(Mode::Yolo));
    assert_eq!(
        seat.state(session)
            .expect("the next generation reads controls")
            .mode
            .current,
        Some(Mode::Yolo)
    );
}

/// `auto` delegates to a judge model. Without one the run refuses at startup,
/// so offering it would be offering a mode that cannot be entered.
#[test]
fn auto_is_offered_only_where_a_judge_is_configured() {
    let without_judge_session = SessionId::new();
    let without_judge = seat(false);
    admit_all(&without_judge, without_judge_session);
    assert!(
        !without_judge
            .state(without_judge_session)
            .expect("a readable seat")
            .mode
            .available
            .contains(&Mode::Auto)
    );
    let with_judge_session = SessionId::new();
    let with_judge = seat(true);
    admit_all(&with_judge, with_judge_session);
    assert!(
        with_judge
            .state(with_judge_session)
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
    admit_all(&seat, touched);
    admit_all(&seat, untouched);
    seat.stage(touched, SessionControlChange::Mode(Mode::Plan))
        .expect("plan is offered");
    assert_eq!(
        seat.state(touched).expect("a readable seat").mode.current,
        Some(Mode::Plan)
    );
    assert_eq!(
        seat.state(untouched).expect("a readable seat").mode.current,
        Some(Mode::Default)
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
        Some(ModelRef("ollama/gemma4".to_owned())),
        Some(ThinkingLevel::Low),
        Mode::Default,
        false,
        Confinement::Workspace,
        None,
    );
    let session = SessionId::new();
    admit_all(&seat, session);
    let state = seat.state(session).expect("a readable seat");
    assert_eq!(
        state.model.available,
        vec![ModelRef("ollama/gemma4".to_owned())]
    );
    assert_eq!(state.effort.available, vec![ThinkingLevel::Low]);
    assert!(state.mode.available.contains(&Mode::Plan));
}

#[test]
fn an_unconfigured_seat_can_select_a_model_before_selecting_unknown_effort() {
    let seat = AcpSessionControls::seeded(
        None,
        None,
        Mode::Default,
        false,
        Confinement::Workspace,
        Some(catalogue()),
    );
    let session = SessionId::new();
    admit_all(&seat, session);
    let initial = seat
        .state(session)
        .expect("an unconfigured seat is reachable");
    assert_eq!(initial.model.current, None);
    assert_eq!(initial.effort.current, None);
    assert_eq!(initial.model.chosen, None);
    assert_eq!(initial.effort.chosen, None);

    let selected = seat
        .stage(
            session,
            SessionControlChange::Model(ModelRef("ollama/gemma4".to_owned())),
        )
        .expect("the catalogue offers the model");
    assert_eq!(
        selected.model.current,
        Some(ModelRef("ollama/gemma4".to_owned()))
    );
    assert_eq!(
        selected.model.chosen,
        Some(ModelRef("ollama/gemma4".to_owned()))
    );
    assert_eq!(selected.effort.current, None);
    assert_eq!(selected.effort.chosen, None);
    assert_eq!(
        selected.effort.available,
        vec![ThinkingLevel::Off, ThinkingLevel::Low]
    );

    let chosen = seat
        .stage(session, SessionControlChange::Effort(ThinkingLevel::Low))
        .expect("a known effort can now be chosen");
    assert_eq!(chosen.effort.current, Some(ThinkingLevel::Low));
    assert_eq!(chosen.effort.chosen, Some(ThinkingLevel::Low));
}

#[test]
fn a_known_model_default_is_current_without_becoming_a_choice() {
    let seat = AcpSessionControls::seeded(
        Some(ModelRef("ollama/gemma4".to_owned())),
        None,
        Mode::Default,
        false,
        Confinement::Workspace,
        Some(catalogue_with_default(ThinkingLevel::Medium)),
    );
    let session = SessionId::new();
    admit_all(&seat, session);
    let state = seat.state(session).expect("a readable seat");
    assert_eq!(state.effort.current, Some(ThinkingLevel::Medium));
    assert_eq!(state.effort.chosen, None);
}

#[test]
fn an_incompatible_explicit_effort_refuses_model_change_atomically() {
    let seat = AcpSessionControls::seeded(
        Some(ModelRef("anthropic/claude-sonnet-5".to_owned())),
        Some(ThinkingLevel::High),
        Mode::Default,
        false,
        Confinement::Workspace,
        Some(catalogue()),
    );
    let session = SessionId::new();
    admit_all(&seat, session);
    let before = seat.state(session).expect("a readable seat");
    let refusal = seat
        .stage(
            session,
            SessionControlChange::Model(ModelRef("ollama/gemma4".to_owned())),
        )
        .expect_err("an explicit effort incompatible with the target must refuse");
    assert!(matches!(
        refusal,
        ControlError::Refused {
            control: "model",
            ..
        }
    ));
    assert_eq!(seat.state(session).expect("state remains readable"), before);
}

#[test]
fn an_unstated_effort_derives_the_target_model_default_without_chosen_effort() {
    let seat = AcpSessionControls::seeded(
        Some(ModelRef("anthropic/claude-sonnet-5".to_owned())),
        None,
        Mode::Default,
        false,
        Confinement::Workspace,
        Some(catalogue_with_default(ThinkingLevel::Low)),
    );
    let session = SessionId::new();
    admit_all(&seat, session);
    let before = seat.state(session).expect("a readable seat");
    assert_eq!(before.effort.current, None);
    let after = seat
        .stage(
            session,
            SessionControlChange::Model(ModelRef("ollama/gemma4".to_owned())),
        )
        .expect("a model change may derive its known default");
    assert_eq!(after.effort.current, Some(ThinkingLevel::Low));
    assert_eq!(after.effort.chosen, None);
}

#[test]
fn a_new_model_with_unknown_default_does_not_reuse_the_base_resolved_effort() {
    let seat = AcpSessionControls::seeded(
        Some(ModelRef("ollama/gemma4".to_owned())),
        None,
        Mode::Default,
        false,
        Confinement::Workspace,
        Some(catalogue_with_default(ThinkingLevel::Medium)),
    );
    let session = SessionId::new();
    admit_all(&seat, session);
    let before = seat.state(session).expect("a readable seat");
    assert_eq!(before.effort.current, Some(ThinkingLevel::Medium));
    assert_eq!(before.effort.chosen, None);

    let after = seat
        .stage(
            session,
            SessionControlChange::Model(ModelRef("anthropic/claude-sonnet-5".to_owned())),
        )
        .expect("the target model is available");
    assert_eq!(
        after.model.chosen,
        Some(ModelRef("anthropic/claude-sonnet-5".to_owned()))
    );
    assert_eq!(
        after.effort.current, None,
        "an unknown target default must clear the previous model's resolved effort"
    );
    assert_eq!(after.effort.chosen, None);
}

/// A reconnecting host is offered configuration again, not the last
/// connection's pick.
///
/// The seat is the connection's, and a new connection builds a new one. That
/// is the decision and not an omission: a durable per-session model would make
/// resume order-dependent, which
/// `docs/decisions/2026-08-30-skill-invocation-preserves-one-authority-epoch.md`
/// rejects, and the terminal seat is bound by the same rule — a session-only
/// pick lives in memory for that session only. What survives a disconnect is
/// the hold, which is a fact about the session, not a preference about the
/// next generation.
///
/// So the reconnected host must be told `chosen: None` rather than inherit a
/// choice it cannot see the provenance of. Reporting the earlier pick as
/// `chosen` would claim this host picked it.
#[test]
fn a_reconnecting_seat_offers_configuration_and_not_the_previous_pick() {
    let session = SessionId::new();
    let first = seat(false);
    admit_all(&first, session);
    first
        .stage(session, SessionControlChange::Mode(Mode::Plan))
        .expect("plan is offered");
    assert_eq!(
        first.state(session).expect("a readable seat").mode.chosen,
        Some(Mode::Plan),
        "the connection that picked sees its own pick"
    );

    // The host reconnects: same session, a seat this connection owns.
    let reconnected = seat(false);
    admit_all(&reconnected, session);
    let state = reconnected.state(session).expect("a readable seat");
    assert_eq!(
        state.mode.chosen, None,
        "a pick this connection did not make is not its choice"
    );
    assert_eq!(
        state.mode.current,
        Some(Mode::Default),
        "and the run uses what configuration resolves"
    );
}

/// Two connections picking on one session at the same moment do not disturb
/// each other.
///
/// There is no shared map to race on, and that is the design rather than an
/// accident: the seat belongs to the connection, so each host's pick is its
/// own and the run submitted through a connection uses that connection's. The
/// race is worth running anyway, because "no shared state" is a claim about
/// the implementation and this is the assertion that keeps it true — a seat
/// that later grew a process-wide cache would make one host's pick silently
/// retune another's window.
#[test]
fn two_connections_picking_at_once_each_keep_their_own() {
    let session = SessionId::new();
    let first = seat(false);
    let second = seat(false);
    admit_all(&first, session);
    admit_all(&second, session);

    std::thread::scope(|scope| {
        scope.spawn(|| {
            for _ in 0..64 {
                first
                    .stage(session, SessionControlChange::Mode(Mode::Plan))
                    .expect("plan is offered");
            }
        });
        scope.spawn(|| {
            for _ in 0..64 {
                second
                    .stage(session, SessionControlChange::Mode(Mode::Edits))
                    .expect("edits is offered");
            }
        });
    });

    assert_eq!(
        first.state(session).expect("a readable seat").mode.chosen,
        Some(Mode::Plan),
        "the first host's window was retuned by the second"
    );
    assert_eq!(
        second.state(session).expect("a readable seat").mode.chosen,
        Some(Mode::Edits),
        "the second host's window was retuned by the first"
    );
}

// --- the confinement ceiling ---

/// The offer is everything at least as strong as the ceiling, and nothing
/// weaker.
///
/// This is the whole enforcement. `controls::change` admits a value because it
/// appears in `available` and for no other reason, so a confinement that is
/// absent here cannot be reached by any message — there is no second rule that
/// could come to disagree with this one.
#[test]
fn the_offer_stops_at_the_ceiling_the_process_was_started_with() {
    let session = SessionId::new();
    for (ceiling, offered) in [
        (
            Confinement::Off,
            vec![Confinement::Ro, Confinement::Workspace, Confinement::Off],
        ),
        (
            Confinement::Workspace,
            vec![Confinement::Ro, Confinement::Workspace],
        ),
        (Confinement::Ro, vec![Confinement::Ro]),
    ] {
        let seat = ceilinged(false, ceiling);
        admit_all(&seat, session);
        let state = seat.state(session).expect("a readable seat");
        assert_eq!(
            state.sandbox.available, offered,
            "the offer under a `{ceiling}` ceiling"
        );
        assert_eq!(
            state.sandbox.current,
            Some(ceiling),
            "a session starts on the ceiling"
        );
        assert_eq!(
            state.sandbox.chosen, None,
            "inheriting the ceiling is not a choice anybody made"
        );
    }
}

/// A host may confine itself further than the operator required.
#[test]
fn a_host_can_tighten_the_confinement_and_the_pick_is_recorded_as_one() {
    let seat = ceilinged(false, Confinement::Workspace);
    let session = SessionId::new();
    admit_all(&seat, session);
    let after = seat
        .stage(session, SessionControlChange::Sandbox(Confinement::Ro))
        .expect("`ro` is stronger than the `workspace` ceiling");
    assert_eq!(after.sandbox.current, Some(Confinement::Ro));
    assert_eq!(
        after.sandbox.chosen,
        Some(Confinement::Ro),
        "a pick has to travel as a pick: only `chosen` reaches the run"
    );
}

/// It may not loosen it, and the refusal says whose decision it was.
///
/// `Refused`, not `Unavailable`: `off` is a real confinement this build
/// supports, and telling the host to pick from the list would suggest the list
/// is merely incomplete rather than deliberately capped by the person who
/// started the process.
#[test]
fn a_host_cannot_loosen_the_confinement_past_the_ceiling() {
    let seat = ceilinged(false, Confinement::Ro);
    let session = SessionId::new();
    admit_all(&seat, session);
    let error = seat
        .stage(session, SessionControlChange::Sandbox(Confinement::Off))
        .expect_err("`off` is weaker than the `ro` ceiling");
    match &error {
        ControlError::Refused { control, why } => {
            assert_eq!(*control, "sandbox");
            assert!(
                why.contains("ro"),
                "the refusal does not name the ceiling in force: {why}"
            );
        }
        other => panic!("a capped confinement should be refused, not {other}"),
    }
    assert_eq!(
        seat.state(session).expect("a readable seat").sandbox.chosen,
        None,
        "a refused pick was staged anyway"
    );
}

/// Every ceiling offers itself, so the picker's current value is always one of
/// its choices. A client cannot render a select whose current value is absent
/// from the options, and `ro` — the tightest — is the case that would break
/// first if the filter were ever written the other way round.
#[test]
fn the_value_in_force_is_always_one_of_the_choices() {
    let session = SessionId::new();
    for ceiling in [Confinement::Ro, Confinement::Workspace, Confinement::Off] {
        let seat = ceilinged(false, ceiling);
        admit_all(&seat, session);
        let state = seat.state(session).expect("a readable seat");
        assert!(
            state
                .sandbox
                .current
                .is_some_and(|current| state.sandbox.available.contains(&current)),
            "a `{ceiling}` ceiling offers a picker its own current value cannot \
             be found in: {:?}",
            state.sandbox.available
        );
    }
}

#[test]
fn measured_confinement_offer_intersects_the_ceiling_and_keeps_typed_reasons() {
    let seat = ceilinged(false, Confinement::Workspace);
    let session = SessionId::new();
    let workspace_missing = missing(Confinement::Workspace);
    seat.seed_confinement(
        session,
        vec![
            (Confinement::Ro, observation(Confinement::Ro)),
            // `off` is measured, but it is weaker than the operator ceiling.
            (Confinement::Off, observation(Confinement::Off)),
        ],
        vec![(Confinement::Workspace, workspace_missing.clone())],
    )
    .expect("the first measured offer seeds the session");

    let state = seat.state(session).expect("a readable seat");
    assert_eq!(
        state.sandbox.available,
        vec![Confinement::Ro],
        "the offer is measured capabilities intersected with the ceiling"
    );
    assert_eq!(
        state.sandbox.current,
        Some(Confinement::Workspace),
        "an unsupported configured default remains current so refusal stays truthful"
    );
    let refusal = seat
        .stage(
            session,
            SessionControlChange::Sandbox(Confinement::Workspace),
        )
        .expect_err("a measured-but-unsupported confinement must refuse");
    assert_eq!(
        refusal,
        ControlError::MissingCapabilities {
            control: "sandbox",
            value: "workspace".to_owned(),
            missing: Box::new(workspace_missing),
        }
    );
}

#[test]
fn confinement_measurements_are_seeded_once_without_duplicate_rows_or_reseed() {
    let seat = ceilinged(false, Confinement::Workspace);
    let session = SessionId::new();
    assert!(
        seat.seed_confinement(
            session,
            vec![
                (Confinement::Ro, observation(Confinement::Ro)),
                (Confinement::Ro, observation(Confinement::Ro)),
            ],
            Vec::new(),
        )
        .is_err(),
        "duplicate supported measurements must not be silently collapsed"
    );
    seat.seed_confinement(
        session,
        vec![(Confinement::Ro, observation(Confinement::Ro))],
        Vec::new(),
    )
    .expect("a unique measured offer seeds the session");
    assert!(
        seat.seed_confinement(
            session,
            vec![(Confinement::Ro, observation(Confinement::Ro))],
            Vec::new(),
        )
        .is_err(),
        "a session must not be reseeded after admission"
    );
}

// Catches: a live ACP selection bypasses system max_mode after startup resolution.
#[test]
fn acp_mode_selection_cannot_raise_system_maximum() {
    use ego_core::config::{ConfigLayer, Source, Trust, resolve};

    let system = ConfigLayer::parse(Source::System, "max_mode = 'default'").unwrap();
    let effective = resolve(&[system], Trust::Untrusted, None).unwrap();
    let seat = AcpSessionControls::new(&effective, Confinement::Workspace, None);
    let session = SessionId::new();
    admit_all(&seat, session);

    let result = seat.stage(session, SessionControlChange::Mode(Mode::Yolo));
    assert!(
        result.is_err(),
        "ACP accepted yolo above system max_mode=default"
    );
    let state = seat.state(session).unwrap();
    assert_eq!(state.mode.current, Some(Mode::Default));
    assert_eq!(state.mode.chosen, None);
}
