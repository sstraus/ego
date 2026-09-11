//! contract for transactional, bounded process evidence.
//!
//! Evidence retrieval is an exact session capability: the reference and
//! original-byte offset are both part of the request that policy settles and
//! the guarded reader consumes. Storage is incremental and transactional;
//! process failures carry committed output only for the two statuses observed
//! after both pipes have drained.

use ego_core::event::domain::{DomainEvent, Envelope, EventKind};
use ego_core::event::id::{EventId, ItemId, Producer, Seq, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::permit::{
    Authority, CanonPath, Classification, EnvPolicy, Op, PolicyEngine, ProcessError,
    StreamProvenance, ToolContentBlock, ToolError, ToolOutputProvenance, ToolResult,
};
use ego_core::public::{PUBLIC_EVENT_VERSION, PublicEvent, project_public};
use ego_core::state::{ItemKind, SessionState};
use ego_core::store::codec::{CODEC_VERSION, StoreLine, decode_line, encode_record};
use ego_core::store::evidence::{
    EVIDENCE_PAGE_BYTES, EvidenceError, EvidenceId, EvidencePage, EvidenceRead, EvidenceRef,
};
use serde_json::json;

fn reference(bytes: u64) -> EvidenceRef {
    EvidenceRef::new(
        EvidenceId::parse("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
            .unwrap(),
        bytes,
    )
}

fn max_reference() -> EvidenceRef {
    reference(u64::MAX)
}

fn process_result() -> ToolResult {
    ToolResult::new(vec![
        ToolContentBlock::process(StreamProvenance::Stdout, b"stdout bytes", 64, reference(12)),
        ToolContentBlock::process(StreamProvenance::Stderr, b"stderr bytes", 64, reference(12)),
    ])
}

#[test]
fn evidence_ref_has_one_canonical_string_wire_and_schema() {
    let reference = reference(42);
    let encoded = serde_json::to_value(&reference).expect("evidence reference serialises");
    assert_eq!(encoded, json!(reference.to_string()));
    assert_eq!(
        serde_json::from_value::<EvidenceRef>(encoded).expect("canonical reference decodes"),
        reference
    );

    let schema = schemars::schema_for!(EvidenceRef).to_value();
    assert_eq!(schema["type"], "string");
    assert!(
        schema.get("properties").is_none(),
        "reference schema became an object"
    );

    let hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    for malformed in [
        format!("{}:01", hash),
        format!("{}:+1", hash),
        format!("{}:1:0", hash),
        hash.to_string(),
        format!("{}:1", hash.to_ascii_uppercase()),
        format!("{}:1", &hash[..63]),
        format!("{hash}0:1"),
    ] {
        assert!(
            serde_json::from_value::<EvidenceRef>(json!(malformed.clone())).is_err(),
            "non-canonical evidence reference was accepted: {malformed}"
        );
    }
}

#[test]
fn evidence_read_is_strict_offset_bounded_and_eof_inclusive() {
    let reference = reference(9);
    let request = EvidenceRead::new(reference.clone(), 4).expect("offset inside reference");
    let wire = serde_json::to_value(&request).expect("evidence request serialises");
    assert_eq!(wire["reference"], json!(reference.to_string()));
    assert_eq!(wire["offset"], 4);

    assert!(
        serde_json::from_value::<EvidenceRead>(json!({
            "reference": reference.to_string()
        }))
        .is_err(),
        "offset is required"
    );
    assert!(
        serde_json::from_value::<EvidenceRead>(json!({
            "reference": reference.to_string(),
            "offset": 4,
            "unknown": true
        }))
        .is_err(),
        "unknown request fields must not be accepted"
    );
    assert!(EvidenceRead::new(reference.clone(), 10).is_err());
    assert!(EvidenceRead::new(reference.clone(), reference.bytes).is_ok());
    assert!(
        serde_json::from_value::<EvidenceRead>(json!({
            "reference": reference.to_string(),
            "offset": 10
        }))
        .is_err(),
        "wire decoding must apply the same bound as the constructor"
    );
}

#[test]
fn evidence_page_has_fixed_size_and_truthful_interval_metadata() {
    assert_eq!(EVIDENCE_PAGE_BYTES, 32 * 1024);
    // The page starts two bytes before the page boundary and carries five
    // bytes, so its interval ends exactly at PAGE + 3 and is genuinely final.
    let final_reference = reference((EVIDENCE_PAGE_BYTES as u64) + 3);
    let page: EvidencePage = serde_json::from_value(json!({
        "reference": final_reference.to_string(),
        "offset": EVIDENCE_PAGE_BYTES as u64 - 2,
        "bytes": [1, 2, 3, 4, 5],
        "next_offset": null
    }))
    .expect("a final page at the declared end is valid");
    let wire = serde_json::to_value(page).expect("page serialises");
    assert_eq!(wire["reference"], json!(final_reference.to_string()));
    assert_eq!(wire["offset"], (EVIDENCE_PAGE_BYTES as u64) - 2);
    assert_eq!(wire["bytes"].as_array().unwrap().len(), 5);
    assert!(wire["next_offset"].is_null());

    let incomplete_reference = reference((EVIDENCE_PAGE_BYTES as u64) + 4);
    assert!(
        serde_json::from_value::<EvidencePage>(json!({
            "reference": incomplete_reference.to_string(),
            "offset": EVIDENCE_PAGE_BYTES as u64 - 2,
            "bytes": [1, 2, 3, 4, 5],
            "next_offset": null
        }))
        .is_err(),
        "a non-final page must carry its next offset"
    );
    assert!(
        serde_json::from_value::<EvidencePage>(json!({
            "reference": final_reference.to_string(),
            "offset": EVIDENCE_PAGE_BYTES as u64 - 2,
            "bytes": [1, 2, 3, 4, 5],
            "next_offset": final_reference.bytes
        }))
        .is_err(),
        "an EOF page must not carry a next offset"
    );

    let oversized = serde_json::from_value::<EvidencePage>(json!({
        "reference": final_reference.to_string(),
        "offset": 0,
        "bytes": vec![0_u8; EVIDENCE_PAGE_BYTES + 1],
        "next_offset": EVIDENCE_PAGE_BYTES
    }));
    assert!(oversized.is_err(), "a page may never widen the fixed bound");

    assert!(EvidencePage::new(max_reference(), u64::MAX - 1, vec![1, 2]).is_err());
}

#[test]
fn read_evidence_authority_covers_the_exact_reference_and_offset_only() {
    let root = CanonPath::new("/workspace");
    let admitted = EvidenceRead::new(reference(32), 0).unwrap();
    let policy = PolicyEngine::new(root, ego_core::config::Mode::Plan, Default::default());
    let call = ego_core::permit::PreparedCall::new(
        "read_tool_result",
        Op::ReadEvidence {
            request: admitted.clone(),
        },
    );

    let Classification::Settled(Ok(authorized)) = policy.classify(call) else {
        panic!("session evidence must settle intrinsically");
    };
    assert_eq!(authorized.by(), &Authority::SessionEvidence);
    assert_eq!(
        authorized.call().ops()[0],
        Op::ReadEvidence {
            request: admitted.clone(),
        }
    );
    assert!(authorized.permit().allows_evidence(&admitted).is_ok());
    assert!(
        authorized
            .permit()
            .allows_evidence(&EvidenceRead::new(reference(33), 0).unwrap())
            .is_err()
    );
}

#[test]
fn process_provenance_names_encoding_and_never_lossily_replaces_binary() {
    let binary = [0xff, 0xfe, 0x00, 0x80, 0x41];
    let block = ToolContentBlock::process(
        StreamProvenance::Stdout,
        &binary,
        64,
        reference(binary.len() as u64),
    );
    assert!(!block.text.contains('\u{fffd}'));
    assert!(
        block.text.to_ascii_lowercase().contains("base64"),
        "binary process previews must label their model-visible encoding"
    );
    let model_text = ToolResult::new(vec![block.clone()]).model_text();
    assert!(
        model_text.contains("[base64]\n"),
        "binary process model text must label its base64 payload"
    );
    let wire = serde_json::to_value(&block).expect("binary process block serialises");
    assert_eq!(wire["provenance"]["source"], "process");
    assert_eq!(wire["provenance"]["encoding"], "base64");
    assert_eq!(
        wire["full_output"],
        reference(binary.len() as u64).to_string()
    );

    let utf8 = ToolContentBlock::process(
        StreamProvenance::Stdout,
        "exact utf-8 π".as_bytes(),
        64,
        reference(14),
    );
    let wire = serde_json::to_value(&utf8).expect("utf8 process block serialises");
    assert_eq!(wire["provenance"]["encoding"], "utf8");
    assert_eq!(utf8.text, "exact utf-8 π");
}

#[test]
fn evidence_page_binary_model_text_is_explicitly_base64() {
    let binary = vec![0xff, 0xfe, 0x00, 0x80, 0x41];
    let page = EvidencePage::new(reference(binary.len() as u64), 0, binary)
        .expect("binary evidence page is valid");
    let block = ToolContentBlock::evidence_page(page);

    assert!(!block.text.contains('\u{fffd}'));
    assert!(
        block.text.to_ascii_lowercase().contains("base64"),
        "binary evidence pages must label their model-visible encoding"
    );
    let model_text = ToolResult::new(vec![block.clone()]).model_text();
    assert!(
        model_text.starts_with("[base64]\n"),
        "binary evidence-page model text must label its base64 payload"
    );
    let wire = serde_json::to_value(&block).expect("binary evidence page serialises");
    assert_eq!(wire["provenance"]["source"], "evidence_page");
    assert_eq!(wire["provenance"]["encoding"], "base64");
}

#[test]
fn binary_process_head_tail_preview_has_one_labelled_payload_and_retrieval_ref() {
    let head = [0xff, 0x00, 0xfe];
    let tail = [0x80, 0x41, 0x81];
    let complete = reference(64);
    let block =
        ToolContentBlock::process_preview(StreamProvenance::Stdout, &head, &tail, complete.clone());

    assert!(!block.text.contains('\u{fffd}'));
    assert_eq!(
        block.text.lines().next(),
        Some("[base64 head+tail preview; 58 bytes omitted between retained segments]")
    );
    assert_eq!(block.text.matches("/wD+gEGB").count(), 1);
    assert!(
        block
            .text
            .contains(&format!("[read_tool_result {complete}]"))
    );
    let model_text = ToolResult::new(vec![block.clone()]).model_text();
    assert!(!model_text.contains('\u{fffd}'));
    assert_eq!(model_text.matches("/wD+gEGB").count(), 1);
    assert!(
        model_text
            .contains("[base64 head+tail preview; 58 bytes omitted between retained segments]")
    );
    assert!(matches!(
        block.provenance,
        ToolOutputProvenance::Process {
            encoding: ego_core::permit::ToolOutputEncoding::Base64,
            truncated: true,
            omitted_bytes: 58,
            ..
        }
    ));
}

#[test]
fn oversized_preview_is_incremental_head_tail_and_retrievable() {
    let bytes = vec![b'h'; 4096];
    let block = ToolContentBlock::process(
        StreamProvenance::Stdout,
        &bytes,
        64,
        reference(bytes.len() as u64),
    );
    assert!(block.is_truncated());
    assert!(block.preview_is_head_and_tail());
    assert!(block.text.len() < bytes.len());
    assert!(block.text.contains("omitted"));
    assert!(block.evidence_reference().is_some());
    let wire = serde_json::to_value(&block).unwrap();
    assert_eq!(wire["provenance"]["omitted_bytes"], 4032);
}

#[test]
fn exit_and_signal_failures_carry_only_fully_committed_stream_results() {
    let result = process_result();
    let exit = ProcessError::Exit {
        code: Some(17),
        result: result.clone(),
    };
    let signal = ProcessError::Signal {
        signal: Some(15),
        result: result.clone(),
    };
    for error in [exit, signal] {
        let ToolError::Process(error) = ToolError::from(error) else {
            unreachable!()
        };
        let failed = error.into_failed_result();
        assert_eq!(failed.content.len(), 3);
        assert!(matches!(
            failed.content[0].provenance,
            ToolOutputProvenance::Process {
                stream: StreamProvenance::Stdout,
                ..
            }
        ));
        assert!(matches!(
            failed.content[1].provenance,
            ToolOutputProvenance::Process {
                stream: StreamProvenance::Stderr,
                ..
            }
        ));
        assert!(matches!(
            &failed.content[2].provenance,
            ToolOutputProvenance::Builtin
        ));
    }
}

#[test]
fn failed_process_diagnostic_is_one_bounded_utf8_builtin_after_streams() {
    use ego_core::permit::MAX_TOOL_ERROR_DIAGNOSTIC_BYTES;

    assert_eq!(MAX_TOOL_ERROR_DIAGNOSTIC_BYTES, 4 * 1024);
    let result = process_result();
    let error = ToolError::from(ProcessError::Exit {
        code: Some(17),
        result,
    });
    let failed = error.into_failed_result();
    let diagnostic = failed.content.last().expect("diagnostic block");
    assert!(matches!(
        &diagnostic.provenance,
        ToolOutputProvenance::Builtin
    ));
    assert!(diagnostic.text.is_char_boundary(diagnostic.text.len()));
    assert!(diagnostic.text.len() <= MAX_TOOL_ERROR_DIAGNOSTIC_BYTES);
    assert_eq!(
        failed
            .content
            .iter()
            .filter(|block| matches!(block.provenance, ToolOutputProvenance::Builtin))
            .count(),
        1
    );
}

#[test]
fn non_status_process_errors_never_claim_completed_output() {
    let errors = [
        ProcessError::Launch {
            executable: "missing".into(),
            reason: "not found".into(),
        },
        ProcessError::Backend("pipe failed".into()),
        ProcessError::Invariant("writer failed".into()),
        ProcessError::Timeout {
            timeout: std::time::Duration::from_secs(1),
        },
        ProcessError::Cancelled,
        ProcessError::EvidenceLimit {
            stream: StreamProvenance::Stdout,
            limit: 4,
        },
        ProcessError::Evidence(EvidenceError::Io {
            message: "fsync failed".into(),
        }),
    ];
    for error in errors {
        let failed = ToolError::from(error).into_failed_result();
        assert_eq!(failed.content.len(), 1);
        assert!(matches!(
            &failed.content[0].provenance,
            ToolOutputProvenance::Builtin
        ));
        assert!(failed.content[0].full_output.is_none());
    }
}

fn event(item: Option<ItemId>, seq: Seq, kind: EventKind) -> DomainEvent {
    DomainEvent {
        env: Envelope {
            seq,
            id: EventId::new(),
            turn: TurnId::new(),
            item,
            parent: None,
            ts: Timestamp::EPOCH,
            producer: Producer::Main,
        },
        kind,
    }
}

#[test]
fn failed_structured_result_survives_turn_reduction_and_history_roundtrip() {
    let item = ItemId::new();
    let mut seq = Seq::default();
    let started = event(
        Some(item),
        seq.next(),
        EventKind::ToolCallStarted {
            name: "exec".into(),
            args: "{}".into(),
            call_id: Some("call-1".into()),
        },
    );
    let result = process_result();
    let completed = event(
        Some(item),
        seq.next(),
        EventKind::ToolCallCompleted {
            ok: false,
            result: result.clone(),
        },
    );

    let mut state = SessionState::default();
    state.apply(&started);
    state.apply(&completed);
    let item_state = state.items.get(&item).expect("tool item reduced");
    assert_eq!(item_state.kind, ItemKind::ToolCall);
    assert_eq!(item_state.ok, Some(false));
    assert_eq!(item_state.tool_result.as_ref(), Some(&result));
    assert!(item_state.text.is_empty());

    let bytes = encode_record(completed.env.seq, &completed).expect("codec encodes v12 result");
    let StoreLine::Record(record) = decode_line(&bytes).expect("codec decodes v12 result") else {
        panic!("structured result must be a durable record");
    };
    let decoded: DomainEvent = serde_json::from_str(record.payload.get()).unwrap();
    assert_eq!(decoded.kind, completed.kind);
}

#[test]
fn failed_structured_result_public_projection_stays_text_only_with_current_version() {
    assert_eq!(CODEC_VERSION, 12);
    assert_eq!(PUBLIC_EVENT_VERSION, 10);
    let item = ItemId::new();
    let completed = event(
        Some(item),
        Seq::default(),
        EventKind::ToolCallCompleted {
            ok: false,
            result: process_result(),
        },
    );
    let Some(PublicEvent::ItemCompleted { ok, text, .. }) = project_public(&completed) else {
        panic!("failed tool completion remains publicly projectable");
    };
    assert!(!ok);
    let text = text.expect("public event carries rendered tool text");
    assert!(text.contains("stdout") && text.contains("stderr"));
    let wire = serde_json::to_value(project_public(&completed).unwrap()).unwrap();
    assert!(wire.get("full_output").is_none());
    assert!(wire.get("provenance").is_none());
}

#[test]
fn evidence_requests_do_not_smuggle_environment_values_into_identity() {
    let policy = EnvPolicy::allow_only(["PATH", "EGO_SECRET"]).unwrap();
    let rendered = format!("{policy:?}");
    assert!(rendered.contains("PATH"));
    assert!(rendered.contains("EGO_SECRET"));
    assert!(!rendered.contains("EGO_SECRET="));
}
