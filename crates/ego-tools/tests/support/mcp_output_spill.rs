use ego_core::event::domain::EventKind;
use ego_core::event::id::ItemId;
use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::mcp::{
    CancellationClassification, ForeignContent, ForeignResult, McpAdmissionFingerprint,
};
use ego_core::permit::InvocationDigest;
use ego_core::store::evidence::{EvidenceId, EvidenceRef};
use ego_tools::store::SessionEvidenceStore;

fn foreign(text: &str) -> ToolResult {
    ToolResult::from_foreign(
        ForeignResult::new(
            "fixture",
            "large",
            InvocationDigest::from_bytes(b"{}"),
            McpAdmissionFingerprint::new("capture", "large", "schema").unwrap(),
            "object",
            None,
            None,
            vec![ForeignContent::text(text)],
            EvidenceRef::new(EvidenceId::parse("0".repeat(64)).unwrap(), 0),
            DeliveryCertainty::DefinitelySent,
            CancellationClassification::Completed,
        )
        .unwrap(),
    )
}

fn output_path(result: &ToolResult) -> Option<PathBuf> {
    serde_json::to_value(result).unwrap()["provenance"]["result"]["output_path"]
        .as_str()
        .map(PathBuf::from)
}

fn spilled_path(result: &ToolResult) -> PathBuf {
    output_path(result).expect("published output path")
}

// Uses repository-owned projection over real file bytes, not a wire-server fixture.
struct ForeignRead;
impl TypedTool for ForeignRead {
    type Params = ego_tools::tools::ReadParams;
    fn name(&self) -> &str {
        "fixture_foreign"
    }
    fn description(&self) -> &str {
        "Project local text through the MCP result boundary"
    }
    fn prepare_typed(
        &self,
        params: Self::Params,
        ctx: &PrepareCtx<'_>,
    ) -> Result<PreparedCall, ToolError> {
        Ok(PreparedCall::new(
            self.name(),
            Op::Read {
                path: ctx
                    .resolver
                    .canonicalize(&ctx.cwd.as_path().join(params.path))?,
            },
        ))
    }
    fn execute<'a, 'io>(
        &'a self,
        call: PreparedCall,
        io: &'a dyn GuardedIo<'io>,
    ) -> BoxFuture<'a, Result<ToolResult, ToolError>>
    where
        'io: 'a,
    {
        Box::pin(async move {
            let [Op::Read { path }] = call.ops() else {
                panic!("read fixture");
            };
            Ok(foreign(&String::from_utf8(io.read(path)?).unwrap()))
        })
    }
}

// Catches: oversized MCP text is discarded, persisted inline, or spilled outside the read perimeter.
#[test]
fn mcp_output_spill_large_result_keeps_full_bytes_readable_and_events_small() {
    let ws = populated();
    let directory = TempDir::new().unwrap();
    let session = SessionId::new();
    let evidence = SessionEvidenceStore::open(directory.path(), session, 1024 * 1024).unwrap();
    let text = format!("{}\nTAIL ONLY IN FILE\n", "row\n".repeat(30_000));
    let invocation = RunInvocationId::new();
    let item = ItemId::new();
    let mut answer = foreign(&text);
    answer.spill_mcp_output(Some(&evidence), invocation, item);
    let path = spilled_path(&answer);
    assert!(path.is_absolute());
    assert_eq!(
        path.file_name().unwrap().to_str().unwrap(),
        format!("{invocation}-{item}.txt")
    );
    assert_eq!(
        path.parent().unwrap().parent().unwrap(),
        directory.path().join(session.to_string())
    );
    assert_eq!(fs::read(&path).unwrap(), text.as_bytes());
    assert!(answer.model_text().starts_with(&"row\n".repeat(5120)));
    assert!(
        answer
            .model_text()
            .contains("showing first 20480 bytes of 120019 bytes")
    );
    assert!(answer.model_text().contains("grep(query, path)"));
    assert!(!answer.model_text().contains("TAIL ONLY IN FILE"));
    let event = serde_json::to_string(&EventKind::ToolCallCompleted {
        ok: true,
        result: answer,
    })
    .unwrap();
    assert!(event.contains(path.to_str().unwrap()));
    assert!(!event.contains("TAIL ONLY IN FILE"));
    // JSON escapes 5120 newlines in each of the two existing preview fields.
    assert!(event.len() < 60_000);

    let mut same = foreign(&text);
    same.spill_mcp_output(Some(&evidence), invocation, item);
    assert_eq!(spilled_path(&same), path);
    assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);

    // Repeating the same call must keep the first complete file, not overwrite it.
    let mut retry = foreign(&"different\n".repeat(3000));
    retry.spill_mcp_output(Some(&evidence), invocation, item);
    assert!(output_path(&retry).is_none());
    assert_eq!(fs::read(&path).unwrap(), text.as_bytes());

    let capture = GenerationPolicyCapture::new(ws.cwd(), Mode::Default, Ruleset::default(), None)
        .unwrap()
        .with_session_read_root(evidence.mcp_output_root().unwrap().unwrap())
        .unwrap();
    let snapshot =
        GenerationPolicySnapshot::decode_confidential(&capture.snapshot().encode_confidential())
            .unwrap();
    let restored = GenerationPolicyCapture::restore(Arc::new(snapshot), None)
        .unwrap()
        .policy();
    assert!(matches!(
        restored.classify(PreparedCall::new(
            "write",
            Op::Write {
                path: CanonPath::new(&path),
                change: Change::Whole(b"replaced".to_vec()),
            }
        )),
        Classification::Settled(Err(_))
    ));
    let private = path
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("manifest.json");
    assert!(matches!(
        restored.classify(PreparedCall::new(
            "read",
            Op::Read {
                path: CanonPath::new(private)
            }
        )),
        Classification::Settled(Err(_))
    ));

    ws.file("large.txt", &text);
    let mut registry = ws.registry.clone();
    registry.push(Arc::new(Typed(ForeignRead)));
    let store = JsonlStore::new(directory.path(), &ws.root, Box::new(OsIo));
    let generator = Arc::new(FakeGenerator::new([
        vec![
            AiEvent::ToolCall {
                call_id: "read-spill".into(),
                name: "read".into(),
                args: json!({"path":path}).to_string(),
            },
            AiEvent::ToolCall {
                call_id: "foreign-spill".into(),
                name: "fixture_foreign".into(),
                args: json!({"path":"large.txt"}).to_string(),
            },
            AiEvent::Stop(StopReason::ToolUse),
        ],
        vec![
            AiEvent::TextDelta {
                text: "done".into(),
            },
            AiEvent::Stop(StopReason::Stop),
        ],
    ]));
    let sink = CollectingSink::new();
    let clock = SystemClock;
    let mut loop_ = TurnLoop::new(
        Session {
            id: session,
            state: SessionState::default(),
            store: &store,
            evidence: &NoEvidence,
            clock: &clock,
            sink: &sink,
            cancel: CancelToken::new(),
        },
        GenerationRuntime {
            policy_source: Arc::new(FixedPolicySource::new(
                ws.cwd(),
                Mode::Default,
                Ruleset::default(),
            )),
            source: Arc::new(FixedSource {
                generator: generator.clone(),
            }),
            gate: SharedBudgetGate::new(BudgetGate::new(usd("1000"))),
            config: TurnConfig {
                cache: CacheRetention::Short,
                system: None,
                identity: None,
                token_ceiling: None,
                retry: 0,
                fallbacks: Vec::new(),
            },
            admission: Arc::new(EvidenceAdmissionService::new()),
            invocation: RunInvocationId::new(),
            retry_pause: Arc::new(NoRetryPause),
        },
        Tools::new(
            &registry,
            ToolEffectPorts::new(
                &ws.fs,
                &ws.process,
                &evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            ws.cwd(),
        ),
    )
    .unwrap();
    let outcome = block_on(loop_.run("read the full MCP output")).unwrap();
    assert!(outcome.ok);
    let state = loop_.state();
    let foreign = state
        .items
        .values()
        .find(|item| item.tool_name.as_deref() == Some("fixture_foreign"))
        .unwrap()
        .tool_result
        .as_ref()
        .unwrap();
    assert_eq!(fs::read_to_string(spilled_path(foreign)).unwrap(), text);
    assert!(!foreign.model_text().contains("TAIL ONLY IN FILE"));
    assert!(serde_json::to_string(foreign).unwrap().len() < 60_000);
    assert!(format!("{:?}", generator.calls()[1].messages).contains("TAIL ONLY IN FILE"));
}

// Catches: the threshold is off by one or small MCP results gain a notice/file.
#[test]
fn mcp_output_spill_small_result_is_untouched_including_exact_limit() {
    let directory = TempDir::new().unwrap();
    let session = SessionId::new();
    let store = SessionEvidenceStore::open(directory.path(), session, 1024 * 1024).unwrap();
    for text in [String::new(), "small\n".into(), "x".repeat(20480)] {
        let mut answer = foreign(&text);
        answer.spill_mcp_output(Some(&store), RunInvocationId::new(), ItemId::new());
        assert_eq!(answer.model_text(), text);
    }
    assert!(!directory.path().join(session.to_string()).exists());
}

// Catches: a failed spill fails the tool call or advertises a nonexistent full-output file.
#[test]
fn mcp_output_spill_write_failure_returns_preview_without_path() {
    let directory = TempDir::new().unwrap();
    let session = SessionId::new();
    let output = directory
        .path()
        .join(session.to_string())
        .join("mcp-output");
    fs::create_dir_all(output.parent().unwrap()).unwrap();
    fs::write(&output, b"not a directory").unwrap();
    let store = SessionEvidenceStore::open(directory.path(), session, 1024 * 1024).unwrap();
    let mut answer = foreign(&"x".repeat(20481));
    answer.spill_mcp_output(Some(&store), RunInvocationId::new(), ItemId::new());
    assert_eq!(
        answer.model_text(),
        format!(
            "{}\n[MCP output truncated: showing first 20480 bytes of 20481 bytes. Full output could not be saved.]",
            "x".repeat(20480)
        )
    );
    assert!(output_path(&answer).is_none());
}

// Catches: truncation slices inside a multibyte character or an incomplete line when a line boundary exists.
#[test]
fn mcp_output_spill_multibyte_and_line_boundaries_preserve_full_text() {
    let directory = TempDir::new().unwrap();
    let store =
        SessionEvidenceStore::open(directory.path(), SessionId::new(), 1024 * 1024).unwrap();
    for (text, preview) in [
        ("界".repeat(7000), "界".repeat(6826)),
        (
            format!("first line\n{}", "界".repeat(7000)),
            "first line\n".into(),
        ),
    ] {
        let mut answer = foreign(&text);
        answer.spill_mcp_output(Some(&store), RunInvocationId::new(), ItemId::new());
        assert!(answer.model_text().starts_with(&format!(
            "{preview}{}[MCP output truncated:",
            if preview.ends_with('\n') { "" } else { "\n" }
        )));
        assert_eq!(fs::read_to_string(spilled_path(&answer)).unwrap(), text);
    }
}

// Catches: masking only the preview leaks a secret split at the cutoff or through the readable file.
#[test]
fn mcp_output_spill_masks_secrets_before_preview_and_file_disclosure() {
    let directory = TempDir::new().unwrap();
    let store =
        SessionEvidenceStore::open(directory.path(), SessionId::new(), 1024 * 1024).unwrap();
    let mut answer = foreign(&format!("{}secret-token\nsecret-token", "x".repeat(20475)));
    answer.mask_secrets(&ego_core::secrets::SecretValues::new([
        b"secret-token".to_vec()
    ]));
    answer.spill_mcp_output(Some(&store), RunInvocationId::new(), ItemId::new());
    let full = fs::read_to_string(spilled_path(&answer)).unwrap();
    assert_eq!(full, format!("{}[REDACTED]\n[REDACTED]", "x".repeat(20475)));
    assert!(!answer.model_text().contains("secret"));
}
