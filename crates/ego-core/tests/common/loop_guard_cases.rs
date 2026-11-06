use super::*;

struct ProgressingRead(Arc<AtomicUsize>);

impl TypedTool for ProgressingRead {
    type Params = ReadParams;
    fn name(&self) -> &str {
        "read"
    }
    fn description(&self) -> &str {
        "Read a changing status"
    }
    fn prepare_typed(
        &self,
        params: Self::Params,
        ctx: &PrepareCtx<'_>,
    ) -> Result<PreparedCall, ToolError> {
        FakeReadTool.prepare_typed(params, ctx)
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
            let result = FakeReadTool.execute(call, io).await?;
            let count = self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ToolResult::builtin(format!(
                "{}: {count}",
                result.model_text()
            )))
        })
    }
}

fn progressing_reads(scripts: Vec<Vec<AiEvent>>, count: Arc<AtomicUsize>) -> Fixture {
    let mut fx = Fixture::new(scripts);
    fx.registry = vec![Arc::new(Typed(ProgressingRead(count)))];
    for index in 0..16 {
        fx.fs
            .file(&format!("/ws/status-{index}"), &"status".repeat(1024));
    }
    fx
}

fn single_calls(arguments: impl IntoIterator<Item = String>, name: &str) -> Vec<Vec<AiEvent>> {
    arguments
        .into_iter()
        .enumerate()
        .map(|(index, args)| {
            vec![
                AiEvent::ToolCall {
                    call_id: format!("cycle-{index}"),
                    name: name.into(),
                    args,
                },
                AiEvent::Stop(StopReason::ToolUse),
            ]
        })
        .chain([vec![AiEvent::Stop(StopReason::Stop)]])
        .collect()
}

// Catches: alternating A/B calls evade a consecutive-only repetition detector.
#[test]
fn alternating_calls_warn_then_stop_before_the_next_write() {
    let args = ["A", "B"]
        .into_iter()
        .cycle()
        .take(15)
        .collect::<Vec<_>>()
        .into_iter()
        .map(|contents| json!({"path":"/ws/out.txt", "contents":contents}).to_string());
    let mut fx = Fixture::new(single_calls(args, "fake_write")).with_write_tool();
    fx.set_mode(Mode::Yolo);
    let (reason, _, _) = run(&fx, SessionState::default(), "write");
    assert_eq!(reason, ExitReason::Guard);
    assert_eq!(fx.fs.read(&CanonPath::new("/ws/out.txt")).unwrap(), b"B");
    assert_eq!(fx.generator.calls().len(), 15);
}

// Catches: the first repeat is stopped instead of permitting bounded polling and feedback.
#[test]
fn bounded_polling_can_finish_after_the_warning() {
    let args = vec![json!({"path":"/ws/status.txt"}).to_string(); 7];
    let fx = Fixture::new(single_calls(args, "read")).with_read_tool();
    fx.fs.file("/ws/status.txt", "pending");
    let (reason, ok, state) = run(&fx, SessionState::default(), "poll seven times");
    assert_eq!(reason, ExitReason::Completed);
    assert!(ok);
    assert_eq!(
        state
            .items
            .values()
            .filter(|item| item.kind == ItemKind::ToolCall && item.ok == Some(true))
            .count(),
        7
    );
    let requests = fx.generator.calls();
    assert!(requests[3].messages.messages().iter().flat_map(|message| &message.content).any(|block|
        matches!(block, ContentBlock::Text { text } if text.contains("Tool loop warning"))));
}

// Catches: ever-changing results reset the total turn budget as well as repeat counts.
#[test]
fn changing_result_polling_still_stops_at_turn_budget() {
    let scripts = (0..33)
        .map(|batch| {
            let mut calls: Vec<_> = (0..128)
                .map(|index| AiEvent::ToolCall {
                    call_id: format!("poll-{batch}-{index}"),
                    name: "read".into(),
                    args: json!({"path":format!("/ws/status-{}", index % 16)}).to_string(),
                })
                .collect();
            calls.push(AiEvent::Stop(StopReason::ToolUse));
            calls
        })
        .chain([vec![AiEvent::Stop(StopReason::Stop)]])
        .collect();
    let count = Arc::new(AtomicUsize::new(0));
    let fx = progressing_reads(scripts, count.clone());
    let (reason, ok, _) = run(&fx, SessionState::default(), "poll");
    assert_eq!(reason, ExitReason::Guard);
    assert!(!ok);
    assert_eq!(count.load(Ordering::SeqCst), 4096);
}
