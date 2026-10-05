//! Calibrated destructive scrutiny of canonical exec arguments.
use super::{
    ConfiguredDecisions,
    calibration::{Metrics, Selection},
};
use crate::http::Http;
use ego_core::{
    config::{JevUse, ModelRef},
    permit::{
        Op, PreparedCall,
        analysis::{DestructiveJudge, DestructiveOpinion},
    },
};
use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::json;
use std::{collections::BTreeMap, path::Path, sync::Arc};

/// The recorded held-out gate and prompt ship with the executable.
const HELD_OUT: &str = include_str!("../../../../docs/metrics/jev/held-out.json");

#[derive(Deserialize)]
struct Gate {
    selection: Selection,
    metrics: Metrics,
    on: bool,
    regression: bool,
}

/// One configured Decisions exchange, without caching or retries.
pub struct JevDestructiveJudge {
    client: Result<ConfiguredDecisions, super::DecisionError>,
    http: Arc<dyn Http>,
}
impl JevDestructiveJudge {
    pub fn new(
        client: Result<ConfiguredDecisions, super::DecisionError>,
        http: Arc<dyn Http>,
    ) -> Self {
        Self { client, http }
    }
}
impl DestructiveJudge for JevDestructiveJudge {
    fn judge<'a>(
        &'a self,
        root: &'a Path,
        call: &'a PreparedCall,
    ) -> BoxFuture<'a, DestructiveOpinion> {
        Box::pin(async move {
            let Ok(client) = &self.client else {
                return DestructiveOpinion::Card;
            };
            let settings = match client.settings(root) {
                Ok(settings) => settings,
                Err(_) => return DestructiveOpinion::Card,
            };
            let policy = settings.policy(JevUse::Destructive);
            if !policy.on {
                return DestructiveOpinion::CurrentJudge;
            }
            let gate = serde_json::from_str::<serde_json::Value>(HELD_OUT)
                .ok()
                .and_then(|report| {
                    serde_json::from_value::<Gate>(report["uses"]["destructive"].clone()).ok()
                });
            let Some(gate) = gate.filter(|g| g.on && !g.regression && g.metrics.passed()) else {
                return DestructiveOpinion::CurrentJudge;
            };
            // Preserve argument boundaries: shell syntax inside a literal argument is data.
            let [Op::Spawn { spec }] = call.ops() else {
                return DestructiveOpinion::Card;
            };
            let Some(argv) = spec
                .argv()
                .iter()
                .map(|arg| arg.to_str())
                .collect::<Option<Vec<_>>>()
            else {
                return DestructiveOpinion::Card;
            };
            let text = argv
                .iter()
                .map(|arg| format!("'{}'", arg.replace('\'', "'\\''")))
                .collect::<Vec<_>>()
                .join(" ");
            // Calibration bounded every input at 4096 bytes; larger/native commands retain scrutiny.
            if text.len() > 4096 {
                return DestructiveOpinion::Card;
            }
            let answer = client
                .ask(
                    self.http.as_ref(),
                    root,
                    JevUse::Destructive,
                    true,
                    &json!({"text": text}),
                    &BTreeMap::from([("decision".into(), gate.selection.prompt)]),
                )
                .await;
            match answer {
                Ok(Some(answers))
                    if answers
                        .get("decision")
                        .is_some_and(|p| *p <= policy.low.get()) =>
                {
                    DestructiveOpinion::Safe {
                        model: ModelRef(settings.model),
                    }
                }
                Ok(None) => DestructiveOpinion::CurrentJudge,
                _ => DestructiveOpinion::Card,
            }
        })
    }
}
