//! contract for the core judge seam.
//!
//! The CLI integration contract proves the real TurnLoop path. This module
//! pins the public, provider-neutral judge request and decoder: the request is
//! canonical and tool-less, while every uncertain response fails closed.

use std::ffi::OsString;
use std::time::Duration;

use ego_core::config::{Mode, ModelRef};
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{ContentBlock, ErrorKind, GenRequest, StopReason, ThinkingLevel};
use ego_core::permit::judge::{
    JUDGE_REASON_MAX_BYTES, JUDGE_RESPONSE_MAX_BYTES, JudgeConfig, JudgeDecoder, JudgeInput,
    JudgeOutcome,
};
use ego_core::permit::{
    CanonPath, Confinement, EnvPolicy, Escalation, ExecCaps, Op, OpName, PreparedCall,
    RunConfinement, SpawnSpec,
};

fn config() -> JudgeConfig {
    JudgeConfig {
        identity: ModelRef("ollama/qwen3".to_string()),
        request_model: "ollama/qwen3".to_string(),
        max_output_tokens: Some(512),
    }
}

fn spawn_call() -> PreparedCall {
    let spec = SpawnSpec::new(
        vec![
            OsString::from("/usr/bin/printf"),
            OsString::from("argv-native"),
        ],
        Some(CanonPath::new("/workspace")),
        Some(Duration::from_secs(5)),
        EnvPolicy::allow_only(["PATH", "EGO_CONTRACT_SECRET"]).unwrap(),
    )
    .unwrap();
    PreparedCall::new("exec", Op::Spawn { spec })
}

fn escalation() -> Escalation {
    Escalation::ModeAsks {
        mode: Mode::Auto,
        op: OpName::Spawn,
        path: CanonPath::new("/workspace"),
    }
}

fn confinement(level: Confinement, offline: bool) -> RunConfinement {
    RunConfinement::new(level, offline, CanonPath::new("/workspace")).unwrap()
}

fn canonical_json(request: &GenRequest) -> serde_json::Value {
    let message = request
        .messages
        .messages()
        .get(1)
        .expect("judge request has one canonical user message");
    let ContentBlock::Text { text } = message
        .content
        .first()
        .expect("canonical user message has text")
    else {
        panic!("judge canonical facts are not text");
    };
    serde_json::from_str(text).expect("canonical judge facts are JSON")
}

fn decode(events: impl IntoIterator<Item = AiEvent>) -> JudgeOutcome {
    let mut decoder = JudgeDecoder::new();
    for event in events {
        decoder.observe(&event);
    }
    decoder.finish()
}

fn text_stop(text: &str) -> JudgeOutcome {
    decode([
        AiEvent::TextDelta {
            text: text.to_string(),
        },
        AiEvent::Stop(StopReason::Stop),
    ])
}

fn reason(outcome: &JudgeOutcome) -> &str {
    outcome.reason()
}

#[test]
fn separately_resolved_judge_model_is_toolless_and_not_the_main_identity() {
    let input = JudgeInput::new(
        &spawn_call(),
        &confinement(Confinement::Workspace, false),
        ExecCaps::READ_SCOPED.union(ExecCaps::NO_IP_NETWORK),
        &escalation(),
    );
    let judge_config = JudgeConfig {
        identity: ModelRef("openai/gpt-5.1".to_string()),
        request_model: "openai/gpt-5.1".to_string(),
        max_output_tokens: Some(256),
    };
    let request = input.request(&judge_config);

    assert_eq!(request.model, "openai/gpt-5.1");
    assert!(request.tools.is_empty());
    let encoded = serde_json::to_value(&request).expect("judge request serialises");
    assert!(encoded.get("tool_calls").is_none());
}

#[test]
fn judge_request_is_toolless_and_contains_only_canonical_authority_facts() {
    let input = JudgeInput::new(
        &spawn_call(),
        &confinement(Confinement::Workspace, true),
        ExecCaps::READ_SCOPED
            .union(ExecCaps::WRITE_SCOPED)
            .union(ExecCaps::NO_IP_NETWORK),
        &escalation(),
    );
    let request = input.request(&config());
    let encoded = serde_json::to_string(&request).expect("judge request serializes");

    assert_eq!(request.model, "ollama/qwen3");
    assert!(
        request.tools.is_empty(),
        "judge inherited model tools: {encoded}"
    );
    assert!(!request.parallel_tool_calls);
    assert_eq!(request.thinking, ThinkingLevel::Off);
    assert_eq!(request.max_output_tokens, Some(512));
    assert!(encoded.contains("/usr/bin/printf"));
    assert!(encoded.contains("argv-native"));
    assert!(encoded.contains("workspace"));
    assert!(encoded.contains("offline"));
    assert!(encoded.contains("EGO_CONTRACT_SECRET"));
    assert!(encoded.contains("read_scoped"));
    assert!(encoded.contains("write_scoped"));
    assert!(encoded.contains("no_ip_network"));
    assert!(!encoded.contains("EGO_CONTRACT_SECRET=value"));
    assert!(!encoded.contains("Ask"));

    let canonical = canonical_json(&request);
    let object = canonical
        .as_object()
        .expect("canonical facts are an object");
    assert_eq!(
        object.len(),
        4,
        "judge received a non-canonical fact: {canonical}"
    );
    for key in ["call", "confinement", "exec_caps", "escalation"] {
        assert!(
            object.contains_key(key),
            "canonical facts omitted {key}: {canonical}"
        );
    }

    let confinement = object
        .get("confinement")
        .and_then(serde_json::Value::as_object)
        .expect("canonical confinement is an object");
    assert_eq!(
        serde_json::Value::Object(confinement.clone()),
        serde_json::json!({
            "level": "workspace",
            "offline": true,
            "workspace": "/workspace",
        }),
        "judge confinement must be exactly the canonical three-field object"
    );
}

#[test]
fn judge_request_describes_write_shape_without_disclosing_content() {
    let call = PreparedCall::new(
        "write",
        Op::Write {
            path: CanonPath::new("/workspace/notes.txt"),
            change: ego_core::permit::Change::Whole(b"private bytes never leave prepare".to_vec()),
        },
    );
    let input = JudgeInput::new(
        &call,
        &confinement(Confinement::Ro, false),
        ExecCaps::READ_SCOPED.union(ExecCaps::WRITE_DENIED),
        &Escalation::NoRuleCovers {
            op: OpName::Write,
            path: CanonPath::new("/workspace/notes.txt"),
        },
    );
    let encoded = serde_json::to_string(&input.request(&config())).unwrap();

    assert!(encoded.contains("/workspace/notes.txt"));
    assert!(encoded.contains("write"));
    assert!(!encoded.contains("private bytes never leave prepare"));
}

#[test]
fn judge_decoder_accepts_only_the_three_strict_verdicts() {
    for (verdict, expected) in [
        ("allow", "allow"),
        ("deny", "deny"),
        ("escalate", "escalate"),
    ] {
        let outcome = text_stop(&format!(
            "{{\"verdict\":\"{verdict}\",\"reason\":\"{expected} reason\"}}"
        ));
        assert_eq!(reason(&outcome), format!("{expected} reason"));
        match (expected, outcome) {
            ("allow", JudgeOutcome::Allow { .. })
            | ("deny", JudgeOutcome::Deny { .. })
            | ("escalate", JudgeOutcome::Escalate { .. }) => {}
            _ => panic!("decoded the wrong judge verdict"),
        }
    }
}

#[test]
fn judge_decoder_maps_every_uncertain_response_to_escalate_without_retry() {
    let malformed = [
        r#"{"verdict":"allow","reason":"ok","extra":true}"#,
        r#"{"verdict":"maybe","reason":"unknown verdict"}"#,
        r#"{"verdict":"allow"}"#,
        r#"{"verdict":"allow","reason":"ok","reason":"duplicate"}"#,
        r#"{"verdict":"allow","reason":"ok"} trailing"#,
        r#"{"verdict":"allow","reason":"ok"}{"verdict":"deny","reason":"second"}"#,
    ];
    for body in malformed {
        assert!(
            matches!(text_stop(body), JudgeOutcome::Escalate { .. }),
            "malformed judge response was accepted: {body}"
        );
    }

    assert!(matches!(
        decode([AiEvent::TextDelta {
            text: r#"{"verdict":"allow","reason":"no terminal"}"#.to_string(),
        }]),
        JudgeOutcome::Escalate { .. }
    ));
    assert!(matches!(
        decode([
            AiEvent::ToolCall {
                call_id: "judge-tool".to_string(),
                name: "read".to_string(),
                args: "{}".to_string(),
            },
            AiEvent::Stop(StopReason::Stop),
        ]),
        JudgeOutcome::Escalate { .. }
    ));
    assert!(matches!(
        decode([
            AiEvent::Error {
                message: "transport".to_string(),
                kind: ErrorKind::Transient,
            },
            AiEvent::Stop(StopReason::Stop),
        ]),
        JudgeOutcome::Escalate { .. }
    ));
    assert!(matches!(
        decode([
            AiEvent::TextDelta {
                text: r#"{"verdict":"allow","reason":"wrong terminal"}"#.to_string(),
            },
            AiEvent::Stop(StopReason::Length),
        ]),
        JudgeOutcome::Escalate { .. }
    ));
}

#[test]
fn judge_decoder_enforces_an_aggregate_response_cap_before_parsing() {
    let prefix = r#"{"verdict":"allow","reason":""}"#;
    let oversized = format!("{}{}", prefix, "x".repeat(JUDGE_RESPONSE_MAX_BYTES));
    assert!(matches!(
        text_stop(&oversized),
        JudgeOutcome::Escalate { .. }
    ));
}

#[test]
fn judge_decoder_accepts_a_valid_response_at_the_exact_aggregate_cap() {
    let prefix = r#"{"verdict":"allow","reason":""}"#;
    let body = format!(
        "{{\"verdict\":\"allow\",\"reason\":\"{}\"}}",
        "x".repeat(JUDGE_RESPONSE_MAX_BYTES - prefix.len())
    );
    assert_eq!(body.len(), JUDGE_RESPONSE_MAX_BYTES);
    assert!(matches!(text_stop(&body), JudgeOutcome::Allow { .. }));
}

#[test]
fn judge_decoder_persists_a_valid_utf8_reason_inside_the_byte_bound() {
    let valid_reason = "€".repeat(JUDGE_REASON_MAX_BYTES / "€".len());
    assert_eq!(valid_reason.len(), JUDGE_REASON_MAX_BYTES - 1);
    let valid = text_stop(
        &serde_json::json!({
            "verdict": "allow",
            "reason": valid_reason.clone(),
        })
        .to_string(),
    );
    assert_eq!(reason(&valid), valid_reason);

    let reason_text = "€".repeat(500);
    let outcome = text_stop(
        &serde_json::json!({
            "verdict": "deny",
            "reason": reason_text,
        })
        .to_string(),
    );
    let persisted = reason(&outcome);
    assert!(persisted.len() <= JUDGE_REASON_MAX_BYTES);
    assert!(std::str::from_utf8(persisted.as_bytes()).is_ok());
}
