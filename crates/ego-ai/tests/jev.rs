//! Recorded OpenRouter Decisions wire, captured 2026-10-05.
use ego_ai::{
    http::{Http, HttpCall, PreparedHttpCall},
    jev::{DecisionError, DecisionsClient},
};
use ego_core::{
    credentials::SecretMaterial,
    permit::{HttpRequest, OneHopResponse, TransportError},
};
use futures::{future::BoxFuture, stream};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

struct Recorder {
    calls: Arc<Mutex<Vec<HttpRequest>>>,
    response: Value,
    stall: bool,
}
struct Call {
    calls: Arc<Mutex<Vec<HttpRequest>>>,
    response: Value,
    stall: bool,
}
impl Http for Recorder {
    fn begin_call(&self) -> Box<dyn HttpCall> {
        Box::new(Call {
            calls: self.calls.clone(),
            response: self.response.clone(),
            stall: self.stall,
        })
    }
}
impl HttpCall for Call {
    fn execute(
        &mut self,
        prepared: PreparedHttpCall,
    ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
        let (_, _, _, request) = prepared.into_parts();
        self.calls.lock().unwrap().push(request);
        let bytes = serde_json::to_vec(&self.response).unwrap();
        let stall = self.stall;
        Box::pin(async move {
            if stall {
                futures::future::pending::<()>().await;
            }
            Ok(OneHopResponse::streaming_response(
                200,
                vec![],
                Box::pin(stream::iter([Ok(bytes)])),
            ))
        })
    }
}
fn enabled(
    mut config: ego_core::config::JevConfig,
    root: &std::path::Path,
) -> ego_core::config::JevConfig {
    let on = ego_core::config::JevUseOverride {
        on: Some(true),
        ..Default::default()
    };
    config.roots.push(ego_core::config::JevRoot {
        path: root.to_string_lossy().into(),
        endpoint: None,
        wake: Some(on.clone()),
        destructive: Some(on.clone()),
        router: Some(on),
    });
    config
}
fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/jev/decisions.json")).unwrap()
}
fn client() -> DecisionsClient {
    DecisionsClient::new(
        "https://openrouter.ai/api/alpha/decisions".parse().unwrap(),
        "~typesafe/jev-latest".into(),
        Duration::from_millis(25),
    )
    .unwrap()
}
fn questions() -> BTreeMap<String, String> {
    fixture()["request"]["questions"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(id, q)| (id.clone(), q["instructions"].as_str().unwrap().into()))
        .collect()
}
// Catches: configurable privacy, wrong question mapping, secret in diagnostics.
#[tokio::test]
async fn recorded_answers_keep_ids_and_always_send_zdr() {
    let calls = Arc::new(Mutex::new(vec![]));
    let http = Recorder {
        calls: calls.clone(),
        response: fixture()["response"].clone(),
        stall: false,
    };
    let answers = client()
        .ask(
            &http,
            SecretMaterial::from("synthetic-key"),
            &fixture()["request"]["state"],
            &questions(),
        )
        .await
        .unwrap();
    assert_eq!(
        answers,
        BTreeMap::from([("destructive".into(), 0.02), ("wake".into(), 0.08)])
    );
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    let sent: Value = serde_json::from_slice(calls[0].body()).unwrap();
    assert_eq!(
        sent["provider"],
        json!({"zdr":true,"data_collection":"deny"})
    );
    assert_eq!(sent["questions"], fixture()["request"]["questions"]);
    assert!(!format!("{:?}", calls[0]).contains("synthetic-key"));
}
// Catches: fail-open parsing and provider fallback on malformed or routing errors.
#[tokio::test]
async fn routing_errors_and_invalid_answers_never_supply_probabilities() {
    for response in [
        json!({"error":{"message":"synthetic-key"}}),
        json!({}),
        json!({"answers":{"destructive":{"type":"noul","noul":1.1},"wake":{"type":"noul","noul":0.08}}}),
        json!({"answers":{"destructive":{"type":"choice","noul":0.02},"wake":{"type":"noul","noul":0.08}}}),
    ] {
        let http = Recorder {
            calls: Arc::new(Mutex::new(vec![])),
            response,
            stall: false,
        };
        let error = client()
            .ask(
                &http,
                SecretMaterial::from("synthetic-key"),
                &json!("test"),
                &questions(),
            )
            .await
            .unwrap_err();
        assert_eq!(error, DecisionError::InvalidResponse);
        assert_eq!(http.calls.lock().unwrap().len(), 1);
        assert!(!error.to_string().contains("synthetic-key"));
    }
}
// Catches: an unbounded exchange when the transport never responds.
#[tokio::test]
async fn timeout_cancels_one_exchange_without_retry() {
    let http = Recorder {
        calls: Arc::new(Mutex::new(vec![])),
        response: json!({}),
        stall: true,
    };
    let start = std::time::Instant::now();
    assert_eq!(
        client()
            .ask(
                &http,
                SecretMaterial::from("key"),
                &json!("test"),
                &questions()
            )
            .await
            .unwrap_err(),
        DecisionError::Timeout
    );
    assert!(start.elapsed() < Duration::from_secs(1));
    assert_eq!(http.calls.lock().unwrap().len(), 1);
}

// Catches: cached dotenv credentials, symlink bypass and an off switch still invoking Http.
#[tokio::test]
async fn configured_calls_read_rotated_keys_and_honour_canonical_switches() {
    use ego_ai::jev::ConfiguredDecisions;
    use ego_core::config::{ConfigLayer, JevConfig, JevUse, Source};
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let env_file = home.join("workspace/example-project/.env");
    std::fs::create_dir_all(env_file.parent().unwrap()).unwrap();
    let root = home.join("workspace/repo");
    std::fs::create_dir_all(&root).unwrap();
    let configured = ConfiguredDecisions::new(
        enabled(JevConfig::default(), &root),
        home.into(),
        Duration::from_secs(1),
    );
    let http = Recorder {
        calls: Arc::new(Mutex::new(vec![])),
        response: fixture()["response"].clone(),
        stall: false,
    };
    for key in ["first-synthetic-key", "second-synthetic-key"] {
        std::fs::write(&env_file, format!("OPENROUTER_API_KEY='{key}'\n")).unwrap();
        assert!(
            configured
                .ask(
                    &http,
                    &root,
                    JevUse::Wake,
                    true,
                    &json!("test"),
                    &questions()
                )
                .await
                .unwrap()
                .is_some()
        );
        let calls = http.calls.lock().unwrap();
        assert_eq!(
            calls
                .last()
                .unwrap()
                .headers()
                .iter()
                .find(|h| h.name() == "authorization")
                .unwrap()
                .value(),
            format!("Bearer {key}").as_bytes()
        );
    }
    std::fs::remove_file(&env_file).unwrap();
    assert!(
        configured
            .ask(
                &http,
                &root,
                JevUse::Wake,
                true,
                &json!("test"),
                &questions()
            )
            .await
            .is_err()
    );
    assert_eq!(http.calls.lock().unwrap().len(), 2);
    let text = format!(
        "[[jev.roots]]\npath='{}'\nwake={{on=false}}",
        root.display()
    );
    let config = ConfigLayer::parse(Source::User, &text)
        .unwrap()
        .value
        .jev
        .unwrap();
    let off = ConfiguredDecisions::new(config, home.into(), Duration::from_secs(1));
    assert!(
        off.ask(
            &http,
            &root,
            JevUse::Wake,
            true,
            &json!("test"),
            &questions()
        )
        .await
        .unwrap()
        .is_none()
    );
    #[cfg(unix)]
    {
        let link = home.join("symlink");
        std::os::unix::fs::symlink(&root, &link).unwrap();
        assert!(
            off.ask(
                &http,
                &link,
                JevUse::Wake,
                true,
                &json!("test"),
                &questions()
            )
            .await
            .unwrap()
            .is_none()
        );
    }
    // A use cannot go live just because configuration says on.
    assert!(
        configured
            .ask(
                &http,
                &root,
                JevUse::Router,
                false,
                &json!("test"),
                &questions()
            )
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(http.calls.lock().unwrap().len(), 2);
}
// Catches: gateway identity auth mixed with Authorization and gateway losing ZDR.
#[tokio::test]
async fn explicit_gateway_uses_only_its_identity_header_and_always_zdr() {
    use ego_ai::jev::ConfiguredDecisions;
    use ego_core::config::{ConfigLayer, JevUse, Source};
    let tmp = tempfile::tempdir().unwrap();
    let config=ConfigLayer::parse(Source::User,"[jev]\nendpoint='https://gateway.example/jev/decisions'\nauth={kind='header',name='X-User-Email',value='fixture@example.test'}").unwrap().value.jev.unwrap();
    let configured = ConfiguredDecisions::new(
        enabled(config, tmp.path()),
        tmp.path().into(),
        Duration::from_secs(1),
    );
    let http = Recorder {
        calls: Arc::new(Mutex::new(vec![])),
        response: fixture()["response"].clone(),
        stall: false,
    };
    configured
        .ask(
            &http,
            tmp.path(),
            JevUse::Destructive,
            true,
            &json!("test"),
            &questions(),
        )
        .await
        .unwrap();
    let calls = http.calls.lock().unwrap();
    assert_eq!(
        calls[0]
            .headers()
            .iter()
            .find(|h| h.name() == "x-user-email")
            .unwrap()
            .value(),
        b"fixture@example.test"
    );
    assert!(
        !calls[0]
            .headers()
            .iter()
            .any(|h| h.name() == "authorization")
    );
    let body: Value = serde_json::from_slice(calls[0].body()).unwrap();
    assert_eq!(
        body["provider"],
        json!({"zdr":true,"data_collection":"deny"})
    );
    let settings = configured.settings(tmp.path()).unwrap();
    assert!(!settings.verified_zdr());
    assert!(settings.summary().contains("unverified-ZDR"));
    assert!(!settings.summary().contains("fixture@example.test"));
}

// Catches: explicit environment references ignored in favor of captured or literal keys.
#[tokio::test]
async fn explicit_api_key_gateway_resolves_the_named_environment_at_call_time() {
    use ego_ai::jev::ConfiguredDecisions;
    use ego_core::config::{ConfigLayer, JevUse, Source};
    let tmp = tempfile::tempdir().unwrap();
    // PATH is an existing non-secret environment value; no process-global mutation.
    let expected = std::env::var("PATH").unwrap();
    let config = ConfigLayer::parse(
        Source::User,
        "[jev]\nendpoint='https://gateway.example/decisions'\nauth={kind='api_key',env='PATH'}",
    )
    .unwrap()
    .value
    .jev
    .unwrap();
    let configured = ConfiguredDecisions::new(
        enabled(config, tmp.path()),
        tmp.path().into(),
        Duration::from_secs(1),
    );
    let http = Recorder {
        calls: Arc::new(Mutex::new(vec![])),
        response: fixture()["response"].clone(),
        stall: false,
    };
    configured
        .ask(
            &http,
            tmp.path(),
            JevUse::Router,
            true,
            &json!("test"),
            &questions(),
        )
        .await
        .unwrap();
    let calls = http.calls.lock().unwrap();
    assert_eq!(
        calls[0]
            .headers()
            .iter()
            .find(|h| h.name() == "authorization")
            .unwrap()
            .value(),
        format!("Bearer {expected}").as_bytes()
    );
}

// Catches: inverted/inexact calibrated bands, fabricated zero cost, off-root HTTP,
// or a stalled exchange blocking the fixed-model fallback beyond the hop deadline.
#[tokio::test]
async fn router_honours_bands_root_off_unknown_cost_and_timeout() {
    use ego_ai::jev::router::JevRouter;
    use ego_core::{
        config::{JevAuth, JevConfig, JevRoot, JevUseOverride},
        turn::{RouterStatus, TurnRouter},
    };
    let tmp = tempfile::tempdir().unwrap();
    for (p, on, stall, priced, expected) in [
        (0.15, true, false, true, RouterStatus::Cheap),
        (0.55, true, false, true, RouterStatus::Strong),
        (0.26, true, false, true, RouterStatus::Grey),
        (0.9, true, false, false, RouterStatus::Strong),
        (0.9, false, false, true, RouterStatus::Off),
        (0.9, true, true, true, RouterStatus::Timeout),
    ] {
        // Reuse the captured response envelope; inject probabilities at the policy boundary.
        let mut response = fixture()["response"].clone();
        let mut answer = response["answers"]["wake"].clone();
        answer["noul"] = json!(p);
        response["answers"] = json!({"decision":answer});
        if !priced {
            response.as_object_mut().unwrap().remove("usage");
        }
        let calls = Arc::new(Mutex::new(vec![]));
        let http = Arc::new(Recorder {
            calls: calls.clone(),
            response,
            stall,
        });
        let config = JevConfig {
            auth: JevAuth::Header {
                name: "X-Test".into(),
                value: "test".into(),
            },
            roots: vec![JevRoot {
                path: tmp.path().to_string_lossy().into(),
                endpoint: None,
                wake: None,
                destructive: None,
                router: Some(JevUseOverride {
                    on: Some(on),
                    ..Default::default()
                }),
            }],
            ..Default::default()
        };
        let router = JevRouter::new(config, tmp.path().into(), tmp.path().into(), http);
        let start = std::time::Instant::now();
        let outcome = router.route("bounded lookup").await;
        assert_eq!(outcome.status, expected);
        assert!(start.elapsed() < Duration::from_secs(2));
        assert_eq!(calls.lock().unwrap().len(), usize::from(on));
        if on && !stall {
            assert_eq!(outcome.probability.unwrap().get(), p);
            assert_eq!(outcome.cost_usd.is_some(), priced);
        }
        for call in calls.lock().unwrap().iter() {
            let request: Value = serde_json::from_slice(call.body()).unwrap();
            assert_eq!(request["state"], json!({"text":"bounded lookup"}));
            assert_eq!(
                request["provider"],
                json!({"zdr":true,"data_collection":"deny"})
            );
        }
    }
}
