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
