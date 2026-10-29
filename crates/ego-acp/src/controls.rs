//! ACP v1 session config options: the host's picker for the generation
//! controls and the run's confinement.
//!
//! `docs/architecture.md` §2 selects `session/set_config_option` and its
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
use ego_core::permit::Confinement;

/// The config ids ego advertises. They are the wire spelling of the controls
/// and nothing else answers to them.
pub const MODEL: &str = "model";
pub const EFFORT: &str = "effort";
pub const MODE: &str = "mode";
pub const SANDBOX: &str = "sandbox";
const UNSELECTED: &str = "__ego_unselected__";
const CHOOSE: &str = "Choose a value";

/// Every control, in the order a host should render them.
pub fn options(state: &SessionControlState) -> Vec<v1::SessionConfigOption> {
    vec![
        select(
            MODEL,
            "Model",
            "The model the next generation uses.",
            v1::SessionConfigOptionCategory::Model,
            &state.model,
            true,
            |model: &ModelRef| (model.0.clone(), model.0.clone()),
        ),
        select(
            EFFORT,
            "Reasoning effort",
            "How much the model is asked to think, within what it supports.",
            v1::SessionConfigOptionCategory::ThoughtLevel,
            &state.effort,
            true,
            |effort: &ThinkingLevel| (effort.as_str().to_owned(), effort.as_str().to_owned()),
        ),
        select(
            MODE,
            "Mode",
            if state.mode.available.contains(&Mode::Auto) {
                "The permission mode the next generation is judged under."
            } else {
                "The permission mode the next generation is judged under. \
                 auto is unavailable in this session. It needs a configured \
                 [permissions] judge: ego config set 'permissions.judge=\"openai-codex/gpt-6.1-sol\"'. \
                 If this session uses a profile, add --profile <name> to that command. \
                 Start a new session after configuration. The session max_mode can also restrict auto."
            },
            v1::SessionConfigOptionCategory::Mode,
            &state.mode,
            false,
            |mode: &Mode| (mode.as_str().to_owned(), mode.as_str().to_owned()),
        ),
        // `Other` rather than `Mode`. The pinned schema names `Mode`,
        // `Model`, `ModelConfig` and `ThoughtLevel`, and offers `Other` for a
        // selector it does not name. Confinement is not a permission mode,
        // and labelling it one to fit the enum would tell every host
        // something false about what it is.
        select(
            SANDBOX,
            "Sandbox",
            "How much of the filesystem a command this session runs can reach. \
             The list is what the operator who started this process allowed and \
             this machine can enforce; it can be narrowed here and not widened.",
            v1::SessionConfigOptionCategory::Other(SANDBOX.to_owned()),
            &state.sandbox,
            false,
            |confinement: &Confinement| {
                (
                    confinement.as_str().to_owned(),
                    confinement.as_str().to_owned(),
                )
            },
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
) -> Result<Option<SessionControlChange>, Error> {
    let config = request.config_id.0.as_ref();
    let v1::SessionConfigOptionValue::ValueId { value } = &request.value else {
        return Err(refuse(format!(
            "the `{config}` option is a select and takes a value id, not a boolean"
        )));
    };
    let value = value.0.as_ref();
    match config {
        MODEL if value == UNSELECTED && state.model.current.is_none() => Ok(None),
        MODEL => chosen(&state.model, value, |model: &ModelRef| model.0.as_str())
            .map(|model| Some(SessionControlChange::Model(model.clone()))),
        EFFORT if value == UNSELECTED && state.effort.current.is_none() => Ok(None),
        EFFORT => chosen(&state.effort, value, |effort: &ThinkingLevel| {
            effort.as_str()
        })
        .map(|effort| Some(SessionControlChange::Effort(*effort))),
        MODE => chosen(&state.mode, value, |mode: &Mode| mode.as_str())
            .map(|mode| Some(SessionControlChange::Mode(*mode))),
        // Matched against the offer like everything else, which is the whole
        // enforcement of the ceiling: a confinement outside it was never
        // advertised, so it is refused here by the rule that already exists
        // rather than by a second one that could disagree with the first.
        SANDBOX => chosen(&state.sandbox, value, |confinement: &Confinement| {
            confinement.as_str()
        })
        .map(|confinement| Some(SessionControlChange::Sandbox(*confinement))),
        _ => Err(refuse(format!(
            "this agent advertises no `{config}` session config option"
        ))),
    }
    .map_err(|_| {
        if config == MODE && value == "auto" {
            refuse(
                "`auto` needs a configured `[permissions] judge` and must be allowed by the session max_mode"
                    .to_owned(),
            )
        } else {
            refuse(format!("`{value}` is not an available `{config}`"))
        }
    })
}

/// A real confinement spelling from a sandbox request, offered or not.
///
/// The router uses this only after offer matching refused the value, so the
/// measured admission can distinguish a known but unsupported confinement
/// from an unknown spelling without weakening the offer as the admission
/// rule.
pub fn requested_confinement(request: &v1::SetSessionConfigOptionRequest) -> Option<Confinement> {
    if request.config_id.0.as_ref() != SANDBOX {
        return None;
    }
    let v1::SessionConfigOptionValue::ValueId { value } = &request.value else {
        return None;
    };
    value.0.as_ref().parse().ok()
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
    admits_unselected: bool,
    spell: impl Fn(&T) -> (String, String),
) -> v1::SessionConfigOption {
    let mut options: Vec<_> = control
        .available
        .iter()
        .map(|candidate| {
            let (value, label) = spell(candidate);
            v1::SessionConfigSelectOption::new(v1::SessionConfigValueId::new(value), label)
        })
        .collect();
    let current = match control.current.as_ref() {
        Some(current) => spell(current).0,
        None if admits_unselected => {
            options.insert(
                0,
                v1::SessionConfigSelectOption::new(
                    v1::SessionConfigValueId::new(UNSELECTED),
                    CHOOSE,
                ),
            );
            UNSELECTED.to_owned()
        }
        None => panic!("the `{id}` control must always have a current value"),
    };
    v1::SessionConfigOption::new(
        v1::SessionConfigId::new(id),
        name,
        v1::SessionConfigKind::Select(v1::SessionConfigSelect::new(
            v1::SessionConfigValueId::new(current),
            v1::SessionConfigSelectOptions::Ungrouped(options),
        )),
    )
    .description(Some(description.to_owned()))
    .category(Some(category))
}

fn refuse(message: String) -> Error {
    Error::new(
        i32::from(agent_client_protocol::ErrorCode::InvalidParams),
        message,
    )
}
