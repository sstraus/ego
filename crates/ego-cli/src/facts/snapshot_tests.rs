//! The builtin catalogue against the pinned models.dev snapshot.
//!
//! The oracle is `catalogue/policy.toml` read as data plus the snapshot: the
//! selection rule is restated here from the policy's own documented meaning,
//! independently of `catalogue.py`, so a generator that drops a model or a hand
//! edit of `models.generated.toml` fails here rather than in a release.

use std::collections::BTreeSet;

use ego_core::generator::capabilities::ReasoningEffortSupport;
use serde_json::Value;

use super::*;

const POLICY: &str = include_str!("../../catalogue/policy.toml");
const SNAPSHOT: &str = include_str!("../../catalogue/upstream/models.dev.json");
const GENERATED: &str = BUILTIN_GENERATED;

fn snapshot() -> Value {
    serde_json::from_str(SNAPSHOT).expect("the pinned snapshot is JSON")
}

fn policy() -> toml::Table {
    toml::from_str(POLICY).expect("the policy is TOML")
}

fn strings(value: Option<&toml::Value>) -> BTreeSet<String> {
    value
        .and_then(toml::Value::as_array)
        .map(|list| {
            list.iter()
                .map(|item| item.as_str().expect("a string").to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// What the policy says each provider's current generation is.
fn expected(snapshot: &Value, policy: &toml::Table) -> Vec<(String, BTreeSet<String>)> {
    policy["provider"]
        .as_table()
        .expect("provider tables")
        .iter()
        .map(|(name, provider)| {
            let models = snapshot["providers"][provider["models_dev"].as_str().unwrap()]
                .as_object()
                .expect("the snapshot holds the provider");
            let floor = provider["floor"].as_str().expect("a floor date");
            let mut ids: BTreeSet<String> = models
                .iter()
                .filter(|(_, model)| {
                    model["status"] != "deprecated"
                        && model["release_date"].as_str().unwrap() >= floor
                })
                .map(|(id, _)| id.clone())
                .collect();
            ids.extend(strings(provider.get("include")));
            let excluded = provider
                .get("exclude")
                .and_then(toml::Value::as_table)
                .map(|table| table.keys().cloned().collect::<BTreeSet<_>>())
                .unwrap_or_default();
            ids.retain(|id| !excluded.contains(id));
            (name.clone(), ids)
        })
        .collect()
}

fn row<'a>(
    rows: &'a [(String, HostedModel)],
    slug: &str,
) -> &'a ego_core::generator::capabilities::ModelCapabilities {
    &rows
        .iter()
        .find(|(key, _)| key == slug)
        .unwrap_or_else(|| panic!("the builtin catalogue lacks `{slug}`"))
        .1
        .capabilities
}

/// Catches: a provider row added without its models, or a model dropped by the
/// generator, so a key for that provider lists nothing or a stale subset.
#[test]
fn every_current_snapshot_model_is_in_the_builtin_catalogue_with_the_snapshot_limits() {
    let (snapshot, policy) = (snapshot(), policy());
    let rows = builtin_only();
    for (name, ids) in expected(&snapshot, &policy) {
        assert!(!ids.is_empty(), "`{name}` has no current model");
        let provider = &policy["provider"][&name];
        let models = &snapshot["providers"][provider["models_dev"].as_str().unwrap()];
        for id in ids {
            let caps = row(&rows, &format!("{name}/{id}"));
            let context = provider
                .get("context_override")
                .and_then(toml::Value::as_integer)
                .map(|v| v as u64)
                .unwrap_or_else(|| models[&id]["limit"]["context"].as_u64().unwrap());
            assert_eq!(
                caps.max_context_tokens,
                Some(context),
                "{name}/{id} context"
            );
            assert_eq!(
                caps.max_output_tokens,
                models[&id]["limit"]["output"].as_u64(),
                "{name}/{id} output"
            );
        }
    }
}

/// Catches: an older generation (or a hand-added row) surviving in the builtin
/// catalogue after maintainer's 2026-10-11 decision that only current models are listed.
#[test]
fn the_builtin_catalogue_lists_nothing_beyond_the_current_generations() {
    let (snapshot, policy) = (snapshot(), policy());
    let wanted: BTreeSet<String> = expected(&snapshot, &policy)
        .into_iter()
        .flat_map(|(name, ids)| ids.into_iter().map(move |id| format!("{name}/{id}")))
        .collect();
    let listed: BTreeSet<String> = builtin().model.into_keys().collect();
    assert_eq!(
        listed.difference(&wanted).collect::<Vec<_>>(),
        Vec::<&String>::new(),
        "builtin rows outside the current generation"
    );
    assert!(
        listed.iter().all(|slug| !slug.starts_with("openrouter/")),
        "OpenRouter stays dynamic: it has no builtin rows"
    );
}

/// Catches: a hand row in `models.toml` that competes with generated model facts.
#[test]
fn models_toml_states_no_model_row() {
    let hand: Facts = toml::from_str(BUILTIN).expect("models.toml is TOML");
    assert!(
        hand.model.is_empty(),
        "models.toml holds model rows: {:?}",
        hand.model.keys()
    );
}

/// Catches: an effort set edited by hand, or a toggle read as an effort list.
#[test]
fn reasoning_efforts_follow_the_snapshot_controls() {
    let (snapshot, policy) = (snapshot(), policy());
    let rows = builtin_only();
    for (name, ids) in expected(&snapshot, &policy) {
        let models =
            &snapshot["providers"][policy["provider"][&name]["models_dev"].as_str().unwrap()];
        for id in ids {
            let model = &models[&id];
            let mut want: BTreeSet<&str> = BTreeSet::new();
            if model["reasoning"] != true {
                want.insert("off");
            }
            for option in model["reasoning_options"].as_array().into_iter().flatten() {
                match option["type"].as_str().unwrap() {
                    "toggle" => {
                        want.insert("off");
                    }
                    "effort" => {
                        for value in option["values"].as_array().unwrap() {
                            want.insert(match value.as_str().unwrap() {
                                "none" => "off",
                                other => other,
                            });
                        }
                    }
                    other => panic!("{name}/{id}: unmodelled reasoning option {other}"),
                }
            }
            for dropped in strings(policy["provider"][&name].get("drop_efforts")) {
                want.remove(dropped.as_str());
            }
            let ReasoningEffortSupport::Only(have) =
                &row(&rows, &format!("{name}/{id}")).reasoning_efforts
            else {
                panic!("{name}/{id}: efforts are not an explicit set");
            };
            let have: BTreeSet<&str> = have.iter().map(|level| level.as_str()).collect();
            assert_eq!(have, want, "{name}/{id} efforts");
        }
    }
}

/// Catches: a coding-plan literal 0 (or any rate) imported with the capabilities.
#[test]
fn no_price_enters_the_catalogue_through_the_snapshot() {
    fn mentions_cost(value: &Value) -> bool {
        match value {
            Value::Object(map) => map.contains_key("cost") || map.values().any(mentions_cost),
            Value::Array(list) => list.iter().any(mentions_cost),
            _ => false,
        }
    }
    assert!(
        !mentions_cost(&snapshot()),
        "the snapshot carries a cost field"
    );
}

/// Catches: a hand-edited or stale generated file: every row names the snapshot
/// revision it was read from, and that revision is the pinned one.
#[test]
fn every_generated_row_names_the_pinned_snapshot_revision() {
    let snapshot = snapshot();
    let revision = snapshot["revision"].as_str().expect("a revision");
    let read = snapshot["read"].as_str().expect("a read date");
    assert!(GENERATED.contains(&format!("# revision = {revision}")));
    assert!(GENERATED.contains(&format!("# read = {read}")));
    let lines: Vec<&str> = GENERATED.lines().collect();
    let mut tables = 0;
    for (index, line) in lines.iter().enumerate() {
        if line.starts_with("[model.\"") && line.ends_with(".capabilities]") {
            tables += 1;
            let provenance = lines[index - 1];
            assert!(
                provenance.starts_with(&format!("# models.dev {} read {read}: ", &revision[..8])),
                "row `{line}` lacks its provenance line: {provenance:?}"
            );
        }
    }
    assert!(tables > 0, "no generated rows");
}

/// Catches: a row ego listed before the decision disappearing with no answer
/// for an operator whose config still names it.
#[test]
fn rows_dropped_by_the_current_generation_rule_are_tombstoned() {
    for (old, replacement) in [
        ("anthropic/claude-sonnet-5", "anthropic/claude-sonnet-5-5"),
        ("anthropic/claude-haiku-4-5", "anthropic/claude-haiku-5-5"),
    ] {
        let refusal = retirement(old)
            .unwrap_or_else(|| panic!("`{old}` left the catalogue without a tombstone"))
            .to_string();
        assert!(refusal.contains(replacement), "{refusal}");
        assert!(!builtin().model.contains_key(old));
    }
}
