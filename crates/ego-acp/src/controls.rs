//! ACP v1 session config options: the host's picker for the three generation
//! controls.
//!
//! `docs/06-acp.md` §2 selects `session/set_config_option` and its
//! model-category options as the projection for model and config control.
//! The dedicated `providers/list|set|disable` methods are not served: they
//! are gated behind the schema's `unstable_llm_providers` draft feature,
//! which the pinned SDK does not expose and therefore cannot route, and
//! `providers/set` would let a connected host rewrite the base URL every
//! generation egresses to. §3's rule — a pinned dependency or compiled
//! feature is not an advertised capability — settles it.
//!
//! Everything here is a projection. The available list published by
//! [`SessionControlState`] is the entire offer, and an incoming value is
//! resolved by matching it against that list rather than by parsing the
//! string a second way. A control ego did not offer, and a value ego did not
//! offer, are the same refusal.

use agent_client_protocol::Error;
use agent_client_protocol_schema::v1;
use ego_core::config::{Mode, ModelRef};
use ego_core::control::{ControlSelect, SessionControlChange, SessionControlState};
use ego_core::generator::types::ThinkingLevel;

/// The config ids ego advertises. They are the wire spelling of the three
/// controls and nothing else answers to them.
pub const MODEL: &str = "model";
pub const EFFORT: &str = "effort";
pub const MODE: &str = "mode";

/// Every control, in the order a host should render them.
pub fn options(state: &SessionControlState) -> Vec<v1::SessionConfigOption> {
    vec![
        select(
            MODEL,
            "Model",
            "The model the next generation uses.",
            v1::SessionConfigOptionCategory::Model,
            &state.model,
            |model: &ModelRef| (model.0.clone(), model.0.clone()),
        ),
        select(
            EFFORT,
            "Reasoning effort",
            "How much the model is asked to think, within what it supports.",
            v1::SessionConfigOptionCategory::ThoughtLevel,
            &state.effort,
            |effort: &ThinkingLevel| (effort.as_str().to_owned(), effort.as_str().to_owned()),
        ),
        select(
            MODE,
            "Mode",
            "The permission mode the next generation is judged under.",
            v1::SessionConfigOptionCategory::Mode,
            &state.mode,
            |mode: &Mode| (mode.as_str().to_owned(), mode.as_str().to_owned()),
        ),
    ]
}

/// Resolve one `session/set_config_option` against what ego offered.
///
/// Takes the state rather than parsing, so a value is admitted only because
/// this session currently offers it. That is what keeps a mode this seat may
/// not set, or an effort this model does not support, out of the run without
/// a second rule that could drift from the first.
pub fn change(
    state: &SessionControlState,
    request: &v1::SetSessionConfigOptionRequest,
) -> Result<SessionControlChange, Error> {
    let config = request.config_id.0.as_ref();
    let v1::SessionConfigOptionValue::ValueId { value } = &request.value else {
        return Err(refuse(format!(
            "the `{config}` option is a select and takes a value id, not a boolean"
        )));
    };
    let value = value.0.as_ref();
    match config {
        MODEL => chosen(&state.model, value, |model: &ModelRef| model.0.as_str())
            .map(|model| SessionControlChange::Model(model.clone())),
        EFFORT => chosen(&state.effort, value, |effort: &ThinkingLevel| {
            effort.as_str()
        })
        .map(|effort| SessionControlChange::Effort(*effort)),
        MODE => chosen(&state.mode, value, |mode: &Mode| mode.as_str())
            .map(|mode| SessionControlChange::Mode(*mode)),
        _ => Err(refuse(format!(
            "this agent advertises no `{config}` session config option"
        ))),
    }
    .map_err(|error| error.data(format!("`{value}` is not an available `{config}`")))
}

/// The one value in the offer that spells itself the way the host answered.
fn chosen<'a, T>(
    control: &'a ControlSelect<T>,
    value: &str,
    spell: impl Fn(&T) -> &str,
) -> Result<&'a T, Error> {
    control
        .available
        .iter()
        .find(|candidate| spell(candidate) == value)
        .ok_or_else(Error::invalid_params)
}

fn select<T>(
    id: &str,
    name: &str,
    description: &str,
    category: v1::SessionConfigOptionCategory,
    control: &ControlSelect<T>,
    spell: impl Fn(&T) -> (String, String),
) -> v1::SessionConfigOption {
    let options = control
        .available
        .iter()
        .map(|candidate| {
            let (value, label) = spell(candidate);
            v1::SessionConfigSelectOption::new(v1::SessionConfigValueId::new(value), label)
        })
        .collect();
    v1::SessionConfigOption::new(
        v1::SessionConfigId::new(id),
        name,
        v1::SessionConfigKind::Select(v1::SessionConfigSelect::new(
            v1::SessionConfigValueId::new(spell(&control.current).0),
            v1::SessionConfigSelectOptions::Ungrouped(options),
        )),
    )
    .description(Some(description.to_owned()))
    .category(Some(category))
}

fn refuse(message: String) -> Error {
    Error::invalid_params().data(message)
}
