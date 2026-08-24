//! RED contract for the exact session-bound `read_tool_result` operation.
//! The EvidenceStore/process contracts deliberately live in their own
//! integration tests and do not depend on this tool's parameter surface.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use ego_core::config::{Mode, PermCfg, Ruleset, Source};
use ego_core::permit::{
    Authority, CanonPath, Classification, Confinement, Op, PolicyEngine, RunConfinement,
    ToolError, ToolOutputProvenance,
};
use ego_core::store::evidence::{
    EvidenceError, EvidenceId, EvidenceRead, EvidenceRef, EvidenceStore as _, EVIDENCE_PAGE_BYTES,
};
use ego_core::turn::{CancelToken, Tools as TurnTools};
use ego_tools::exec::{ExecLimits, ExecRunConfig, GuardedExec, ProcessError, SessionEvidenceStore};
use ego_tools::fs::OsFs;
use ego_tools::tools::builtins;
use tempfile::TempDir;

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test Tokio runtime")
        .block_on(future)
}

struct Run {
    _evidence_root: TempDir,
    evidence: SessionEvidenceStore,
    executor: Arc<GuardedExec>,
    policy: PolicyEngine,
    registry: Vec<Arc<dyn ego_core::permit::Tool>>,
    fs: OsFs,
}

fn cwd() -> CanonPath {
    CanonPath::new(std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap())
}

fn open_run() -> Result<Run, ProcessError> {
    let evidence_root = tempfile::tempdir().expect("evidence root");
    let evidence = SessionEvidenceStore::open(
        evidence_root.path(),
        ego_core::event::id::SessionId::new(),
        4 * ego_core::store::evidence::EVIDENCE_PAGE_BYTES as u64,
    )
    .map_err(ProcessError::Evidence)?;
    let cancel = CancelToken::new();
    let policy = RunConfinement::new(Confinement::Off, false, cwd()).unwrap();
    let limits = ExecLimits::new(1024, Duration::from_millis(250))?;
    let config = ExecRunConfig::new(policy, evidence.clone(), cancel, limits);
    let (executor, _) = block_on(GuardedExec::for_run(config))?;
    Ok(Run {
        _evidence_root: evidence_root,
        evidence,
        executor,
        policy: evidence_policy(cwd()),
        registry: builtins(),
        fs: OsFs,
    })
}

fn evidence_policy(root: CanonPath) -> PolicyEngine {
    PolicyEngine::new(
        root,
        Mode::Plan,
        Ruleset::from_layer(
            PermCfg {
                allow: Vec::new(),
                deny: Vec::new(),
                judge: None,
            },
            Source::Project,
        ),
    )
}

fn store_evidence(store: &SessionEvidenceStore, chunks: &[&[u8]]) -> EvidenceRef {
    let mut writer = store.begin().expect("evidence staging");
    for chunk in chunks {
        writer.append(chunk).expect("evidence chunk");
    }
    writer.commit().expect("evidence commit")
}

fn request(reference: &EvidenceRef, offset: u64) -> String {
    serde_json::json!({
        "reference": reference.to_string(),
        "offset": offset,
    })
    .to_string()
}

#[test]
fn read_tool_result_params_require_a_bounded_offset_and_deny_unknown_fields() {
    let root = cwd();
    let run = open_run().unwrap();
    let reference = EvidenceRef::new(
        EvidenceId::parse("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .unwrap(),
        4,
    );
    let tools = tools(&run, root);
    for params in [
        serde_json::json!({"reference": reference.to_string()}),
        serde_json::json!({"reference": reference.to_string(), "offset": 5}),
        serde_json::json!({
            "reference": {
                "id": reference.id.as_str(),
                "bytes": reference.bytes
            },
            "offset": 0
        }),
        serde_json::json!({
            "reference": format!("{}:01", reference.id),
            "offset": 0
        }),
        serde_json::json!({"reference": reference.to_string(), "offset": 0, "path": "/etc/passwd"}),
        serde_json::json!({"reference": reference.to_string(), "offset": 0, "unknown": true}),
        serde_json::json!({}),
    ] {
        assert!(
            tools
                .prepare("read_tool_result", &params.to_string())
                .is_err(),
            "non-singleton read_tool_result params were accepted: {params}"
        );
    }
}

fn tools<'a>(run: &'a Run, root: CanonPath) -> TurnTools<'a> {
    TurnTools::new(
        &run.registry,
        &run.policy,
        &run.fs,
        run.executor.as_ref(),
        &run.evidence,
        root,
    )
}

#[test]
fn read_tool_result_prepares_exact_reference_and_session_authority() {
    let root = cwd();
    let run = open_run().unwrap();
    let reference = store_evidence(&run.evidence, &[b"complete evidence bytes"]);
    let tools = tools(&run, root);
    let (tool, call) = tools
        .prepare("read_tool_result", &request(&reference, 0))
        .expect("read_tool_result prepares");
    assert_eq!(
        call.ops,
        vec![Op::ReadEvidence {
            request: EvidenceRead::new(reference.clone(), 0).unwrap(),
        }]
    );
    let Classification::Settled(Ok(authorized)) = tools.authorize(call) else {
        panic!("same-session evidence must settle intrinsically");
    };
    assert_eq!(
        authorized.call().ops,
        vec![Op::ReadEvidence {
            request: EvidenceRead::new(reference.clone(), 0).unwrap(),
        }]
    );
    let result = block_on(tools.execute(tool, authorized)).expect("read evidence");
    assert_eq!(result.model_text(), "complete evidence bytes");
    assert_eq!(result.content.len(), 1, "one request yields one bounded page");
    let block = &result.content[0];
    assert!(block.is_bounded());
    assert!(block.text.len() <= EVIDENCE_PAGE_BYTES);
    assert_eq!(block.evidence_reference(), Some(&reference));
    assert!(matches!(
        &block.provenance,
        ToolOutputProvenance::EvidencePage {
            offset: 0,
            total: 23,
            next: None,
            eof: true,
            ..
        }
    ));
}

#[test]
fn read_tool_result_uses_original_byte_offsets_and_allows_inclusive_eof() {
    let root = cwd();
    let run = open_run().unwrap();
    let reference = store_evidence(&run.evidence, &[b"complete evidence bytes"]);
    let tools = tools(&run, root);

    let (tool, call) = tools
        .prepare("read_tool_result", &request(&reference, 9))
        .expect("offset request prepares");
    let Classification::Settled(Ok(authorized)) = tools.authorize(call) else {
        panic!("session evidence must settle intrinsically");
    };
    let result = block_on(tools.execute(tool, authorized)).expect("offset evidence");
    assert_eq!(result.model_text(), "evidence bytes");
    assert!(matches!(
        &result.content[0].provenance,
        ToolOutputProvenance::EvidencePage {
            offset: 9,
            total: 23,
            next: None,
            eof: true,
            ..
        }
    ));

    let (tool, call) = tools
        .prepare("read_tool_result", &request(&reference, reference.bytes))
        .expect("inclusive EOF request prepares");
    let Classification::Settled(Ok(authorized)) = tools.authorize(call) else {
        panic!("session evidence EOF must settle intrinsically");
    };
    let result = block_on(tools.execute(tool, authorized)).expect("EOF evidence");
    assert_eq!(result.content.len(), 1);
    assert_eq!(result.model_text(), "");
    assert!(matches!(
        &result.content[0].provenance,
        ToolOutputProvenance::EvidencePage {
            offset: 23,
            total: 23,
            next: None,
            eof: true,
            ..
        }
    ));
}

#[test]
fn read_tool_result_cross_session_reference_is_not_found() {
    let root = cwd();
    let source = open_run().unwrap();
    let target = open_run().unwrap();
    let reference = store_evidence(&source.evidence, &[b"source session evidence"]);
    let tools = tools(&target, root);
    let (tool, call) = tools
        .prepare("read_tool_result", &request(&reference, 0))
        .expect("cross-session request prepares");
    let Classification::Settled(Ok(authorized)) = tools.authorize(call) else {
        panic!("session evidence must settle before store lookup");
    };
    let error = block_on(tools.execute(tool, authorized)).unwrap_err();
    let rendered = error.to_string();
    assert!(
        matches!(error, ToolError::Evidence(EvidenceError::NotFound { .. })),
        "cross-session evidence must be a typed NotFound, not global access: {rendered}"
    );
}

#[test]
fn read_tool_result_rejects_forged_length_and_hash_references() {
    let root = cwd();
    let run = open_run().unwrap();
    let valid = store_evidence(&run.evidence, &[b"integrity-bound evidence"]);
    let forged_hash = EvidenceRef::new(
        EvidenceId::parse("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
            .unwrap(),
        valid.bytes,
    );
    let forged_length = EvidenceRef::new(valid.id.clone(), valid.bytes + 1);
    let tools = tools(&run, root);

    let (tool, call) = tools
        .prepare("read_tool_result", &request(&forged_length, 0))
        .expect("forged length remains syntactically valid");
    let Classification::Settled(Ok(authorized)) = tools.authorize(call) else {
        panic!("integrity checks happen at the session evidence reader");
    };
    let error = block_on(tools.execute(tool, authorized)).unwrap_err();
    let rendered = error.to_string();
    assert!(
        matches!(error, ToolError::Evidence(EvidenceError::WrongLength { .. })),
        "forged length must be rejected by the typed evidence verifier: {rendered}"
    );

    let (tool, call) = tools
        .prepare("read_tool_result", &request(&forged_hash, 0))
        .expect("forged hash remains syntactically valid");
    let Classification::Settled(Ok(authorized)) = tools.authorize(call) else {
        panic!("integrity checks happen at the session evidence reader");
    };
    let error = block_on(tools.execute(tool, authorized)).unwrap_err();
    let rendered = error.to_string();
    assert!(
        matches!(
            error,
            ToolError::Evidence(
                EvidenceError::NotFound { .. } | EvidenceError::Corrupt { .. }
            )
        ),
        "forged hash must be rejected by the typed evidence verifier: {rendered}"
    );
}

#[test]
fn read_tool_result_has_no_filesystem_or_rule_authority() {
    let root = cwd();
    let run = open_run().unwrap();
    let reference = store_evidence(&run.evidence, &[b"session-only evidence"]);
    let tools = tools(&run, root);

    let (tool, call) = tools
        .prepare("read_tool_result", &request(&reference, 0))
        .expect("read_tool_result prepares without a filesystem path");
    assert_eq!(
        call.ops,
        vec![Op::ReadEvidence {
            request: EvidenceRead::new(reference.clone(), 0).unwrap(),
        }]
    );
    let Classification::Settled(Ok(authorized)) = tools.authorize(call) else {
        panic!("session evidence must settle without a rule or mode approval");
    };
    assert_eq!(authorized.by(), &Authority::SessionEvidence);
    assert_eq!(
        authorized.call().ops,
        vec![Op::ReadEvidence {
            request: EvidenceRead::new(reference, 0).unwrap(),
        }]
    );
    let result = block_on(tools.execute(tool, authorized)).expect("read evidence");
    assert_eq!(result.model_text(), "session-only evidence");
}
