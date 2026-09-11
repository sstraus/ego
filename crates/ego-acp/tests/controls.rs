//! contract for ACP session config options (regression case, criterion 1).
//!
//! `docs/06-acp.md` §2 says model and config control is projected as
//! `session/set_config_option` over model-category options, and that a valid
//! change is staged and applied only at a safe generation boundary. These
//! pin the wire half of that: what a host is offered, and what ego does with
//! every answer a host can send back.
//!
//! Nothing here may accept a value ego did not offer. The available list is
//! the whole authority: the adapter matches an incoming value against it
//! rather than parsing the string a second way, which is the same rule the
//! permission seat applies to option ids.

use agent_client_protocol::Error;
use agent_client_protocol_schema::v1;
use ego_acp::controls;
use ego_core::config::{Mode, ModelRef};
use ego_core::control::{ControlSelect, SessionControlChange, SessionControlState};
use ego_core::generator::types::ThinkingLevel;
use ego_core::permit::Confinement;

const UNSELECTED: &str = "__ego_unselected__";

fn state() -> SessionControlState {
    SessionControlState {
        model: ControlSelect {
            current: Some(ModelRef("ollama/gemma4:e4b-mlx".to_owned())),
            chosen: None,
            available: vec![
                ModelRef("ollama/gemma4:e4b-mlx".to_owned()),
                ModelRef("anthropic/claude-sonnet-5".to_owned()),
            ],
        },
        effort: ControlSelect {
            current: Some(ThinkingLevel::Low),
            chosen: Some(ThinkingLevel::Low),
            available: vec![ThinkingLevel::Off, ThinkingLevel::Low, ThinkingLevel::High],
        },
        mode: ControlSelect {
            current: Some(Mode::Default),
            chosen: None,
            available: vec![Mode::Plan, Mode::Default, Mode::Edits],
        },
        // A `workspace` ceiling: `off` is absent because it is weaker, and a
        // value the state never offered is what `change` must refuse.
        sandbox: ControlSelect {
            current: Some(Confinement::Workspace),
            chosen: None,
            available: vec![Confinement::Ro, Confinement::Workspace],
        },
    }
}

fn unselected_state() -> SessionControlState {
    let mut state = state();
    state.model.current = None;
    state.model.chosen = None;
    state.effort.current = None;
    state.effort.chosen = None;
    state
}

fn request(config: &str, value: &str) -> v1::SetSessionConfigOptionRequest {
    v1::SetSessionConfigOptionRequest::new(
        v1::SessionId::new("session-under-test"),
        v1::SessionConfigId::new(config),
        v1::SessionConfigOptionValue::ValueId {
            value: v1::SessionConfigValueId::new(value),
        },
    )
}

fn option<'a>(options: &'a [v1::SessionConfigOption], id: &str) -> &'a v1::SessionConfigOption {
    options
        .iter()
        .find(|option| option.id.0.as_ref() == id)
        .unwrap_or_else(|| panic!("no config option `{id}`"))
}

fn select(option: &v1::SessionConfigOption) -> &v1::SessionConfigSelect {
    match &option.kind {
        v1::SessionConfigKind::Select(select) => select,
        other => panic!("a generation control is never {other:?}"),
    }
}

fn values(option: &v1::SessionConfigOption) -> Vec<String> {
    match &select(option).options {
        v1::SessionConfigSelectOptions::Ungrouped(options) => options
            .iter()
            .map(|option| option.value.0.to_string())
            .collect(),
        other => panic!("ego offers one flat list per control, not {other:?}"),
    }
}

/// Every control ego offers, each in the ACP category a host uses to decide
/// where to render it.
///
/// Sandbox is `Other` and the other three are named categories, because the
/// pinned schema has no category for a confinement and the three it does name
/// all mean something else. Calling it `Mode` to avoid `Other` would tell
/// every connected host that confinement is a permission mode, which it is
/// not (`2026-09-07-a-host-picks-confinement-under-a-ceiling-it-cannot-raise.md`).
#[test]
fn every_control_is_offered_in_its_own_acp_category() {
    let options = controls::options(&state());
    let ids: Vec<_> = options.iter().map(|o| o.id.0.to_string()).collect();
    assert_eq!(
        ids,
        vec![
            controls::MODEL,
            controls::EFFORT,
            controls::MODE,
            controls::SANDBOX
        ]
    );
    assert_eq!(
        option(&options, controls::SANDBOX).category,
        Some(v1::SessionConfigOptionCategory::Other(
            controls::SANDBOX.to_owned()
        ))
    );
    assert_eq!(
        option(&options, controls::MODEL).category,
        Some(v1::SessionConfigOptionCategory::Model)
    );
    assert_eq!(
        option(&options, controls::EFFORT).category,
        Some(v1::SessionConfigOptionCategory::ThoughtLevel)
    );
    assert_eq!(
        option(&options, controls::MODE).category,
        Some(v1::SessionConfigOptionCategory::Mode)
    );
}

/// A host renders the value the next generation will use. Reporting anything
/// else would make the picker disagree with the run.
#[test]
fn each_control_reports_the_value_the_next_generation_would_use() {
    let options = controls::options(&state());
    assert_eq!(
        select(option(&options, controls::MODEL))
            .current_value
            .0
            .as_ref(),
        "ollama/gemma4:e4b-mlx"
    );
    assert_eq!(
        select(option(&options, controls::EFFORT))
            .current_value
            .0
            .as_ref(),
        "low"
    );
    assert_eq!(
        select(option(&options, controls::MODE))
            .current_value
            .0
            .as_ref(),
        "default"
    );
}

/// The available list is the offer. Effort is per-model and mode excludes
/// what this seat may not set, so the wire list must be exactly the core
/// list rather than every variant the enum happens to have.
#[test]
fn the_offered_values_are_exactly_the_ones_core_says_are_available() {
    let options = controls::options(&state());
    assert_eq!(
        values(option(&options, controls::MODEL)),
        vec!["ollama/gemma4:e4b-mlx", "anthropic/claude-sonnet-5"]
    );
    assert_eq!(
        values(option(&options, controls::EFFORT)),
        vec!["off", "low", "high"]
    );
    assert_eq!(
        values(option(&options, controls::MODE)),
        vec!["plan", "default", "edits"]
    );
}

#[test]
fn a_value_the_host_was_offered_becomes_the_change_it_names() {
    let state = state();
    assert_eq!(
        controls::change(
            &state,
            &request(controls::MODEL, "anthropic/claude-sonnet-5")
        ),
        Ok(Some(SessionControlChange::Model(ModelRef(
            "anthropic/claude-sonnet-5".to_owned()
        ))))
    );
    assert_eq!(
        controls::change(&state, &request(controls::EFFORT, "high")),
        Ok(Some(SessionControlChange::Effort(ThinkingLevel::High)))
    );
    assert_eq!(
        controls::change(&state, &request(controls::MODE, "plan")),
        Ok(Some(SessionControlChange::Mode(Mode::Plan)))
    );
}

/// A value ego never offered cannot be the value ego honours. `yolo` is a
/// real `Mode` and `max` is a real `ThinkingLevel`; neither is in this
/// session's available list, so both are refused by the same rule rather
/// than by a special case.
#[test]
fn a_value_ego_never_offered_is_refused_even_when_the_type_can_hold_it() {
    let state = state();
    for (config, value) in [
        (controls::MODE, "yolo"),
        (controls::EFFORT, "max"),
        (controls::MODEL, "openai/gpt-5"),
        (controls::MODE, ""),
    ] {
        let refusal = controls::change(&state, &request(config, value))
            .expect_err("an unoffered value must not resolve to a change");
        assert_eq!(
            refusal.code,
            Error::invalid_params().code,
            "{config}={value}"
        );
    }
}

#[test]
fn a_config_id_ego_never_advertised_is_refused_rather_than_guessed() {
    let refusal = controls::change(&state(), &request("temperature", "0.7"))
        .expect_err("an unknown config id must not resolve to a change");
    assert_eq!(refusal.code, Error::invalid_params().code);
}

/// ACP lets a client send a boolean for a config option, and a client must
/// advertise that capability first. Every ego control is a select, so a
/// boolean answer is a client asserting a shape ego never published.
#[test]
fn a_boolean_answer_to_a_select_control_is_refused() {
    let boolean = v1::SetSessionConfigOptionRequest::new(
        v1::SessionId::new("session-under-test"),
        v1::SessionConfigId::new(controls::MODE),
        v1::SessionConfigOptionValue::Boolean { value: true },
    );
    let refusal = controls::change(&state(), &boolean)
        .expect_err("a boolean must not resolve to a select change");
    assert_eq!(refusal.code, Error::invalid_params().code);
}

#[test]
fn absent_model_and_effort_project_as_opaque_invites_to_choose() {
    let options = controls::options(&unselected_state());
    for id in [controls::MODEL, controls::EFFORT] {
        let projected = select(option(&options, id));
        assert_eq!(projected.current_value.0.as_ref(), UNSELECTED, "{id}");
        assert!(values(option(&options, id)).contains(&UNSELECTED.to_owned()));
    }
    for id in [controls::MODE, controls::SANDBOX] {
        assert!(
            !values(option(&options, id)).contains(&UNSELECTED.to_owned()),
            "{id}"
        );
    }
}

#[test]
fn selecting_the_opaque_current_value_is_a_noop_until_a_real_value_exists() {
    assert_eq!(
        controls::change(&unselected_state(), &request(controls::MODEL, UNSELECTED),),
        Ok(None)
    );
    assert_eq!(
        controls::change(&unselected_state(), &request(controls::EFFORT, UNSELECTED),),
        Ok(None)
    );

    for id in [controls::MODEL, controls::EFFORT] {
        let refusal = controls::change(&state(), &request(id, UNSELECTED))
            .expect_err("a stale opaque value must refuse after a real choice");
        assert_eq!(refusal.code, Error::invalid_params().code, "{id}");
    }
}
