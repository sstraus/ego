// `mcp_batch`: one model call carries several independent MCP lookups.
//
// Included into `turn` so it can use the turn fixtures. Offline only: the
// generator is scripted and the MCP tools are in-process fakes.

use super::*;
use ego_core::mcp::{McpAdmissionFingerprint, McpOperationKind, McpSubject};
use ego_core::permit::{
    ForeignToolId, ForeignTransportTarget, HttpRoute, HttpScheme, InvocationDigest, NetworkTarget,
};
use ego_core::turn::{AdmittedToolSource, CapturedToolCatalogue, ToolCatalogueError};

type Log = Arc<Mutex<Vec<&'static str>>>;

/// An MCP lookup tool: admitted as an MCP call, recorded when executed.
struct Lookup {
    name: &'static str,
    ran: Log,
    fail: bool,
}

impl ego_core::permit::Tool for Lookup {
    fn descriptor(&self) -> ego_core::permit::ToolDescriptor {
        ego_core::permit::ToolDescriptor {
            name: self.name.into(),
            description: "lookup".into(),
            schema: serde_json::from_value(json!({"type":"object"})).expect("schema"),
        }
    }
    fn prepare(
        &self,
        params: serde_json::Value,
        _: &PrepareCtx<'_>,
    ) -> Result<PreparedCall, ToolError> {
        let digest =
            InvocationDigest::parse(&format!("sha256:{:064x}", self.name.len())).expect("digest");
        let server = fixture_server(self.name);
        PreparedCall::mcp(
            McpSubject::Tool {
                server: server.into(),
                name: self.name.into(),
            },
            McpOperationKind::CallTool,
            vec![(
                ForeignToolId::new(server, self.name).expect("tool id"),
                digest,
            )],
            McpAdmissionFingerprint::new("capture-1", self.name, "schema-1").expect("admission"),
            "profile-1",
            ForeignTransportTarget::http(
                HttpRoute::new(
                    HttpScheme::Https,
                    NetworkTarget::new("mcp.example", 443).expect("target"),
                )
                .expect("route"),
            ),
            &json!({"name": self.name, "arguments": params}),
        )
        .map_err(|error| ToolError::InvalidParams(error.to_string()))
    }
    fn execute<'a, 'io>(
        &'a self,
        _: PreparedCall,
        _: &'a dyn GuardedIo<'io>,
    ) -> BoxFuture<'a, Result<ToolExecution, ToolError>>
    where
        'io: 'a,
    {
        self.ran.lock().unwrap().push(self.name);
        let outcome = if self.fail {
            Err(ToolError::InvalidParams("lookup backend down".into()))
        } else if self.name == "progress" {
            let recorded = include_str!("../fixtures/tuic-blocked-progress-2026-10-17.json");
            Ok(ToolExecution::Complete(
                serde_json::from_str(recorded).expect("recorded TUIC result"),
            ))
        } else {
            Ok(ToolExecution::Complete(ToolResult::builtin(format!(
                "answer from {}",
                self.name
            ))))
        };
        Box::pin(async move { outcome })
    }
}

struct Key;
impl ego_core::suspension::ResumeKeySource for Key {
    fn resume_key(&self) -> Result<Arc<[u8]>, ego_core::suspension::ContinuationError> {
        Ok(Arc::from(vec![7_u8; 32]))
    }
}

struct Catalogue(Vec<Arc<dyn ego_core::permit::Tool>>);
impl AdmittedToolSource for Catalogue {
    fn capture(&self) -> Result<CapturedToolCatalogue, ToolCatalogueError> {
        CapturedToolCatalogue::new(
            "capture-1",
            self.0.iter().cloned().map(RegisteredTool::Effect).collect(),
        )
    }
}

fn lookups(ran: &Log, failing: &[&str]) -> Vec<Arc<dyn ego_core::permit::Tool>> {
    ["alpha_a", "alpha_b", "alpha_c"]
        .into_iter()
        .map(|name| {
            Arc::new(Lookup {
                name,
                ran: Arc::clone(ran),
                fail: failing.contains(&name),
            }) as Arc<dyn ego_core::permit::Tool>
        })
        .collect()
}

fn fixture_server(name: &str) -> &str {
    if name == "progress" { "tuic" } else { "alpha" }
}

fn allowing(names: &[&str]) -> Arc<FixedPolicySource> {
    let tools: Vec<_> = names
        .iter()
        .map(|name| format!("\"tool({}/{name})\"", fixture_server(name)))
        .collect();
    let permissions = ConfigLayer::parse(
        Source::Project,
        &format!(
            "[permissions]\nallow=[{},\"net(mcp.example:443)\"]",
            tools.join(",")
        ),
    )
    .expect("rules")
    .value
    .permissions
    .expect("permissions");
    Arc::new(FixedPolicySource {
        mode: Mode::Default,
        permissions: Ruleset::from_layer(permissions, Source::Project),
        human: None,
    })
}

struct Outcome {
    fx: Fixture,
    exit: ExitReason,
    state: SessionState,
}

fn run_mcp(
    script: Vec<Vec<AiEvent>>,
    tools: Vec<Arc<dyn ego_core::permit::Tool>>,
    policy: Arc<FixedPolicySource>,
    batch: bool,
) -> Outcome {
    let mut fx = Fixture::new(script);
    fx.policy_source = policy;
    let (exit, state) = run_mcp_turn(&fx, tools, batch, None, config());
    Outcome { fx, exit, state }
}

fn run_mcp_turn(
    fx: &Fixture,
    tools: Vec<Arc<dyn ego_core::permit::Tool>>,
    batch: bool,
    task_port: Option<Arc<dyn ego_core::mcp::McpTaskPort>>,
    settings: TestGenerationSettings,
) -> (ExitReason, SessionState) {
    {
        let tools = Tools::from_registry_with_source(
            &[],
            Arc::new(Catalogue(tools)),
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        )
        .expect("distinct tools")
        .with_resume(Arc::new(
            ego_core::suspension::ResumeSealer::new(
                Arc::new(Key),
                Arc::new(FixedClock(1_787_000_000_000)),
                30_000,
            )
            .expect("sealer"),
        ))
        .with_mcp_batch(batch);
        let mut gate = fx.gate.borrow_mut();
        let mut runtime = model_with(
            &fx.generator,
            &mut gate,
            metering(),
            Some(rates()),
            None,
            settings,
        );
        runtime.policy_source = fx.policy_source.clone();
        let mut turn = TurnLoop::open(
            session(
                SessionState::replay(fx.store.events()),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            runtime,
            tools,
        );
        if let Some(port) = task_port {
            turn = turn.with_mcp_task_port(port);
        }
        let outcome =
            block_on(turn.run("look things up")).expect("the store accepted every append");
        (
            outcome.exit_reason,
            turn.into_state().expect("unique turn journal state"),
        )
    }
}

struct TaskStartingLookup {
    lookup: Lookup,
    poller: Arc<crate::mcp_task_ledger::Scripted>,
}

impl ego_core::permit::Tool for TaskStartingLookup {
    fn descriptor(&self) -> ego_core::permit::ToolDescriptor {
        self.lookup.descriptor()
    }

    fn prepare(
        &self,
        params: serde_json::Value,
        ctx: &PrepareCtx<'_>,
    ) -> Result<PreparedCall, ToolError> {
        self.lookup.prepare(params, ctx)
    }

    fn execute<'a, 'io>(
        &'a self,
        _: PreparedCall,
        _: &'a dyn GuardedIo<'io>,
    ) -> BoxFuture<'a, Result<ToolExecution, ToolError>>
    where
        'io: 'a,
    {
        use ego_core::mcp::{McpTaskPollerHandle, McpTaskStart};
        self.lookup.ran.lock().unwrap().push(self.lookup.name);
        Box::pin(async {
            Ok(ToolExecution::TaskStarted(McpTaskStart {
                task_id: "batch-task-b".into(),
                server: "alpha".into(),
                tool: "alpha_b".into(),
                poll_interval_ms: Some(0),
                ttl_ms: None,
                result: ToolResult::builtin("MCP task batch-task-b started"),
                poller: McpTaskPollerHandle(self.poller.clone()),
            }))
        })
    }
}

// Catches: the batch labels task acceptance as lookup completion, loses its
// tracking authority, or reissues the admitted member after journal replay.
#[test]
fn batch_reports_task_started_and_tracks_it_without_replaying_the_member() {
    use ego_core::mcp::McpTaskLedger;
    let all = ["alpha_a", "alpha_b", "alpha_c"];
    let ran = Log::default();
    let poller =
        crate::mcp_task_ledger::Scripted::completing(ToolResult::builtin("final task answer"));
    let mut members = lookups(&ran, &[]);
    members[1] = Arc::new(TaskStartingLookup {
        lookup: Lookup {
            name: "alpha_b",
            ran: ran.clone(),
            fail: false,
        },
        poller: poller.clone(),
    });
    let ledger = Arc::new(McpTaskLedger::new().with_min_interval(std::time::Duration::ZERO));
    let mut fx = Fixture::new(vec![batch_call("task-batch", &all), stop()]);
    fx.policy_source = allowing(&all);
    let (exit, state) = run_mcp_turn(&fx, members.clone(), true, Some(ledger.clone()), config());
    assert_eq!(exit, ExitReason::Completed);
    assert_eq!(*ran.lock().unwrap(), ["alpha_a", "alpha_b"]);
    let result = outer_text(&state, "task-batch");
    let answer: serde_json::Value = serde_json::from_str(&result).expect("structured batch result");
    assert_eq!(
        answer["results"][1]["output"],
        "MCP task batch-task-b started"
    );
    assert!(!result.contains("final task answer"));
    assert_eq!(
        poller.calls(),
        [(McpOperationKind::GetTask, json!({"taskId": "batch-task-b"}))]
    );
    assert_eq!(ledger.live(), 0);

    let mut reopened = Fixture::new(vec![stop()]);
    reopened.policy_source = allowing(&all);
    *reopened.store.events.lock().unwrap() = fx.store.events();
    let (exit, restored) = run_mcp_turn(&reopened, members, true, None, config());
    assert_eq!(exit, ExitReason::Completed);
    assert_eq!(
        *ran.lock().unwrap(),
        ["alpha_a", "alpha_b"],
        "reopen must not dispatch the task again"
    );
    assert_eq!(outer_text(&restored, "task-batch"), result);
    assert_eq!(
        answer["results"][1]["status"], "task_started",
        "task acceptance is not completed lookup success"
    );
    assert_eq!(answer["results"][0]["status"], "ok");
    assert_eq!(answer["results"][2]["status"], "not_run");
}

fn single(id: &str, name: &str) -> Vec<AiEvent> {
    vec![
        AiEvent::ToolCall {
            call_id: id.into(),
            name: name.into(),
            args: "{}".into(),
        },
        AiEvent::Stop(StopReason::ToolUse),
    ]
}

fn batch_call(id: &str, names: &[&str]) -> Vec<AiEvent> {
    let calls: Vec<_> = names
        .iter()
        .map(|name| json!({"tool": name, "arguments": {}}))
        .collect();
    vec![
        AiEvent::ToolCall {
            call_id: id.into(),
            name: "mcp_batch".into(),
            args: json!({ "calls": calls }).to_string(),
        },
        AiEvent::Stop(StopReason::ToolUse),
    ]
}

fn stop() -> Vec<AiEvent> {
    vec![AiEvent::Stop(StopReason::Stop)]
}

fn outer_item(state: &SessionState, call_id: &str) -> ego_core::state::ItemState {
    state
        .items
        .values()
        .find(|item| item.call_id.as_deref() == Some(call_id))
        .expect("the provider call is journaled")
        .clone()
}

fn outer_text(state: &SessionState, call_id: &str) -> String {
    tool_result_text(&outer_item(state, call_id))
}

// Catches: the batch costs an extra generation, runs members out of order, or
// answers differently from the serial path it replaces.
#[test]
fn three_lookups_take_two_generations_batched_and_four_serially() {
    let all = ["alpha_a", "alpha_b", "alpha_c"];
    let serial_ran = Log::default();
    let serial = run_mcp(
        vec![
            single("s1", "alpha_a"),
            single("s2", "alpha_b"),
            single("s3", "alpha_c"),
            stop(),
        ],
        lookups(&serial_ran, &[]),
        allowing(&all),
        false,
    );
    let batched_ran = Log::default();
    let batched = run_mcp(
        vec![batch_call("b1", &all), stop()],
        lookups(&batched_ran, &[]),
        allowing(&all),
        true,
    );
    assert_eq!(serial.exit, ExitReason::Completed);
    assert_eq!(batched.exit, ExitReason::Completed);
    assert_eq!(serial.fx.generator.calls().len(), 4);
    assert_eq!(batched.fx.generator.calls().len(), 2);
    assert_eq!(*serial_ran.lock().unwrap(), all);
    assert_eq!(*batched_ran.lock().unwrap(), all);
    let combined: serde_json::Value =
        serde_json::from_str(&outer_text(&batched.state, "b1")).expect("a JSON answer");
    let outputs: Vec<_> = combined["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            (
                entry["status"].as_str().unwrap(),
                entry["output"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        outputs,
        [
            ("ok", "answer from alpha_a"),
            ("ok", "answer from alpha_b"),
            ("ok", "answer from alpha_c")
        ]
    );
}

// Catches: a member reaching the wire as an unmatched provider call, or the
// outer call answered twice.
#[test]
fn the_next_request_carries_one_call_and_one_result() {
    let all = ["alpha_a", "alpha_b", "alpha_c"];
    let ran = Log::default();
    let out = run_mcp(
        vec![batch_call("b1", &all), stop()],
        lookups(&ran, &[]),
        allowing(&all),
        true,
    );
    let second = &out.fx.generator.calls()[1];
    let blocks: Vec<_> = second
        .messages
        .messages()
        .iter()
        .flat_map(|m| &m.content)
        .collect();
    assert_eq!(
        blocks
            .iter()
            .filter(|b| matches!(b, ContentBlock::ToolUse { .. }))
            .count(),
        1
    );
    assert_eq!(
        blocks
            .iter()
            .filter(|b| matches!(b, ContentBlock::ToolResult { call_id, .. } if call_id == "b1"))
            .count(),
        1
    );
    let (_, repairs) = conversation_from(&out.state).expect("sendable history");
    assert!(repairs.is_empty(), "{repairs:?}");
    let members: Vec<_> = out
        .state
        .items
        .values()
        .filter(|item| item.batch_member.is_some())
        .collect();
    assert_eq!(members.len(), 3);
    assert!(
        members
            .iter()
            .all(|item| item.completed && item.call_id.is_none())
    );
}

// Catches: a bad member leaving earlier members run and the model unanswered,
// or a non-MCP/unknown/nested member being dispatched.
#[test]
fn an_invalid_batch_runs_nothing() {
    let all = ["alpha_a", "alpha_b", "alpha_c"];
    let cases: Vec<(&str, Vec<&str>)> = vec![
        ("unknown member", vec!["alpha_a", "no_such_tool"]),
        ("nested batch", vec!["alpha_a", "mcp_batch"]),
        ("single member", vec!["alpha_a"]),
        ("too many members", vec!["alpha_a"; 9]),
    ];
    for (label, names) in cases {
        let ran = Log::default();
        let out = run_mcp(
            vec![batch_call("b1", &names), stop()],
            lookups(&ran, &[]),
            allowing(&all),
            true,
        );
        assert!(ran.lock().unwrap().is_empty(), "{label}");
        let item = outer_item(&out.state, "b1");
        assert_eq!(item.ok, Some(false), "{label}");
        assert!(tool_result_text(&item).contains("Nothing ran"), "{label}");
        assert!(
            out.state.items.values().all(|i| i.batch_member.is_none()),
            "{label}"
        );
    }
}

// Catches: a permission stop hiding earlier effects or dispatching the suffix.
#[test]
fn a_member_without_permission_stops_the_batch_after_its_predecessors() {
    let ran = Log::default();
    let out = run_mcp(
        vec![batch_call("b1", &["alpha_a", "alpha_b", "alpha_c"]), stop()],
        lookups(&ran, &[]),
        allowing(&["alpha_a", "alpha_c"]),
        true,
    );
    assert_eq!(out.exit, ExitReason::PermissionRequired);
    assert_eq!(*ran.lock().unwrap(), ["alpha_a"]);
    let members = out
        .state
        .items
        .values()
        .filter(|item| item.batch_member.is_some())
        .count();
    assert_eq!(members, 2, "the third member is never started");
    let answer: serde_json::Value = serde_json::from_str(&outer_text(&out.state, "b1"))
        .expect("combined response on permission stop");
    assert_eq!(answer["results"][0]["status"], "ok");
    assert_eq!(answer["results"][0]["output"], "answer from alpha_a");
    assert_eq!(answer["results"][1]["status"], "error");
    assert!(
        answer["results"][1]["output"]
            .as_str()
            .unwrap()
            .contains("alpha_b")
    );
    assert_eq!(answer["results"][2]["status"], "not_run");
}

#[test]
fn batch_blocked_progress_preserves_prefix_and_returns_control_without_sending_again() {
    let ran = Log::default();
    let mut members = lookups(&ran, &[]);
    members[1] = Arc::new(Lookup {
        name: "progress",
        ran: ran.clone(),
        fail: false,
    });
    let args = json!({"calls": [
        {"tool": "alpha_a", "arguments": {}},
        {"tool": "progress", "arguments": {"type":"blocked", "text":"May I continue?"}},
        {"tool": "alpha_c", "arguments": {}}
    ]});
    let out = run_mcp(
        vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "blocked-batch".into(),
                    name: "mcp_batch".into(),
                    args: args.to_string(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            stop(),
        ],
        members,
        allowing(&["alpha_a", "progress", "alpha_c"]),
        true,
    );
    assert_eq!(out.exit, ExitReason::Completed);
    assert_eq!(
        out.fx.generator.calls().len(),
        1,
        "blocked progress hands control back"
    );
    assert_eq!(*ran.lock().unwrap(), ["alpha_a", "progress"]);
    let answer: serde_json::Value =
        serde_json::from_str(&outer_text(&out.state, "blocked-batch")).unwrap();
    assert_eq!(answer["results"][0]["output"], "answer from alpha_a");
    assert_eq!(answer["results"][1]["status"], "ok");
    assert_eq!(answer["results"][2]["status"], "not_run");
}

// Catches: a failed member not stopping the rest, or its error being dropped.
#[test]
fn a_failing_member_stops_the_rest_and_reports_not_run() {
    let all = ["alpha_a", "alpha_b", "alpha_c"];
    let ran = Log::default();
    let out = run_mcp(
        vec![batch_call("b1", &all), stop()],
        lookups(&ran, &["alpha_b"]),
        allowing(&all),
        true,
    );
    assert_eq!(*ran.lock().unwrap(), ["alpha_a", "alpha_b"]);
    let item = outer_item(&out.state, "b1");
    assert_eq!(item.ok, Some(false));
    let combined: serde_json::Value = serde_json::from_str(&tool_result_text(&item)).unwrap();
    let statuses: Vec<_> = combined["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["status"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(statuses, ["ok", "error", "not_run"]);
}

// Catches: a crash mid-batch replaying a started member, hiding an unfinished
// effect from the model, or inventing a member that never started.
#[test]
fn every_crash_cut_keeps_unstarted_unstarted_and_never_replays() {
    let all = ["alpha_a", "alpha_b", "alpha_c"];
    let ran = Log::default();
    let out = run_mcp(
        vec![batch_call("b1", &all), stop()],
        lookups(&ran, &[]),
        allowing(&all),
        true,
    );
    let events = out.fx.store.events();
    let started: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(e.kind, EventKind::BatchMemberStarted { .. }))
        .map(|(index, _)| index)
        .collect();
    assert_eq!(started.len(), 3);
    for (position, cut) in started.iter().copied().enumerate() {
        // Cut right after the member's start: it began, its outcome is unknown.
        let torn = SessionState::replay(events[..=cut].to_vec());
        let begun = torn
            .items
            .values()
            .filter(|item| item.batch_member.is_some())
            .count();
        assert_eq!(begun, position + 1, "later members never started");
        let unfinished = torn.torn_tool_errors();
        assert!(
            unfinished.len() >= 2,
            "the unfinished member and the outer call are torn: {unfinished:?}"
        );
        let (history, repairs) = conversation_from(&torn).expect("sendable history");
        assert!(
            repairs
                .iter()
                .any(|repair| format!("{repair:?}").contains("b1")),
            "the outer call is answered as torn, never replayed: {repairs:?}"
        );
        let uses = history
            .messages()
            .iter()
            .flat_map(|m| &m.content)
            .filter(|b| matches!(b, ContentBlock::ToolUse { .. }))
            .count();
        assert_eq!(uses, 1, "no member is ever a provider call");
    }
    assert_eq!(
        ran.lock().unwrap().len(),
        3,
        "the full run executed each member once"
    );
}

// Catches: the batch tool being exposed without the owner's opt-in.
#[test]
fn the_batch_is_offered_only_on_the_owners_opt_in() {
    let all = ["alpha_a", "alpha_b", "alpha_c"];
    for (opt_in, expected) in [(true, true), (false, false)] {
        let ran = Log::default();
        let out = run_mcp(vec![stop()], lookups(&ran, &[]), allowing(&all), opt_in);
        let offered = out.fx.generator.calls()[0]
            .tools
            .iter()
            .any(|tool| tool.name == "mcp_batch");
        assert_eq!(offered, expected, "opt_in={opt_in}");
    }
}

// Catches: a provider switch advertising the batch on a parallel route or
// stripping the original call/result pair from the next request's history.
#[test]
fn switching_to_a_parallel_route_keeps_batch_history_without_offering_batch() {
    let names = ["alpha_a", "alpha_b", "alpha_c"];
    let ran = Log::default();
    let first = run_mcp(
        vec![batch_call("prior-batch", &names), stop()],
        lookups(&ran, &[]),
        allowing(&names),
        true,
    );
    assert_eq!(first.exit, ExitReason::Completed);
    let mut second = Fixture::new(vec![stop()]);
    second.policy_source = allowing(&names);
    *second.store.events.lock().unwrap() = first.fx.store.events();
    let mut settings = config();
    settings.model = "other-provider/parallel".to_owned();
    settings.parallel_tool_calls = true;
    let (exit, _) = run_mcp_turn(&second, lookups(&ran, &[]), true, None, settings);
    assert_eq!(exit, ExitReason::Completed);
    assert_eq!(*ran.lock().unwrap(), names);
    let requests = second.generator.calls();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert!(request.tools.iter().any(|tool| tool.name == "alpha_a"));
    assert!(request.tools.iter().all(|tool| tool.name != "mcp_batch"));
    let blocks: Vec<_> = request
        .messages
        .messages()
        .iter()
        .flat_map(|m| &m.content)
        .collect();
    assert_eq!(
        blocks
            .iter()
            .filter(|block| matches!(block,
                ContentBlock::ToolUse { call_id, .. } if call_id == "prior-batch"
            ))
            .count(),
        1
    );
    assert_eq!(
        blocks
            .iter()
            .filter(|block| matches!(block,
                ContentBlock::ToolResult { call_id, .. } if call_id == "prior-batch"
            ))
            .count(),
        1
    );
}

// Catches: batches bypassing the repeated-call guard and dispatching members
// again even though identical requests keep returning the same result.
#[test]
fn repeated_batches_stop_before_the_eighth_dispatch() {
    let names = ["alpha_a", "alpha_b"];
    let ran = Log::default();
    let out = run_mcp(
        (0..10)
            .map(|i| batch_call(&format!("batch-{i}"), &names))
            .collect(),
        lookups(&ran, &[]),
        allowing(&names),
        true,
    );
    assert_eq!(out.exit, ExitReason::Guard);
    assert_eq!(out.fx.generator.calls().len(), 8);
    assert_eq!(*ran.lock().unwrap(), names.repeat(7));
}

struct SuspendingLookup(Lookup);

impl ego_core::permit::Tool for SuspendingLookup {
    fn descriptor(&self) -> ego_core::permit::ToolDescriptor {
        self.0.descriptor()
    }

    fn prepare(
        &self,
        params: serde_json::Value,
        ctx: &PrepareCtx<'_>,
    ) -> Result<PreparedCall, ToolError> {
        self.0.prepare(params, ctx)
    }

    fn execute<'a, 'io>(
        &'a self,
        _: PreparedCall,
        _: &'a dyn GuardedIo<'io>,
    ) -> BoxFuture<'a, Result<ToolExecution, ToolError>>
    where
        'io: 'a,
    {
        self.0.ran.lock().unwrap().push(self.0.name);
        Box::pin(async {
            Ok(ToolExecution::InputRequired(
                ego_core::mcp::ForeignInputRequired::new(
                    "mcp-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                    ego_core::question::QuestionRequest::new(
                        "Confirm lookup",
                        ego_core::question::QuestionMode::Round(ego_core::question::ElicitationRound {
                            expires_at_ms: None,
                            requests: [(
                                "confirm".to_owned(),
                                ego_core::question::ElicitationMember {
                                    prompt: "Confirm lookup".to_owned(),
                                    kind: ego_core::question::ElicitationKind::Form {
                                        schema: json!({"type":"object","properties":{"choice":{"type":"string"}},"required":["choice"]}),
                                    },
                                },
                            )].into(),
                            completed: Default::default(),
                        }),
                        vec![],
                        false,
                    ),
                )
                .expect("valid continuation"),
            ))
        })
    }
}

enum SuspensionPhase {
    Start,
    Answer(ItemId),
    Recover(TurnId),
    Recap,
    Interactive,
}

type ElicitationGate = (
    futures::channel::oneshot::Sender<()>,
    futures::channel::oneshot::Receiver<()>,
);

struct WaitingElicitor(Mutex<Option<ElicitationGate>>);

impl ego_core::question::Elicitor for WaitingElicitor {
    fn elicit<'a>(
        &'a self,
        _: ItemId,
        _: ego_core::question::QuestionRequest,
    ) -> BoxFuture<'a, Result<ego_core::question::QuestionOutcome, ego_core::question::ElicitorError>>
    {
        let (opened, answer) = self.0.lock().unwrap().take().expect("one elicitation");
        Box::pin(async move {
            opened.send(()).expect("question observer");
            answer.await.expect("human answer");
            Ok(ego_core::question::QuestionOutcome::Answered(
                suspension_answer(),
            ))
        })
    }
}

fn suspension_answer() -> ego_core::question::QuestionAnswer {
    ego_core::question::QuestionAnswer::Round(ego_core::question::ElicitationAnswers {
        confirmed: true,
        responses: [("confirm".to_owned(), json!({"choice":"yes"}))].into(),
    })
}

fn suspension_phase(
    fx: &Fixture,
    id: SessionId,
    state: SessionState,
    members: &[Arc<dyn ego_core::permit::Tool>],
    foreign: Arc<crate::question_runtime::RecordingForeignQuestionPort>,
    phase: SuspensionPhase,
) -> (ExitReason, SessionState) {
    let tools = Tools::from_registry_with_source(
        &[],
        Arc::new(Catalogue(members.to_vec())),
        ToolEffectPorts::new(
            &fx.fs,
            fx.process.as_ref(),
            &fx.evidence,
            Arc::new(NoResolver),
            Arc::new(NoHop),
            Arc::new(NoDuplex),
        ),
        CanonPath::new("/ws"),
    )
    .expect("MCP catalogue")
    .with_resume(Arc::new(
        ego_core::suspension::ResumeSealer::new(
            Arc::new(Key),
            Arc::new(FixedClock(1_787_000_000_000)),
            30_000,
        )
        .expect("sealer"),
    ))
    .with_mcp_batch(true);
    let mut gate = fx.gate.borrow_mut();
    let mut runtime = model_with(
        &fx.generator,
        &mut gate,
        metering(),
        Some(rates()),
        None,
        config(),
    );
    runtime.policy_source = fx.policy_source.clone();
    let mut restored = session(state, &fx.store, &fx.clock, &fx.sink, CancelToken::new());
    restored.id = id;
    let turn = TurnLoop::open(restored, runtime, tools);
    let mut turn = crate::question_runtime::bind_foreign_fixture(turn, foreign)
        .with_hook_policy(fx.policy_source.current().expect("captured fixture policy"))
        .expect("restore foreign continuation binding before resume");
    let (ingress, mut control) = ego_core::turn::interactive_control();
    block_on(async {
        match phase {
            SuspensionPhase::Start => turn.run("look things up").await,
            SuspensionPhase::Answer(question) => {
                let sent = fx.generator.calls().len();
                let error = turn
                    .recap(&CancelToken::new())
                    .await
                    .expect_err("recap waits for the pending batch question");
                assert!(error.to_string().contains("pending MCP batch question"));
                assert_eq!(
                    fx.generator.calls().len(),
                    sent,
                    "recap must not reach the provider while the question is open"
                );
                turn.resume_question(question, suspension_answer()).await
            }
            SuspensionPhase::Recover(turn_id) => turn.resume_turn(turn_id).await,
            SuspensionPhase::Recap => {
                let recap = turn
                    .recap(&CancelToken::new())
                    .await
                    .expect("recap uses the recovered batch result");
                assert_eq!(recap.text(), "lookup recap");
                return Ok((
                    ExitReason::Completed,
                    turn.into_state().expect("closed recap turn"),
                ));
            }
            SuspensionPhase::Interactive => {
                let (opened, waiting) = futures::channel::oneshot::channel();
                let (answer, release) = futures::channel::oneshot::channel();
                turn = turn
                    .with_interactive_control(&mut control)
                    .with_elicitor(Arc::new(WaitingElicitor(Mutex::new(Some((
                        opened, release,
                    ))))));
                let user = async {
                    waiting.await.expect("batch is suspended");
                    assert_eq!(fx.generator.calls().len(), 1, "no generation while waiting");
                    let status = ingress
                        .submit("Handle my queued prompt".to_owned(), vec![])
                        .await
                        .expect("interactive input reaches the turn while elicitation is open");
                    assert_eq!(status.queued + status.pending_steers, 1);
                    answer.send(()).expect("elicitor receives the answer");
                };
                let (outcome, ()) = futures::join!(turn.run("look things up"), user);
                outcome
            }
        }
        .map(|outcome| (outcome.exit_reason, turn.into_state().expect("closed turn")))
    })
    .expect("durable suspension phase")
}

// Catches: reopening tears a live batch, replays its suspended dispatch, starts
// the third member, or loses/duplicates the outer answer after member settlement.
#[test]
fn suspended_second_member_reopens_and_completes_outer_once_at_every_settlement_cut() {
    let all = ["alpha_a", "alpha_b", "alpha_c"];
    let ran = Log::default();
    let mut members = lookups(&ran, &[]);
    members[1] = Arc::new(SuspendingLookup(Lookup {
        name: "alpha_b",
        ran: Arc::clone(&ran),
        fail: false,
    }));
    let foreign = Arc::new(crate::question_runtime::RecordingForeignQuestionPort::default());
    let id = SessionId::new();
    let mut fx = Fixture::new(vec![batch_call("b1", &all), stop()]);
    fx.policy_source = allowing(&all);
    let (exit, _) = suspension_phase(
        &fx,
        id,
        SessionState::default(),
        &members,
        foreign.clone(),
        SuspensionPhase::Start,
    );
    assert_eq!(exit, ExitReason::PermissionRequired);
    assert_eq!(*ran.lock().unwrap(), ["alpha_a", "alpha_b"]);
    assert!(foreign.calls().is_empty());
    let state = SessionState::replay(fx.store.events());
    let pending = state.pending_question().expect("durable foreign question");
    let question = pending.item_id();
    let turn_id = pending.turn();
    let outer = state
        .items
        .iter()
        .find(|(_, item)| item.call_id.as_deref() == Some("b1"))
        .map(|(id, _)| *id)
        .unwrap();
    assert!(!state.items[&outer].completed);
    assert!(
        state.torn_tool_errors().is_empty(),
        "a live suspension owns both calls"
    );
    assert_eq!(
        state
            .items
            .values()
            .filter(|item| item.batch_member == Some(outer))
            .count(),
        2
    );
    assert_eq!(
        fx.generator.calls().len(),
        1,
        "no generation while input is pending"
    );

    // A fresh TurnLoop receives only journal replay, not the previous in-memory state.
    let (exit, _) = suspension_phase(
        &fx,
        id,
        state,
        &members,
        foreign.clone(),
        SuspensionPhase::Answer(question),
    );
    assert_eq!(exit, ExitReason::Completed);
    assert_eq!(foreign.calls().len(), 1);
    let events = fx.store.events();
    let outer_completion = events
        .iter()
        .position(|event| {
            event.env.item == Some(outer)
                && matches!(event.kind, EventKind::ToolCallCompleted { .. })
        })
        .expect("outer completion");
    let member_completion = events[..outer_completion]
        .iter()
        .rposition(|event| matches!(event.kind, EventKind::ToolCallCompleted { .. }))
        .expect("suspended member completion precedes the outer");
    assert_eq!(fx.generator.calls().len(), 2);

    let answer_settlement = events[..member_completion]
        .iter()
        .rposition(|event| matches!(event.kind, EventKind::QuestionSettled { .. }))
        .expect("the recorded answer precedes the member result");
    let prefix = events[..=answer_settlement].to_vec();
    let state = SessionState::replay(prefix.clone());
    // The original answer is durable, but the effect port has yet to claim it.
    // This is a fresh port with no previous dispatch; the durable store is shared.
    let unclaimed_port = Arc::new(crate::question_runtime::RecordingForeignQuestionPort::default());
    let mut reopened = Fixture::new(vec![stop()]);
    reopened.policy_source = allowing(&all);
    *reopened.store.events.lock().unwrap() = prefix;
    let (exit, recovered) = suspension_phase(
        &reopened,
        id,
        state,
        &members,
        unclaimed_port.clone(),
        SuspensionPhase::Answer(question),
    );
    assert_eq!(exit, ExitReason::Completed);
    assert_eq!(
        unclaimed_port.calls(),
        foreign.calls(),
        "recovery uses the original answer"
    );
    assert_eq!(*ran.lock().unwrap(), ["alpha_a", "alpha_b"]);
    assert_eq!(
        outer_text(&recovered, "b1"),
        outer_text(&SessionState::replay(events.clone()), "b1")
    );
    assert_eq!(reopened.generator.calls().len(), 1);

    // Real journal prefixes: after member completion, after outer completion,
    // and after the original resumed turn closed. No synthetic lifecycle events.
    for (cut, phase, reply) in [
        (member_completion + 1, SuspensionPhase::Start, stop()),
        (
            member_completion + 1,
            SuspensionPhase::Recap,
            vec![
                AiEvent::TextDelta {
                    text: "lookup recap".into(),
                },
                AiEvent::Stop(StopReason::Stop),
            ],
        ),
        (
            outer_completion + 1,
            SuspensionPhase::Recover(turn_id),
            stop(),
        ),
        (events.len(), SuspensionPhase::Recover(turn_id), stop()),
    ] {
        let prefix = events[..cut].to_vec();
        let state = SessionState::replay(prefix.clone());
        let mut reopened = Fixture::new(vec![reply]);
        reopened.policy_source = allowing(&all);
        *reopened.store.events.lock().unwrap() = prefix;
        let (exit, state) =
            suspension_phase(&reopened, id, state, &members, foreign.clone(), phase);
        assert_eq!(exit, ExitReason::Completed, "cut={cut}");
        assert!(state.pending_question().is_none(), "cut={cut}");
        assert_eq!(*ran.lock().unwrap(), ["alpha_a", "alpha_b"], "cut={cut}");
        assert_eq!(
            foreign.calls().len(),
            1,
            "continuation replayed at cut={cut}"
        );
        assert_eq!(
            reopened
                .store
                .events()
                .iter()
                .filter(|event| {
                    event.env.item == Some(outer)
                        && matches!(event.kind, EventKind::ToolCallCompleted { .. })
                })
                .count(),
            1,
            "outer completion count at cut={cut}"
        );
        let answer: serde_json::Value = serde_json::from_str(&outer_text(&state, "b1")).unwrap();
        let statuses: Vec<_> = answer["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["status"].as_str().unwrap())
            .collect();
        assert_eq!(statuses, ["ok", "ok", "not_run"], "cut={cut}");
        assert_eq!(answer["results"][0]["output"], "answer from alpha_a");
        assert_eq!(answer["results"][1]["output"], "foreign-complete");
        let requests = reopened.generator.calls();
        let blocks: Vec<_> = requests[0]
            .messages
            .messages()
            .iter()
            .flat_map(|m| &m.content)
            .collect();
        assert_eq!(
            blocks
                .iter()
                .filter(|block| matches!(block, ContentBlock::ToolUse { .. }))
                .count(),
            1
        );
        assert_eq!(
            blocks
                .iter()
                .filter(|block| matches!(block,
                    ContentBlock::ToolResult { call_id, .. } if call_id == "b1"
                ))
                .count(),
            1
        );
        let (_, repairs) = conversation_from(&state).expect("settled history");
        assert!(repairs.is_empty(), "cut={cut}: {repairs:?}");
    }
}

// Catches: a queued prompt replacing the pending elicitation or reaching the
// model before the original batch has its correlated answer.
#[test]
fn interactive_batch_answers_before_delivering_a_prompt_queued_during_elicitation() {
    let all = ["alpha_a", "alpha_b", "alpha_c"];
    let ran = Log::default();
    let mut members = lookups(&ran, &[]);
    members[1] = Arc::new(SuspendingLookup(Lookup {
        name: "alpha_b",
        ran: Arc::clone(&ran),
        fail: false,
    }));
    let foreign = Arc::new(crate::question_runtime::RecordingForeignQuestionPort::default());
    let mut fx = Fixture::new(vec![batch_call("b1", &all), stop(), stop()]);
    fx.policy_source = allowing(&all);
    let (exit, state) = suspension_phase(
        &fx,
        SessionId::new(),
        SessionState::default(),
        &members,
        foreign.clone(),
        SuspensionPhase::Interactive,
    );
    assert_eq!(exit, ExitReason::Completed);
    assert_eq!(*ran.lock().unwrap(), ["alpha_a", "alpha_b"]);
    assert_eq!(foreign.calls().len(), 1);
    let events = fx.store.events();
    let batch = events
        .iter()
        .find(|event| {
            event.env.item.is_some_and(|id| {
                state
                    .items
                    .get(&id)
                    .is_some_and(|item| item.call_id.as_deref() == Some("b1"))
            }) && matches!(event.kind, EventKind::ToolCallCompleted { .. })
        })
        .expect("durable batch answer");
    let queued = events
        .iter()
        .find(|event| {
            matches!(
                &event.kind, EventKind::UserPrompt { text, queued_prompt: Some(_), .. }
                    if text == "Handle my queued prompt"
            )
        })
        .expect("queued prompt preserved for the model");
    assert!(batch.env.seq < queued.env.seq);
}
