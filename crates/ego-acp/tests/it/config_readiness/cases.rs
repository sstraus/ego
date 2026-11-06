use super::*;
use ego_acp::serve::{SessionConfinementAdmission, SessionConfinementFuture};
use serde_json::{Value, json};

fn recording() -> Vec<Value> {
    include_str!("../../fixtures/816/public-ego-delayed-config.jsonl")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

async fn config_update(connection: &mut support::Connected) -> Value {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let frame = connection.frame().await;
            if frame["params"]["update"]["sessionUpdate"] == "config_option_update" {
                break frame["params"]["update"].clone();
            }
        }
    })
    .await
    .expect("deferred configuration must settle after admission completes")
}

// Catches: clients cannot distinguish an unmeasured offer from unsupported controls,
// or completion of a failed measurement from an offer that is still pending.
#[tokio::test]
async fn deferred_config_readiness_reports_pending_then_terminal() {
    let captured = recording();
    let response_index = captured.iter().position(|v| v["id"] == 2).unwrap();
    let update_index = captured
        .iter()
        .position(|v| v["params"]["update"]["sessionUpdate"] == "config_option_update")
        .unwrap();
    assert!(response_index < update_index);
    assert!(
        captured[response_index]["result"]
            .get("configOptions")
            .is_none()
    );

    for failed in [false, true] {
        let fixture = support::DurableAcpFixture::new();
        let admission = Arc::new(Admission::default());
        admission.blocks();
        admission.fail.store(failed, Ordering::SeqCst);
        let controls: Arc<dyn SessionControls> = Arc::new(Seat::new(Confinement::Ro));
        let mut connection =
            support::Connected::new(router(&fixture, Some(controls), Arc::clone(&admission)));
        connection.initialize().await;
        let created = connection.new_session(ROOT).await;
        assert!(
            created["result"].get("configOptions").is_none(),
            "{created}"
        );
        admission.release();
        let update = config_update(&mut connection).await;
        connection.abort().await;
        assert_eq!(
            created["result"]["_meta"]["ego"]["configOptions"]["state"], "pending",
            "deferred response is ambiguous: {created}"
        );
        assert_eq!(
            update["_meta"]["ego"]["configOptions"]["state"],
            if failed { "failed" } else { "ready" },
            "{update}"
        );
        if failed {
            assert_eq!(update["configOptions"], json!([]));
            assert!(update["_meta"]["ego"]["error"].is_object(), "{update}");
        } else {
            assert_eq!(update["configOptions"].as_array().unwrap().len(), 4);
        }
        assert_eq!(fixture.executor_effect_counts(), (0, 0, 0));
    }
}

struct RecordedRefusal;

impl SessionConfinementAdmission for RecordedRefusal {
    fn initialize(&self, _session: SessionId, _root: RunRoot) -> SessionConfinementFuture {
        Box::pin(async { Ok(()) })
    }

    fn observation(
        &self,
        _session: SessionId,
        _confinement: Confinement,
    ) -> Result<ExecObservation, ControlError> {
        let frame: Value = serde_json::from_str(include_str!(
            "../../fixtures/816/public-ego-sandbox-refusal.json"
        ))
        .unwrap();
        Err(ControlError::Refused {
            control: "sandbox",
            why: frame["event"]["message"].as_str().unwrap().to_owned(),
        })
    }
}

// Catches: a configured but unachievable default looks usable until the first prompt.
#[tokio::test]
async fn unsupported_current_sandbox_explains_recorded_refusal_before_prompt() {
    let captured = recording();
    let options = captured
        .iter()
        .find_map(|v| v["params"]["update"]["configOptions"].as_array())
        .unwrap();
    let sandbox = options.iter().find(|v| v["id"] == "sandbox").unwrap();
    assert_eq!(sandbox["currentValue"], "workspace");
    assert_eq!(sandbox["options"], json!([]));
    let mut seat = Seat::new(sandbox["currentValue"].as_str().unwrap().parse().unwrap());
    seat.available = sandbox["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["value"].as_str().unwrap().parse().unwrap())
        .collect();
    let fixture = support::DurableAcpFixture::new();
    let mut connection = support::Connected::new(fixture.router_with_confinement_and_controls(
        Some(Arc::new(seat)),
        Some(Arc::new(RecordedRefusal)),
    ));
    connection.initialize().await;
    let created = connection.new_session(ROOT).await;
    let update = config_update(&mut connection).await;
    connection.abort().await;
    let sandbox = update["configOptions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["id"] == "sandbox")
        .unwrap();
    assert_eq!(sandbox["currentValue"], "workspace");
    assert_eq!(sandbox["options"], json!([]));
    let reason = sandbox["_meta"]["ego"]["unavailableReason"]
        .as_str()
        .unwrap_or_default();
    assert!(
        reason.contains("no IP networking for spawned processes"),
        "measured refusal was hidden before prompt: {sandbox}; created={created}"
    );
    assert!(
        sandbox["description"]
            .as_str()
            .unwrap()
            .contains("unavailable"),
        "{sandbox}"
    );
    assert_eq!(fixture.executor_effect_counts(), (0, 0, 0));
}
