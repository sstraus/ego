//! contract for the shared catalogue picker and atomic user-config port.

use std::sync::{Arc, Mutex};

use ego_ai::catalogue::{
    FactFreshness, ModelAvailability, ModelAvailabilityCause, ModelCatalogueEntry,
    ModelCatalogueSnapshot, ModelCatalogueSourceStatus, ModelFactProvenance, ModelSlug,
    SuppliedAuthority,
};
use ego_cli::model::GenerationSelection;
use ego_cli::model_picker::{
    ModelPicker, PickerError, PickerFocus, PickerInput, PickerInputOutcome, PickerOutcome,
};
use ego_cli::repl::actions::{ActionId, ActionSink, LocalAction, LocalActionOutcome};
use ego_core::config::{
    ConfigLayer, ModelRef, RuleSnapshot, Source, UserConfigError, UserConfigMutation,
    UserConfigMutationFacts, UserConfigMutationResult, UserConfigPublication, UserConfigSnapshot,
    UserConfigStore, UserConfigTarget,
};
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ImageInputSupport, ModelCapabilities, ReasoningCarryover,
    ReasoningEffortSupport, Transport,
};
use ego_core::generator::types::ThinkingLevel;
use ego_core::public::MeteringKind;

fn capabilities() -> ModelCapabilities {
    ModelCapabilities {
        schema_version: ModelCapabilities::SCHEMA_VERSION,
        max_context_tokens: Some(128_000),
        max_output_tokens: Some(16_000),
        max_tools: Some(128),
        supports_parallel_tool_calls: true,
        reasoning_carryover: ReasoningCarryover::Portable,
        reasoning_efforts: ReasoningEffortSupport::Any,
        reasoning_state_dialect: None,
        cancel: CancelSemantics::Cooperative,
        cache: CacheRetention::Short,
        transport: Transport::Sse,
        image_input: ImageInputSupport::Unknown,
        default_reasoning_effort: None,
    }
}

fn entry(
    slug: &str,
    provenance: ModelFactProvenance,
    metering: MeteringKind,
    availability: ModelAvailability,
) -> ModelCatalogueEntry {
    ModelCatalogueEntry {
        slug: ModelSlug::parse(slug).unwrap(),
        capabilities: capabilities(),
        metering,
        provenance,
        freshness: FactFreshness::Current,
        availability,
    }
}

fn source(
    provider: &str,
    provenance: ModelFactProvenance,
    availability: ModelAvailability,
) -> ModelCatalogueSourceStatus {
    ModelCatalogueSourceStatus {
        provider: provider.into(),
        provenance,
        freshness: FactFreshness::Current,
        availability,
    }
}

fn catalogue() -> Arc<ModelCatalogueSnapshot> {
    let openai = ModelFactProvenance::supplied(SuppliedAuthority::System);
    let ollama = ModelFactProvenance::discovered("ollama");
    Arc::new(ModelCatalogueSnapshot::from_entries_and_sources(
        vec![
            entry(
                "openai/gpt-5.6",
                openai.clone(),
                MeteringKind::Currency,
                ModelAvailability::Available,
            ),
            entry(
                "ollama/qwen3",
                ollama.clone(),
                MeteringKind::Unmetered,
                ModelAvailability::Available,
            ),
        ],
        vec![
            source("openai", openai, ModelAvailability::Available),
            source("ollama", ollama, ModelAvailability::Available),
        ],
    ))
}

fn catalogue_with_default(default: Option<ThinkingLevel>) -> Arc<ModelCatalogueSnapshot> {
    let mut snapshot = (*catalogue()).clone();
    snapshot
        .models
        .iter_mut()
        .find(|row| row.slug.to_string() == "openai/gpt-5.6")
        .expect("catalogue fixture includes openai/gpt-5.6")
        .capabilities
        .default_reasoning_effort = default;
    Arc::new(snapshot)
}

#[derive(Clone)]
struct RecordingUserConfigStore {
    mutations: Arc<Mutex<Vec<UserConfigMutation>>>,
    publication: Result<UserConfigPublication, UserConfigError>,
}

impl RecordingUserConfigStore {
    fn new(publication: Result<UserConfigPublication, UserConfigError>) -> Self {
        Self {
            mutations: Arc::new(Mutex::new(Vec::new())),
            publication,
        }
    }

    fn mutations(&self) -> Vec<UserConfigMutation> {
        self.mutations.lock().unwrap().clone()
    }
}

impl UserConfigStore for RecordingUserConfigStore {
    fn snapshot(&self, target: UserConfigTarget) -> Result<UserConfigSnapshot, UserConfigError> {
        Ok(UserConfigSnapshot {
            values: ego_core::config::ManagedConfigKey::for_target(target.clone())
                .iter()
                .copied()
                .map(|key| (key, None))
                .collect(),
            target,
        })
    }

    fn rule_snapshot(&self, target: UserConfigTarget) -> Result<RuleSnapshot, UserConfigError> {
        Ok(RuleSnapshot {
            target,
            permissions_present: false,
            allow: Vec::new(),
            deny: Vec::new(),
        })
    }

    fn mutate(
        &self,
        mutation: UserConfigMutation,
    ) -> Result<UserConfigMutationResult, UserConfigError> {
        self.mutations.lock().unwrap().push(mutation);
        self.publication
            .clone()
            .map(|publication| UserConfigMutationResult {
                target: UserConfigTarget::Base,
                facts: UserConfigMutationFacts::Set {
                    key: ego_core::config::ManagedConfigKey::Bare,
                    value: ego_core::config::ManagedConfigValue::parse(
                        ego_core::config::ManagedConfigKey::Bare,
                        "true",
                    )
                    .unwrap(),
                },
                changed: true,
                publication,
            })
    }
}

fn selection() -> Arc<GenerationSelection> {
    Arc::new(GenerationSelection::new(
        ModelRef("openai/gpt-5.6".into()),
        ThinkingLevel::Medium,
    ))
}

fn assert_selection(selection: &GenerationSelection, model: &str, thinking: ThinkingLevel) {
    let snapshot = selection
        .snapshot()
        .expect("the shared generation selection is readable");
    assert_eq!(snapshot.identity, ModelRef(model.into()));
    assert_eq!(snapshot.thinking, thinking);
    assert_eq!(snapshot.explicit_thinking, Some(thinking));
}

fn open_model_picker(picker: &mut ModelPicker) {
    assert_eq!(
        ActionSink::invoke(
            picker,
            LocalAction {
                id: ActionId::Model,
                busy: false,
            },
        ),
        LocalActionOutcome::Completed
    );
}

fn choose_openai_high(picker: &mut ModelPicker) {
    open_model_picker(picker);
    assert_eq!(
        picker.input(PickerInput::Number(2)).unwrap(),
        PickerInputOutcome::Consumed
    );
    assert_eq!(
        picker.view().current_id.as_deref(),
        Some("openai/gpt-5.6"),
        "highlight must expose the selected model identity"
    );
    assert_eq!(
        picker.input(PickerInput::Enter).unwrap(),
        PickerInputOutcome::FocusChanged(PickerFocus::Reasoning)
    );
    assert_eq!(
        picker.input(PickerInput::Number(5)).unwrap(),
        PickerInputOutcome::Consumed
    );
}

#[test]
fn reasoning_modal_starts_on_the_model_default_when_one_is_known() {
    let store = RecordingUserConfigStore::new(Ok(UserConfigPublication::PublishedDurably));
    let selection = selection();
    let mut picker = ModelPicker::for_selection(
        catalogue_with_default(Some(ThinkingLevel::Medium)),
        Arc::clone(&selection),
        Arc::new(store),
    );
    open_model_picker(&mut picker);
    picker
        .input(PickerInput::Number(2))
        .expect("select the OpenAI row");
    assert_eq!(
        picker.input(PickerInput::Enter).unwrap(),
        PickerInputOutcome::FocusChanged(PickerFocus::Reasoning)
    );
    assert_eq!(
        picker.view().current_id.as_deref(),
        Some("medium"),
        "a known model default is the initial reasoning highlight"
    );
}

#[test]
fn unknown_reasoning_default_requires_a_real_arrow_or_number_before_enter() {
    let store = RecordingUserConfigStore::new(Ok(UserConfigPublication::PublishedDurably));
    let selection = selection();
    let mut picker = ModelPicker::for_selection(
        catalogue_with_default(None),
        Arc::clone(&selection),
        Arc::new(store),
    );
    open_model_picker(&mut picker);
    picker
        .input(PickerInput::Number(2))
        .expect("select the OpenAI row");
    assert_eq!(
        picker.input(PickerInput::Enter).unwrap(),
        PickerInputOutcome::FocusChanged(PickerFocus::Reasoning)
    );
    assert_eq!(picker.view().current_id, None);
    assert_eq!(
        picker.input(PickerInput::Enter),
        Err(PickerError::NoSelection),
        "unknown default must not silently choose first, last, or Off"
    );
    assert_eq!(picker.view().current_id, None);
    assert_eq!(
        picker.input(PickerInput::Number(5)).unwrap(),
        PickerInputOutcome::Consumed
    );
    assert_eq!(picker.view().current_id.as_deref(), Some("high"));
}

#[test]
fn a_human_picker_choice_of_off_remains_explicit() {
    let store = RecordingUserConfigStore::new(Ok(UserConfigPublication::PublishedDurably));
    let selection = selection();
    let mut picker = ModelPicker::for_selection(
        catalogue_with_default(Some(ThinkingLevel::Medium)),
        Arc::clone(&selection),
        Arc::new(store),
    );
    open_model_picker(&mut picker);
    picker
        .input(PickerInput::Number(2))
        .expect("select the OpenAI row");
    picker
        .input(PickerInput::Enter)
        .expect("open the reasoning selector");
    picker
        .input(PickerInput::Number(1))
        .expect("select explicit Off");
    let PickerInputOutcome::Closed(PickerOutcome::ModelSwitch(request)) = picker
        .input(PickerInput::SessionOnly)
        .expect("session-only Off selection")
    else {
        panic!("the picker must return a typed model switch");
    };
    assert_eq!(request.thinking(), ThinkingLevel::Off);
}

fn catalogue_with_openai(
    availability: ModelAvailability,
    reasoning: ReasoningEffortSupport,
) -> Arc<ModelCatalogueSnapshot> {
    let mut snapshot = (*catalogue()).clone();
    let row = snapshot
        .models
        .iter_mut()
        .find(|row| row.slug.to_string() == "openai/gpt-5.6")
        .expect("catalogue fixture includes openai/gpt-5.6");
    row.availability = availability;
    row.capabilities.reasoning_efforts = reasoning;
    Arc::new(snapshot)
}

#[test]
fn picker_and_models_use_one_snapshot_identity_order_and_availability() {
    let snapshot = catalogue();
    let picker = ModelPicker::new(Arc::clone(&snapshot));

    assert!(Arc::ptr_eq(picker.snapshot(), &snapshot));
    assert_eq!(picker.entries(), snapshot.models.as_slice());
    assert_eq!(picker.sources(), snapshot.sources.as_slice());
    assert_eq!(snapshot.v, 2);
    assert_eq!(
        snapshot
            .models
            .iter()
            .map(|row| row.slug.to_string())
            .collect::<Vec<_>>(),
        ["ollama/qwen3", "openai/gpt-5.6"],
    );
    assert_eq!(
        snapshot
            .sources
            .iter()
            .map(|source| source.provider.as_str())
            .collect::<Vec<_>>(),
        ["ollama", "openai"],
    );
    assert!(snapshot.models.iter().all(|row| match row.availability {
        ModelAvailability::Available => true,
        ModelAvailability::Unavailable { .. } => !picker.can_select(&row.slug),
    }));
}

#[test]
fn picker_does_not_define_a_second_model_row_or_reorder_shared_entries() {
    let snapshot = catalogue();
    let picker = ModelPicker::new(Arc::clone(&snapshot));
    assert_eq!(picker.row_count(), snapshot.models.len());
    assert_eq!(
        picker.slugs(),
        snapshot
            .models
            .iter()
            .map(|row| row.slug.clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        picker.reasoning_efforts(),
        snapshot
            .models
            .iter()
            .map(|row| row.capabilities.reasoning_efforts.clone())
            .collect::<Vec<_>>()
    );
}

#[test]
fn absent_thinking_flag_preserves_file_value_and_unknown_keys_fail_strictly() {
    let file = ConfigLayer::parse(
        Source::User,
        "model = 'openai/gpt-5.6'\nthinking = 'high'\n",
    )
    .unwrap();
    let no_flag = ego_cli::config::flags_layer(None, None);
    let effective =
        ego_core::config::resolve(&[file, no_flag], ego_core::config::Trust::Untrusted, None)
            .unwrap();
    assert_eq!(effective.thinking, Some(ThinkingLevel::High));

    let unknown =
        ConfigLayer::parse(Source::User, "model = 'openai/gpt-5.6'\nunknown = true\n").unwrap_err();
    assert!(unknown.to_string().contains("unknown"));
}

#[test]
fn cli_model_and_thinking_override_persisted_picker_defaults() {
    let persisted = ConfigLayer::parse(
        Source::User,
        "model = 'openai/gpt-5.6'\nthinking = 'medium'\n",
    )
    .unwrap();
    let flags = ego_cli::config::flags_layer(Some("ollama/qwen3"), Some(ThinkingLevel::High));
    let effective = ego_core::config::resolve(
        &[persisted, flags],
        ego_core::config::Trust::Untrusted,
        None,
    )
    .unwrap();

    assert_eq!(effective.model, Some(ModelRef("ollama/qwen3".to_owned())));
    assert_eq!(effective.thinking, Some(ThinkingLevel::High));
}

#[test]
fn escape_and_session_only_stage_without_user_config_mutation() {
    let store = RecordingUserConfigStore::new(Ok(UserConfigPublication::PublishedDurably));
    let escape_selection = selection();
    let mut picker = ModelPicker::for_selection(
        catalogue(),
        Arc::clone(&escape_selection),
        Arc::new(store.clone()),
    );
    open_model_picker(&mut picker);
    assert_eq!(
        picker.input(PickerInput::Escape).unwrap(),
        PickerInputOutcome::Closed(PickerOutcome::Cancelled)
    );
    assert!(store.mutations().is_empty());
    assert_selection(&escape_selection, "openai/gpt-5.6", ThinkingLevel::Medium);

    let store = RecordingUserConfigStore::new(Ok(UserConfigPublication::PublishedDurably));
    let session_selection = selection();
    let mut picker = ModelPicker::for_selection(
        catalogue(),
        Arc::clone(&session_selection),
        Arc::new(store.clone()),
    );
    choose_openai_high(&mut picker);
    let PickerInputOutcome::Closed(PickerOutcome::ModelSwitch(request)) =
        picker.input(PickerInput::SessionOnly).unwrap()
    else {
        panic!("session-only selection must stage a model-switch request");
    };
    assert_eq!(request.target(), ModelRef("openai/gpt-5.6".into()));
    assert_eq!(request.thinking(), ThinkingLevel::High);
    assert!(!request.is_persistent());
    assert!(store.mutations().is_empty());
    assert_selection(&session_selection, "openai/gpt-5.6", ThinkingLevel::Medium);
}

#[test]
fn enter_stages_one_typed_request_without_mutating_memory() {
    let store = RecordingUserConfigStore::new(Ok(UserConfigPublication::PublishedDurably));
    let selection = selection();
    let mut picker =
        ModelPicker::for_selection(catalogue(), Arc::clone(&selection), Arc::new(store.clone()));
    choose_openai_high(&mut picker);
    let PickerInputOutcome::Closed(PickerOutcome::ModelSwitch(request)) =
        picker.input(PickerInput::Enter).unwrap()
    else {
        panic!("persistent selection must stage a model-switch request");
    };
    assert_eq!(request.target(), ModelRef("openai/gpt-5.6".into()));
    assert_eq!(request.thinking(), ThinkingLevel::High);
    assert!(request.is_persistent());
    assert!(store.mutations().is_empty(), "run owner owns persistence");
    assert_selection(&selection, "openai/gpt-5.6", ThinkingLevel::Medium);
}

#[test]
fn failed_store_does_not_affect_picker_request_staging() {
    let store =
        RecordingUserConfigStore::new(Err(UserConfigError::new("injected pre-publish failure")));
    let selection = selection();
    let mut picker =
        ModelPicker::for_selection(catalogue(), Arc::clone(&selection), Arc::new(store.clone()));
    choose_openai_high(&mut picker);
    let PickerInputOutcome::Closed(PickerOutcome::ModelSwitch(request)) =
        picker.input(PickerInput::Enter).unwrap()
    else {
        panic!("picker must stage without calling the config store");
    };
    assert_eq!(request.target(), ModelRef("openai/gpt-5.6".into()));
    assert_eq!(request.thinking(), ThinkingLevel::High);
    assert!(request.is_persistent());
    assert!(store.mutations().is_empty());
    assert_selection(&selection, "openai/gpt-5.6", ThinkingLevel::Medium);
}

#[test]
fn uncertain_publication_is_owned_by_run_owner_after_staging() {
    let diagnostic = "directory barrier did not complete";
    let store =
        RecordingUserConfigStore::new(Ok(UserConfigPublication::PublishedDurabilityUncertain {
            diagnostic: diagnostic.into(),
        }));
    let selection = selection();
    let mut picker =
        ModelPicker::for_selection(catalogue(), Arc::clone(&selection), Arc::new(store.clone()));
    choose_openai_high(&mut picker);

    let PickerInputOutcome::Closed(PickerOutcome::ModelSwitch(request)) =
        picker.input(PickerInput::Enter).unwrap()
    else {
        panic!("picker must stage before publication uncertainty is known");
    };
    assert_eq!(request.target(), ModelRef("openai/gpt-5.6".into()));
    assert_eq!(request.thinking(), ThinkingLevel::High);
    assert!(request.is_persistent());
    assert!(store.mutations().is_empty(), "run owner owns publication");
    assert_selection(&selection, "openai/gpt-5.6", ThinkingLevel::Medium);
}

#[test]
fn action_sink_opener_is_single_flight_and_reopens_after_escape() {
    let selection = selection();
    let mut picker = ModelPicker::for_selection(
        catalogue(),
        selection,
        Arc::new(RecordingUserConfigStore::new(Ok(
            UserConfigPublication::PublishedDurably,
        ))),
    );
    open_model_picker(&mut picker);
    assert_eq!(
        ActionSink::invoke(
            &mut picker,
            LocalAction {
                id: ActionId::Model,
                busy: false,
            },
        ),
        LocalActionOutcome::InFlight
    );
    assert_eq!(
        picker.input(PickerInput::Escape).unwrap(),
        PickerInputOutcome::Closed(PickerOutcome::Cancelled)
    );
    open_model_picker(&mut picker);
    assert!(picker.is_open());
}

#[test]
fn thinking_action_opens_current_model_and_current_effort() {
    let store = RecordingUserConfigStore::new(Ok(UserConfigPublication::PublishedDurably));
    let selection = selection();
    let mut picker =
        ModelPicker::for_selection(catalogue(), Arc::clone(&selection), Arc::new(store.clone()));
    assert_eq!(
        ActionSink::invoke(
            &mut picker,
            LocalAction {
                id: ActionId::Thinking,
                busy: false,
            },
        ),
        LocalActionOutcome::Completed
    );
    assert_eq!(picker.view().focus, PickerFocus::Reasoning);
    assert_eq!(
        picker.view().selected_model.as_deref(),
        Some("openai/gpt-5.6")
    );
    assert_eq!(picker.view().current_id.as_deref(), Some("medium"));
    let PickerInputOutcome::Closed(PickerOutcome::ModelSwitch(request)) =
        picker.input(PickerInput::SessionOnly).unwrap()
    else {
        panic!("session-only thinking selection must stage a request");
    };
    assert_eq!(request.target(), ModelRef("openai/gpt-5.6".into()));
    assert_eq!(request.thinking(), ThinkingLevel::Medium);
    assert!(!request.is_persistent());
    assert!(store.mutations().is_empty());
    assert_selection(&selection, "openai/gpt-5.6", ThinkingLevel::Medium);
}

#[test]
fn repeated_model_and_thinking_actions_are_in_flight_while_picker_is_open() {
    let selection = selection();
    let mut picker = ModelPicker::for_selection(
        catalogue(),
        selection,
        Arc::new(RecordingUserConfigStore::new(Ok(
            UserConfigPublication::PublishedDurably,
        ))),
    );
    assert_eq!(
        ActionSink::invoke(
            &mut picker,
            LocalAction {
                id: ActionId::Thinking,
                busy: false,
            },
        ),
        LocalActionOutcome::Completed
    );
    for id in [ActionId::Model, ActionId::Thinking] {
        assert_eq!(
            ActionSink::invoke(&mut picker, LocalAction { id, busy: false },),
            LocalActionOutcome::InFlight
        );
    }
    assert_eq!(
        picker.input(PickerInput::Escape).unwrap(),
        PickerInputOutcome::Closed(PickerOutcome::Cancelled)
    );
}

#[test]
fn thinking_refuses_closed_with_typed_unavailable_diagnostic() {
    let selection = selection();
    let mut picker = ModelPicker::for_selection(
        catalogue_with_openai(
            ModelAvailability::Unavailable {
                cause: ModelAvailabilityCause::SourceUnreachable,
                message: "provider unavailable".into(),
            },
            ReasoningEffortSupport::Any,
        ),
        selection,
        Arc::new(RecordingUserConfigStore::new(Ok(
            UserConfigPublication::PublishedDurably,
        ))),
    );
    assert_eq!(
        ActionSink::invoke(
            &mut picker,
            LocalAction {
                id: ActionId::Thinking,
                busy: false,
            },
        ),
        LocalActionOutcome::Refused {
            reason: "the current model has no selectable reasoning authority",
        }
    );
    assert!(!picker.is_open());
    assert!(matches!(
        picker.diagnostic(),
        Some(PickerError::Unavailable { slug, .. }) if slug == "openai/gpt-5.6"
    ));
}

#[test]
fn thinking_refuses_closed_with_typed_unknown_effort_diagnostic() {
    let selection = selection();
    let mut picker = ModelPicker::for_selection(
        catalogue_with_openai(
            ModelAvailability::Available,
            ReasoningEffortSupport::Unknown,
        ),
        selection,
        Arc::new(RecordingUserConfigStore::new(Ok(
            UserConfigPublication::PublishedDurably,
        ))),
    );
    assert!(matches!(
        ActionSink::invoke(
            &mut picker,
            LocalAction {
                id: ActionId::Thinking,
                busy: false,
            },
        ),
        LocalActionOutcome::Refused { .. }
    ));
    assert!(!picker.is_open());
    assert!(matches!(
        picker.diagnostic(),
        Some(PickerError::UnknownReasoningSupport { slug }) if slug == "openai/gpt-5.6"
    ));
}

#[test]
fn thinking_refuses_closed_with_typed_absent_model_diagnostic() {
    let selection = Arc::new(GenerationSelection::new(
        ModelRef("missing/model".into()),
        ThinkingLevel::Medium,
    ));
    let mut picker = ModelPicker::for_selection(
        catalogue(),
        selection,
        Arc::new(RecordingUserConfigStore::new(Ok(
            UserConfigPublication::PublishedDurably,
        ))),
    );
    assert!(matches!(
        ActionSink::invoke(
            &mut picker,
            LocalAction {
                id: ActionId::Thinking,
                busy: false,
            },
        ),
        LocalActionOutcome::Refused { .. }
    ));
    assert!(!picker.is_open());
    assert!(matches!(
        picker.diagnostic(),
        Some(PickerError::UnknownModel { slug }) if slug == "missing/model"
    ));
}

#[test]
fn replacing_the_snapshot_updates_picker_availability_without_a_second_catalogue() {
    let selection = Arc::new(GenerationSelection::new(
        ModelRef("ollama/qwen3".into()),
        ThinkingLevel::Low,
    ));
    let mut picker = ModelPicker::for_selection(
        catalogue(),
        selection,
        Arc::new(RecordingUserConfigStore::new(Ok(
            UserConfigPublication::PublishedDurably,
        ))),
    );
    let mut unavailable = (*catalogue()).clone();
    unavailable
        .models
        .iter_mut()
        .find(|row| row.slug.to_string() == "ollama/qwen3")
        .expect("catalogue fixture includes ollama/qwen3")
        .availability = ModelAvailability::Unavailable {
        cause: ModelAvailabilityCause::SourceUnreachable,
        message: "provider unavailable".into(),
    };
    let unavailable = Arc::new(unavailable);
    assert_eq!(
        picker.replace_snapshot(Arc::clone(&unavailable)),
        LocalActionOutcome::Completed
    );
    assert!(Arc::ptr_eq(picker.snapshot(), &unavailable));
    assert!(!picker.can_select(&ModelSlug::parse("ollama/qwen3").unwrap()));
}

fn picker_on_current_openai() -> ModelPicker {
    ModelPicker::for_selection(
        catalogue(),
        selection(),
        Arc::new(RecordingUserConfigStore::new(Ok(
            UserConfigPublication::PublishedDurably,
        ))),
    )
}

fn type_text(picker: &mut ModelPicker, text: &str) {
    for character in text.chars() {
        let input = match character.to_digit(10) {
            Some(digit) => PickerInput::Number(digit as usize),
            None => PickerInput::Char(character),
        };
        picker.input(input).expect("typed key");
    }
}

#[test]
fn the_model_list_opens_on_the_current_model_and_names_it() {
    // Catches: a cursor fixed on row 1, which is `ollama/qwen3` here while the
    // session runs `openai/gpt-5.6` on row 2.
    let mut picker = picker_on_current_openai();
    open_model_picker(&mut picker);
    let view = picker.view();
    assert_eq!(view.current_id.as_deref(), Some("openai/gpt-5.6"));
    assert_eq!(view.current_model.as_deref(), Some("openai/gpt-5.6"));
}

#[test]
fn the_reasoning_step_names_the_current_effort_only_for_the_current_model() {
    // Catches: a `current` marker on the effort of a model the session is not
    // running, which would present another model's effort as this one's.
    let mut picker = picker_on_current_openai();
    open_model_picker(&mut picker);
    picker.input(PickerInput::Number(2)).unwrap();
    picker.input(PickerInput::Enter).unwrap();
    assert_eq!(
        picker.view().current_reasoning,
        Some(ThinkingLevel::Medium),
        "the current model's effort is marked"
    );
    picker.input(PickerInput::Escape).unwrap();
    open_model_picker(&mut picker);
    picker.input(PickerInput::Number(1)).unwrap();
    picker.input(PickerInput::Enter).unwrap();
    assert_eq!(picker.view().current_reasoning, None);
}

#[test]
fn typing_filters_by_substring_and_digits_select_only_while_the_filter_is_empty() {
    // Catches: type-to-search that swallows digits before the filter exists,
    // and digits that select a row once `gpt-5` is being typed.
    let mut picker = picker_on_current_openai();
    open_model_picker(&mut picker);
    picker.input(PickerInput::Number(1)).unwrap();
    assert_eq!(
        picker.view().current_id.as_deref(),
        Some("ollama/qwen3"),
        "an empty filter keeps digits selecting by number"
    );
    type_text(&mut picker, "GPT-5");
    let view = picker.view();
    assert_eq!(view.filter, "GPT-5");
    assert_eq!(
        view.models
            .iter()
            .map(|row| row.slug.to_string())
            .collect::<Vec<_>>(),
        ["openai/gpt-5.6"]
    );
}

#[test]
fn enter_on_zero_matches_does_nothing_and_never_takes_a_row_selected_before_filtering() {
    // Catches: Enter continuing with the row that was selected before the
    // filter emptied the list.
    let mut picker = picker_on_current_openai();
    open_model_picker(&mut picker);
    picker.input(PickerInput::Number(2)).unwrap();
    type_text(&mut picker, "zzz");
    assert!(picker.view().models.is_empty());
    assert_eq!(
        picker.input(PickerInput::Enter),
        Err(PickerError::NoSelection)
    );
    assert_eq!(picker.view().focus, PickerFocus::Model);
    for _ in 0..3 {
        picker.input(PickerInput::Backspace).unwrap();
    }
    assert_eq!(picker.view().models.len(), 2);
    assert_eq!(
        picker.input(PickerInput::Enter).unwrap(),
        PickerInputOutcome::FocusChanged(PickerFocus::Reasoning)
    );
}

#[test]
fn s_types_into_the_filter_at_the_model_step_and_means_this_session_at_the_reasoning_step() {
    // Catches: `s` stolen from the filter, or ignored at the reasoning step
    // where it saves for this session only.
    let mut picker = picker_on_current_openai();
    open_model_picker(&mut picker);
    type_text(&mut picker, "s");
    assert_eq!(picker.view().filter, "s");
    picker.input(PickerInput::Escape).unwrap();
    choose_openai_high(&mut picker);
    assert_eq!(
        picker.input(PickerInput::Char('x')).unwrap(),
        PickerInputOutcome::Consumed
    );
    let PickerInputOutcome::Closed(PickerOutcome::ModelSwitch(request)) =
        picker.input(PickerInput::Char('s')).unwrap()
    else {
        panic!("s at the reasoning step stages a session-only request");
    };
    assert!(!request.is_persistent());
}
