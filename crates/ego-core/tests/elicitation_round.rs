//! Durable elicitation round boundary regressions for regression case.

use ego_core::question::{QuestionAnswer, QuestionRequest};
use serde_json::json;

fn round() -> serde_json::Value {
    json!({
        "prompt":"MCP server alpha requires input",
        "mode":{"round":{
            "requests":{
                "profile":{"prompt":"Public profile", "kind":{"form":{"schema":{
                    "type":"object", "properties":{"name":{"type":"string"}},
                    "required":["name"], "additionalProperties":false
                }}}},
                "verify":{"prompt":"Complete verification on the page", "kind":{"url":{
                    "url":"https://example.test/verify?private=not-for-model",
                    "elicitation_id":"verify-42"
                }}}
            },
            "completed":{}
        }},
        "options":[],"free_text":false
    })
}

#[test]
fn durable_round_rejects_missing_or_replaced_correlation_before_retry() {
    // Catches: a partial answer or a replacement key authorizes a remote retry.
    let request: QuestionRequest = serde_json::from_value(round()).expect("typed round");
    request.validate().unwrap();
    for answer in [
        json!({"confirmed":true,"responses":{"profile":{"name":"Ada"}}}),
        json!({"confirmed":true,"responses":{"other":{"name":"Ada"},"verify":{"completed":true}}}),
        json!({"confirmed":false,"responses":{"profile":{"name":"Ada"},"verify":{"completed":true}}}),
        json!({"confirmed":true,"responses":{"profile":{"name":"Ada"},"verify":{"code":"secret"}}}),
    ] {
        assert!(QuestionAnswer::from_json(&request, &answer.to_string()).is_err());
    }
    let answer = QuestionAnswer::from_json(
        &request,
        &json!({
            "confirmed":true,"responses":{"profile":{"name":"Ada"},"verify":{"completed":true}}
        })
        .to_string(),
    )
    .expect("complete explicitly confirmed round");
    assert!(!answer.model_text().contains("private"));
}

#[test]
fn unsafe_url_is_refused_before_a_round_becomes_durable() {
    // Catches: a server supplies a credential-bearing or executable URL.
    for url in [
        "javascript:alert(1)",
        "https://user:password@example.test/",
        "http://example.test/",
        "https://example.test/\n",
    ] {
        let mut value = round();
        value["mode"]["round"]["requests"]["verify"]["kind"]["url"]["url"] = json!(url);
        let request: QuestionRequest = serde_json::from_value(value).expect("typed round");
        assert!(request.validate().is_err(), "unsafe URL was admitted");
    }
}

#[test]
fn ninth_member_is_refused_before_presentation() {
    // Catches: a server spends an unbounded number of human requests in one round.
    let mut value = round();
    let member = value["mode"]["round"]["requests"]["profile"].clone();
    let requests = value["mode"]["round"]["requests"].as_object_mut().unwrap();
    for index in 0..7 {
        requests.insert(format!("extra-{index}"), member.clone());
    }
    let request: QuestionRequest = serde_json::from_value(value).expect("typed round");
    assert!(request.validate().is_err());
}

#[test]
fn public_question_projection_omits_the_private_url_and_member_prompts() {
    // Catches: a host-facing URL is copied into general public events or summaries.
    let request: QuestionRequest = serde_json::from_value(round()).unwrap();
    let report = ego_core::public::QuestionReport::from_request(
        ego_core::event::id::ItemId::new(),
        &ego_core::question::QuestionTarget::ForeignContinuation {
            tool_call: ego_core::event::id::ItemId::new(),
            continuation_id: "local-operation".to_owned(),
        },
        &request,
    );
    let projected = serde_json::to_string(&report).unwrap();
    for private in [
        "example.test",
        "private=",
        "Complete verification",
        "verify-42",
    ] {
        assert!(
            !projected.contains(private),
            "private round content escaped into public output"
        );
    }
}

#[test]
fn expiry_boundary_and_previously_completed_member_are_not_reauthorized() {
    // Catches: a stale URL launch is allowed at the exact deadline or completion is replaceable.
    let mut request: QuestionRequest = serde_json::from_value(round()).unwrap();
    let ego_core::question::QuestionMode::Round(round) = &mut request.mode else {
        panic!("round")
    };
    round.expires_at_ms = Some(1000);
    assert!(!round.expired_at(999));
    assert!(round.expired_at(1000));
    assert!(round.expired_at(1001));
    round
        .complete_member("verify", json!({"completed":true}))
        .unwrap();
    assert!(
        round
            .complete_member("verify", json!({"completed":true}))
            .is_err()
    );
    assert!(
        round
            .complete_member("different-server-key", json!({"completed":true}))
            .is_err()
    );
    assert!(round.complete_member("profile", json!({"name":3})).is_err());
}
