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

const TEST_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/target/judge-contract");

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
        Some(CanonPath::new(TEST_ROOT)),
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
        path: CanonPath::new(TEST_ROOT),
    }
}

fn confinement(level: Confinement, offline: bool) -> RunConfinement {
    RunConfinement::new(level, offline, CanonPath::new(TEST_ROOT)).unwrap()
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
    serde_json::from_str(
        text.trim_start_matches("<untrusted_action_json>\n")
            .trim_end_matches("\n</untrusted_action_json>"),
    )
    .expect("canonical judge facts are JSON")
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
fn judge_request_missing_program_name_cannot_assess_the_action() {
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
    assert!(
        encoded.contains("/usr/bin/printf"),
        "judge cannot identify the program"
    );
    assert!(encoded.contains("argv-native"));
    let argv = &canonical_json(&request)["call"]["operations"][0]["argv"];
    assert_eq!(argv.as_array().unwrap().len(), 2);
    assert_eq!(argv[0]["bytes"], "/usr/bin/printf".len());
    assert_eq!(argv[1]["bytes"], "argv-native".len());
    assert_eq!(argv[0]["sha256"].as_str().unwrap().len(), 64);
    assert_ne!(argv[0]["sha256"], argv[1]["sha256"]);
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
        5,
        "judge received a non-canonical fact: {canonical}"
    );
    for key in [
        "call",
        "confinement",
        "exec_caps",
        "escalation",
        "task_context",
    ] {
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
            "workspace": TEST_ROOT,
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

#[test]
fn cross_provider_secret_flag_values_are_not_disclosed_to_judge() {
    let call = PreparedCall::new(
        "exec",
        Op::Spawn {
            spec: SpawnSpec::new(
                vec![
                    "curl".into(),
                    "--token".into(),
                    "private-token-value".into(),
                    "https://example.com/?secret=value".into(),
                ],
                Some(CanonPath::new(TEST_ROOT)),
                None,
                EnvPolicy::Inherited,
            )
            .unwrap(),
        },
    );
    let input = JudgeInput::new(
        &call,
        &confinement(Confinement::Workspace, false),
        ExecCaps::READ_SCOPED,
        &escalation(),
    );
    let context = ego_core::permit::judge::JudgeTaskContext::default();
    let request =
        input.request_with_context(&config(), &context, Some(&ModelRef("openai/main".into())));
    let encoded = serde_json::to_string(&request).unwrap();
    assert!(!encoded.contains("private-token-value"));
    assert!(!encoded.contains("?secret=value"));
    assert!(encoded.contains("curl"));
    assert!(encoded.contains("example.com"));
}

#[test]
fn same_provider_raw_command_is_not_accidentally_cross_provider_redacted() {
    let input = JudgeInput::new(
        &spawn_call(),
        &confinement(Confinement::Workspace, false),
        ExecCaps::READ_SCOPED,
        &escalation(),
    );
    let request = input.request_with_context(
        &config(),
        &ego_core::permit::judge::JudgeTaskContext::default(),
        Some(&ModelRef("ollama/main".into())),
    );
    let encoded = serde_json::to_string(&request).unwrap();
    assert!(encoded.contains("argv-native"));
    assert!(encoded.contains("/usr/bin/printf"));
    assert_eq!(
        canonical_json(&request)["call"]["operations"][0]["action"]["redacted"],
        false
    );
}

#[test]
fn openrouter_provider_equality_cannot_disclose_raw_upstream_secrets() {
    let spec = SpawnSpec::new(
        vec![
            "curl".into(),
            "--token".into(),
            "private-token-value".into(),
        ],
        Some(CanonPath::new(TEST_ROOT)),
        None,
        EnvPolicy::Inherited,
    )
    .unwrap();
    let input = JudgeInput::new(
        &PreparedCall::new("exec", Op::Spawn { spec }),
        &confinement(Confinement::Workspace, false),
        ExecCaps::READ_SCOPED,
        &escalation(),
    );
    let judge = JudgeConfig {
        identity: ModelRef("openrouter/anthropic/claude-haiku-4.5".into()),
        request_model: "anthropic/claude-haiku-4.5".into(),
        max_output_tokens: Some(512),
    };
    let request = input.request_with_context(
        &judge,
        &Default::default(),
        Some(&ModelRef("openrouter/anthropic/claude-sonnet".into())),
    );
    assert!(
        !serde_json::to_string(&request)
            .unwrap()
            .contains("private-token-value")
    );
    assert_eq!(
        canonical_json(&request)["call"]["operations"][0]["action"]["redacted"],
        true
    );
}

// Catches: a single fenced closed verdict is mistaken for malformed provider prose.
#[test]
fn judge_decoder_accepts_only_one_complete_json_fence() {
    for (body, expected) in [
        (
            "```json\n{\"verdict\":\"allow\",\"reason\":\"bounded\"}\n```",
            "allow",
        ),
        (
            " \n```\n{\"verdict\":\"deny\",\"reason\":\"bounded\"}\n```\n ",
            "deny",
        ),
        (
            "```json\r\n{\"verdict\":\"escalate\",\"reason\":\"bounded\"}\r\n```",
            "escalate",
        ),
    ] {
        let outcome = text_stop(body);
        let actual = match &outcome {
            JudgeOutcome::Allow { .. } => "allow",
            JudgeOutcome::Deny { .. } => "deny",
            JudgeOutcome::Escalate { .. } => "escalate",
        };
        assert_eq!(actual, expected, "{body}");
        assert_eq!(outcome.reason(), "bounded");
    }
    for body in [
        "prose\n```json\n{\"verdict\":\"allow\",\"reason\":\"bounded\"}\n```",
        "```json\n{\"verdict\":\"allow\",\"reason\":\"bounded\"}\n```\nprose",
        "```json\n{\"verdict\":\"allow\",\"reason\":\"bounded\"}",
        "```json\n{\"verdict\":\"allow\",\"reason\":\"bounded\"}\n```\n```json\n{}\n```",
        "```json\n{\"verdict\":\"allow\",\"reason\":\"first\",\"reason\":\"second\"}\n```",
        "```json\n{\"verdict\":\"allow\",\"reason\":\"bounded\",\"extra\":true}\n```",
        "```python\n{\"verdict\":\"allow\",\"reason\":\"bounded\"}\n```",
    ] {
        assert!(
            matches!(text_stop(body), JudgeOutcome::Escalate { .. }),
            "{body}"
        );
    }
    let mut decoder = JudgeDecoder::new();
    decoder.observe(&AiEvent::TextDelta {
        text: "```json\n{\"verdict\":\"allow\",\"reason\":\"bounded\"}\n```".into(),
    });
    assert!(matches!(decoder.finish(), JudgeOutcome::Escalate { .. }));
}

// Catches: a judge approval for unique replacement is reused for an explicit bulk edit.
#[test]
fn replace_all_mode_changes_judge_identity_and_visible_evidence() {
    let make = |replace_all| {
        PreparedCall::new(
            "edit",
            Op::Write {
                path: CanonPath::new(TEST_ROOT),
                change: ego_core::permit::Change::Replace {
                    replace_all,
                    old_string: "old".into(),
                    new_string: "new".into(),
                },
            },
        )
    };
    let unique = make(false);
    let bulk = make(true);
    assert_ne!(unique.authority_fingerprint(), bulk.authority_fingerprint());
    let facts = |call: &PreparedCall| {
        canonical_json(
            &JudgeInput::new(
                call,
                &confinement(Confinement::Workspace, false),
                ExecCaps::READ_SCOPED,
                &escalation(),
            )
            .request(&config()),
        )
    };
    let unique_facts = facts(&unique);
    let bulk_facts = facts(&bulk);
    assert_ne!(unique_facts, bulk_facts);
    assert!(bulk_facts.to_string().contains("\"replace_all\":true"));
    assert!(unique_facts.to_string().contains("\"replace_all\":false"));
}
