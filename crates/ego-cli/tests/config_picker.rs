//! RED contract for the shared catalogue picker and atomic user-config path.

use std::sync::Arc;

use ego_ai::catalogue::{FactFreshness, ModelAvailability};
use ego_core::config::{ConfigLayer, ModelRef, Source};
use ego_core::generator::types::ThinkingLevel;
use ego_cli::model::GenerationSelection;
use ego_cli::model_picker::{ModelPicker, PickerKey, PickerOutcome};

#[test]
fn picker_and_models_use_one_snapshot_identity_order_and_availability() {
    let snapshot = Arc::new(ego_cli::models::build_catalogue_snapshot());
    let picker = ModelPicker::new(Arc::clone(&snapshot));

    assert!(Arc::ptr_eq(picker.snapshot(), &snapshot));
    assert_eq!(picker.entries(), snapshot.models.as_slice());
    assert_eq!(picker.sources(), snapshot.sources.as_slice());
    assert_eq!(snapshot.v, 1);
    assert!(snapshot
        .models
        .windows(2)
        .all(|rows| rows[0].slug <= rows[1].slug));
    assert!(snapshot.models.iter().all(|row| match row.availability {
        ModelAvailability::Available => true,
        ModelAvailability::Unavailable { .. } => !picker.can_select(&row.slug),
    }));
    assert!(snapshot.models.iter().all(|row| {
        matches!(row.freshness, FactFreshness::Current | FactFreshness::Cached)
    }));
}

#[test]
fn picker_does_not_define_a_second_model_row_or_reorder_shared_entries() {
    let snapshot = ego_cli::models::build_catalogue_snapshot();
    let picker = ModelPicker::new(Arc::new(snapshot.clone()));
    assert_eq!(picker.row_count(), snapshot.models.len());
    assert_eq!(picker.slugs(), snapshot.models.iter().map(|row| row.slug.clone()).collect::<Vec<_>>());
    assert_eq!(picker.reasoning_efforts(), snapshot.models.iter().map(|row| row.capabilities.reasoning_efforts.clone()).collect::<Vec<_>>());
}

#[test]
fn absent_thinking_flag_preserves_file_value_and_unknown_keys_fail_strictly() {
    let file = ConfigLayer::parse(Source::User, "model = 'openai/gpt-5.6'\nthinking = 'high'\n").unwrap();
    let no_flag = ego_cli::config::flags_layer(None, None);
    let effective = ego_core::config::resolve(&[file, no_flag], ego_core::config::Trust::Untrusted, None).unwrap();
    assert_eq!(effective.thinking, Some(ThinkingLevel::High));

    let unknown = ConfigLayer::parse(Source::User, "model = 'openai/gpt-5.6'\nunknown = true\n").unwrap_err();
    assert!(unknown.to_string().contains("unknown"));
}

#[test]
fn picker_escape_changes_neither_memory_nor_disk_and_s_commits_memory_only() {
    let selection = Arc::new(GenerationSelection::new(
        ModelRef("openai/gpt-5.6".into()),
        ThinkingLevel::Medium,
    ));
    let mut picker = ModelPicker::for_selection(
        ego_cli::models::build_catalogue_snapshot(),
        Arc::clone(&selection),
        "# keep\nmodel = 'openai/gpt-5.6'\nthinking = 'medium'\nbare = true\n",
    );
    let before = picker.config_text();
    assert_eq!(picker.key(PickerKey::Escape), PickerOutcome::Cancelled);
    assert_eq!(picker.config_text(), before);
    assert_eq!(picker.selection(), ("openai/gpt-5.6", ThinkingLevel::Medium));

    picker.reopen();
    picker.choose("openai/gpt-5.6", ThinkingLevel::High).unwrap();
    assert_eq!(picker.key(PickerKey::SessionOnly), PickerOutcome::SessionInstalled);
    assert_eq!(picker.selection().1, ThinkingLevel::High);
    assert_eq!(picker.config_text(), before);
}

#[test]
fn enter_replaces_user_defaults_only_after_disk_commit_and_preserves_comments_order() {
    let selection = Arc::new(GenerationSelection::new(
        ModelRef("openai/gpt-5.6".into()),
        ThinkingLevel::Medium,
    ));
    let mut picker = ModelPicker::for_selection(
        ego_cli::models::build_catalogue_snapshot(),
        selection,
        "# preserve\nmodel = 'openai/gpt-5.6'\n\nthinking = 'medium'\nbare = true\n",
    );
    picker.choose("openai/gpt-5.6", ThinkingLevel::High).unwrap();
    assert_eq!(picker.key(PickerKey::Enter), PickerOutcome::Persisted);
    let persisted = picker.config_text();
    assert!(persisted.contains("# preserve"));
    assert!(persisted.find("model").unwrap() < persisted.find("thinking").unwrap());
    let parsed = ConfigLayer::parse(Source::User, &persisted).unwrap();
    assert_eq!(parsed.value.model, Some(ModelRef("openai/gpt-5.6".into())));
    assert_eq!(parsed.value.thinking, Some(ThinkingLevel::High));
    assert_eq!(picker.disk_written_before_memory_install(), true);
}

#[test]
fn every_generation_reresolves_selected_slug_against_current_catalogue_authority() {
    let selection = Arc::new(GenerationSelection::new(
        ModelRef("ollama/qwen3".into()),
        ThinkingLevel::Low,
    ));
    let mut picker = ModelPicker::for_selection(
        ego_cli::models::build_catalogue_snapshot(),
        selection,
        "model = 'ollama/qwen3'\nthinking = 'low'\n",
    );
    let first = picker.generation_boundary_resolution();
    let mut unavailable = ego_cli::models::build_catalogue_snapshot();
    unavailable
        .models
        .iter_mut()
        .find(|row| row.slug == "ollama/qwen3")
        .expect("catalogue fixture includes ollama/qwen3")
        .availability = ModelAvailability::Unavailable {
        reason: "provider unavailable".into(),
    };
    picker.replace_snapshot(unavailable);
    let second = picker.generation_boundary_resolution();
    assert_ne!(first.authority_revision(), second.authority_revision());
    assert!(second.is_unavailable());
}
