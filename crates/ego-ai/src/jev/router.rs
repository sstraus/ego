//! Calibrated Jev routing through the existing single-exchange adapter.
use super::{ConfiguredDecisions, DecisionError, calibration::Metrics};
use crate::http::Http;
use ego_core::{
    config::{JevConfig, JevUse, Probability},
    turn::{RouterOutcome, RouterStatus, TurnRouter},
};
use futures::future::BoxFuture;
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

/// One run's routing adapter; it retains references, never credential values.
pub struct JevRouter {
    client: ConfiguredDecisions,
    root: PathBuf,
    http: Arc<dyn Http>,
    prompt: Option<String>,
}
impl JevRouter {
    /// Use the latest shipped independent report, without recalibration or caching.
    pub fn new(config: JevConfig, home: PathBuf, root: PathBuf, http: Arc<dyn Http>) -> Self {
        let report: Result<Value, _> =
            serde_json::from_str(include_str!("../../../../docs/metrics/jev/held-out.json"));
        let calibration: Result<Value, _> = serde_json::from_str(include_str!(
            "../../../../docs/metrics/jev/calibration.json"
        ));
        let prompt = report
            .ok()
            .zip(calibration.ok())
            .and_then(|(report, calibration)| {
                let router = &report["uses"]["router"];
                let metrics: Metrics = serde_json::from_value(router["metrics"].clone()).ok()?;
                if !metrics.passed()
                    || router["on"] != true
                    || router["regression"] != false
                    || router["selection"] != calibration["uses"]["router"]["selection"]
                {
                    return None;
                }
                router["selection"]["prompt"].as_str().map(str::to_owned)
            });
        Self {
            client: ConfiguredDecisions::new(config, home, Duration::from_millis(800)),
            root,
            http,
            prompt,
        }
    }
}
impl TurnRouter for JevRouter {
    fn route<'a>(&'a self, text: &'a str) -> BoxFuture<'a, RouterOutcome> {
        Box::pin(async move {
            let attempt = async move {
                let Some(prompt) = &self.prompt else {
                    return RouterOutcome::fallback(RouterStatus::Uncalibrated);
                };
                let settings = match self.client.settings(&self.root) {
                    Ok(settings) => settings,
                    Err(_) => return RouterOutcome::fallback(RouterStatus::Error),
                };
                let policy = settings.policy(JevUse::Router);
                if !policy.on {
                    return RouterOutcome::fallback(RouterStatus::Off);
                }
                // The calibration corpus and spend reserve used this input bound.
                if text.len() > 4096 {
                    return RouterOutcome::fallback(RouterStatus::Error);
                }
                let reply = self
                    .client
                    .ask_measured(
                        self.http.as_ref(),
                        &self.root,
                        JevUse::Router,
                        true,
                        &json!({"text":text}),
                        &BTreeMap::from([("decision".into(), prompt.clone())]),
                    )
                    .await;
                match reply {
                    Ok(Some(reply)) => {
                        let Some(p) = reply.answers.get("decision").copied() else {
                            return RouterOutcome::fallback(RouterStatus::Error);
                        };
                        let probability = serde_json::from_value::<Probability>(json!(p)).ok();
                        RouterOutcome {
                            status: if p <= policy.low.get() {
                                RouterStatus::Cheap
                            } else if p >= policy.high.get() {
                                RouterStatus::Strong
                            } else {
                                RouterStatus::Grey
                            },
                            probability,
                            cost_usd: reply.cost_usd,
                        }
                    }
                    Ok(None) => RouterOutcome::fallback(RouterStatus::Off),
                    Err(DecisionError::Timeout) => RouterOutcome::fallback(RouterStatus::Timeout),
                    Err(_) => RouterOutcome::fallback(RouterStatus::Error),
                }
            };
            tokio::time::timeout(Duration::from_millis(800), attempt)
                .await
                .unwrap_or_else(|_| RouterOutcome::fallback(RouterStatus::Timeout))
        })
    }
}
