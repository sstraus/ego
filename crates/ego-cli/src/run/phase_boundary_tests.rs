//! Behavioral contracts for the extracted driver phases.
use super::*;
use ego_core::control::{ControlError, ControlSelect, SessionControlState, SessionControls};
use ego_core::event::domain::DomainEvent;
use std::collections::VecDeque;

use super::test_support as support;
use support::{Plan, Reply, Stub, text, tool_call};

const CHILD: &str = "EGO_REGRESSION_PHASE_CHILD";

fn child_case(test: &str, case: &str, plan: Plan) -> Stub {
    let stub = Stub::start(plan);
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        "model = 'anthropic/claude-sonnet-5-5'\nthinking = 'low'\nbudget = '100k'\nsandbox = 'off'\n",
    )
    .unwrap();
    std::fs::write(
        home.path().join("models.toml"),
        support::vision_model_facts(if case == "unknown" {
            "image_input = 'unknown'"
        } else {
            support::READS_PICTURES
        }),
    )
    .unwrap();
    let skill = home.path().join("skills/background-check");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: background-check\ndescription: Check background settlement.\ncontext: fork\nbackground: true\n---\nReturn a short result.\n",
    )
    .unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .env_clear()
        .envs(support::coverage_environment())
        .envs(support::windows_system_environment())
        .env("PATH", support::fixture_path())
        // Match the existing real-driver child harness: its nested turn future
        // exceeds libtest's default stack when coverage is enabled.
        .env("RUST_MIN_STACK", "16777216")
        .env("TMPDIR", std::env::temp_dir())
        .env("HOME", home.path())
        .env("EGO_HOME", home.path())
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("ANTHROPIC_BASE_URL", format!("http://{}", stub.address))
        .env("OPENAI_BASE_URL", format!("http://{}", stub.address))
        .env("OLLAMA_HOST", &stub.address)
        .env(CHILD, case)
        .current_dir(workspace.path())
        .args([
            "--exact",
            &format!("run::phase_boundary_tests::{test}"),
            "--nocapture",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{case}: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    stub
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn args(model: &str) -> Args {
    let crate::cli::Invocation::Interactive(args) = crate::cli::parse(
        [
            "ego",
            "--model",
            model,
            "--thinking",
            "off",
            "--sandbox",
            "off",
            "--trust-workspace",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
    )
    .unwrap() else {
        panic!("interactive arguments")
    };
    *args
}

#[derive(Debug)]
struct ClipboardSeat {
    images: repl::ImageDraft,
    case: String,
    lines: Mutex<VecDeque<repl::Line>>,
    chips: Mutex<Vec<Vec<String>>>,
    notices: Mutex<Vec<String>>,
    inserted: Mutex<Vec<String>>,
}

impl Reads for ClipboardSeat {
    fn read(&self) -> repl::Line {
        self.lines
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(repl::Line::Ended)
    }
}

impl InteractiveSeat for ClipboardSeat {
    fn has_human(&self) -> bool {
        true
    }
    fn read_startup_model(
        &self,
        _: Arc<Mutex<ModelPicker>>,
        _: Option<String>,
    ) -> crate::model_picker::PickerOutcome {
        panic!("explicit model must not open picker")
    }
    fn trust(&self, _: &Path, _: &str) -> Trust {
        Trust::Trusted
    }
    fn permission(&self, _: &PermissionRequest) -> PermissionDecision {
        PermissionDecision::Deny
    }
    fn image_draft(&self) -> Option<repl::ImageDraft> {
        Some(self.images.clone())
    }
    fn set_image_chips(&self, chips: Vec<String>) {
        self.chips.lock().unwrap().push(chips);
    }
    fn set_notice(&self, notice: String) {
        self.notices.lock().unwrap().push(notice);
    }
    fn pasteboard_kind(&self) -> Result<PasteboardKind, String> {
        match self.case.as_str() {
            "text" => Ok(PasteboardKind::Text),
            "empty" => Ok(PasteboardKind::Empty),
            "inspect-error" => Err("fixture inspection failed".to_owned()),
            _ => Ok(PasteboardKind::Image),
        }
    }
    fn pasteboard_image(&self, max_bytes: u64) -> Result<Vec<u8>, String> {
        assert_eq!(
            max_bytes, 65536,
            "model limit must reach the clipboard port"
        );
        match self.case.as_str() {
            "read-error" => Err("fixture image read failed".to_owned()),
            "invalid" => Ok(Vec::new()),
            "unknown" => panic!("unknown capability must refuse before image acquisition"),
            _ => Ok(support::PNG_1X1.to_vec()),
        }
    }
    fn pasteboard_text(&self) -> Result<String, String> {
        Ok("clipboard text".to_owned())
    }
    fn insert_paste_text(&self, text: String) {
        self.inserted.lock().unwrap().push(text);
    }
}

// Catches: Ctrl+V bypasses model limits, loses staged images, or fetches bytes after refusal.
#[test]
fn clipboard_ingress_preserves_image_limits_and_refusals_before_submission() {
    let Ok(case) = std::env::var(CHILD) else {
        for case in [
            "image",
            "remove",
            "text",
            "empty",
            "inspect-error",
            "read-error",
            "invalid",
            "unknown",
        ] {
            let stub = child_case(
                "clipboard_ingress_preserves_image_limits_and_refusals_before_submission",
                case,
                Plan {
                    completions: vec![Reply::Sse(text("done"))],
                    ..Plan::default()
                },
            );
            let requests = stub.generations();
            assert_eq!(requests.len(), 1, "{case}: exactly one submitted turn");
            let user = requests[0]["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|message| message["role"] == "user")
                .unwrap();
            let images = user["content"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|block| block["type"] == "image")
                .count();
            assert_eq!(
                images,
                usize::from(case == "image"),
                "{case}: submitted images"
            );
        }
        return;
    };
    runtime().block_on(async {
        let mut lines =
            VecDeque::from([repl::Line::HostCommand(repl::HostCommand::ClipboardPaste)]);
        if case == "remove" {
            lines.push_back(repl::Line::HostCommand(repl::HostCommand::RemovePasteImage));
        }
        lines.push_back(repl::Line::Submitted("submit after paste".to_owned()));
        let seat = Arc::new(ClipboardSeat {
            images: repl::ImageDraft::default(),
            case: case.clone(),
            lines: Mutex::new(lines),
            chips: Mutex::new(Vec::new()),
            notices: Mutex::new(Vec::new()),
            inserted: Mutex::new(Vec::new()),
        });
        let ended = execute(
            args("anthropic/fixture-vision"),
            |_| &NullSink,
            CancelToken::new(),
            None,
            RunHost {
                driver: Driver::Interactive {
                    seat: seat.clone(),
                    between: &|| {},
                },
                elicitor: None,
                status: None,
                diagnostics: &Stderr,
                text_output: None,
                evidence: Arc::new(EvidenceAdmissionService::new()),
            },
        )
        .await
        .unwrap_or_else(|error| panic!("driver refused: {error:?}"));
        assert_eq!(ended.termination.exit_code(), 0);
        let chips = seat.chips.lock().unwrap();
        match case.as_str() {
            "image" => assert!(chips.iter().any(|chips| chips == &["clipboard 1"])),
            "remove" => {
                let staged = chips
                    .iter()
                    .position(|chips| chips == &["clipboard 1"])
                    .unwrap();
                assert!(chips[staged + 1..].iter().any(Vec::is_empty));
            }
            "text" => assert_eq!(*seat.inserted.lock().unwrap(), ["clipboard text"]),
            _ => {
                assert!(chips.iter().all(Vec::is_empty));
                let expected = match case.as_str() {
                    "empty" => "pasteboard has no image or text",
                    "inspect-error" => "fixture inspection failed",
                    "read-error" => "fixture image read failed",
                    "invalid" => "pasteboard image is invalid",
                    "unknown" => "image-input capability is unknown",
                    _ => unreachable!(),
                };
                assert!(
                    seat.notices
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|notice| notice.contains(expected))
                );
            }
        }
    });
}

#[derive(Debug)]
struct OperatorControls {
    case: String,
    session: SessionId,
}
impl SessionControls for OperatorControls {
    fn state(&self, session: SessionId) -> Result<SessionControlState, ControlError> {
        assert_eq!(
            session, self.session,
            "controls must be read for the submitted session"
        );
        if self.case == "unreadable" {
            return Err(ControlError::Internal("fixture unreadable seat".to_owned()));
        }
        let chosen = self.case == "chosen";
        Ok(SessionControlState {
            model: ControlSelect {
                current: Some(ModelRef("anthropic/fixture-vision".to_owned())),
                chosen: chosen.then(|| ModelRef("anthropic/fixture-vision".to_owned())),
                available: Vec::new(),
            },
            effort: ControlSelect {
                current: Some(ThinkingLevel::Off),
                chosen: chosen.then_some(ThinkingLevel::Off),
                available: Vec::new(),
            },
            mode: ControlSelect {
                current: Some(Mode::Plan),
                chosen: chosen.then_some(Mode::Plan),
                available: Vec::new(),
            },
            sandbox: ControlSelect {
                current: Some(Confinement::Off),
                chosen: None,
                available: Vec::new(),
            },
        })
    }
    fn stage(
        &self,
        _: SessionId,
        _: ego_core::control::SessionControlChange,
    ) -> Result<SessionControlState, ControlError> {
        panic!("submission reads controls; it does not stage them")
    }
}

struct Sinks;
impl ego_core::run::EventSinkFactory for Sinks {
    fn create(&self, _: &ModelCapabilities) -> Arc<dyn EventSink> {
        Arc::new(NullSink)
    }
}

// Catches: submission preflights the prepared model instead of the operator's next-generation choice.
#[test]
fn executor_submission_reads_only_explicit_session_controls_and_survives_an_unreadable_seat() {
    let Ok(case) = std::env::var(CHILD) else {
        for case in ["chosen", "inherited", "unreadable", "absent"] {
            let stub = child_case(
                "executor_submission_reads_only_explicit_session_controls_and_survives_an_unreadable_seat",
                case,
                Plan {
                    completions: vec![Reply::Sse(text("done"))],
                    ..Plan::default()
                },
            );
            let requests = stub.generations();
            assert_eq!(requests.len(), 1);
            assert_eq!(
                requests[0]["model"],
                if case == "chosen" {
                    "fixture-vision"
                } else {
                    "claude-sonnet-5-5"
                }
            );
        }
        return;
    };
    runtime().block_on(async {
        let workspace = std::env::current_dir().unwrap();
        let store = Arc::new(session::jsonl(
            workspace.join("sessions"),
            workspace.clone(),
        ));
        let executor = CliRunExecutor::for_acp(
            None,
            SessionShape::default(),
            store,
            Arc::new(EvidenceAdmissionService::new()),
        );
        let root = RunRoot::new(workspace).unwrap();
        let run_id = RunId::new(SessionId::new(), uuid::Uuid::now_v7());
        let prepared = executor
            .prepare(&RunPreparation {
                run_id,
                invocation: RunInvocationId::new(),
                root: root.clone(),
                selections: RunSelections::empty(),
                continuation: false,
                origin: ego_core::run::RunOrigin::TopLevel,
            })
            .await
            .unwrap();
        let (model, prepared) = prepared.into_parts();
        executor.admit_run_context(&prepared).await.unwrap();
        let handle = executor
            .attach(&RunAttachment {
                run_id,
                root,
                model,
                origin: ego_core::run::RunOrigin::TopLevel,
            })
            .await
            .unwrap();
        let mut bindings = RunExecutionBindings::new(Arc::new(Sinks), None);
        if case != "absent" {
            bindings = bindings.with_controls(Arc::new(OperatorControls {
                case,
                session: run_id.session_id(),
            }));
        }
        let result = executor
            .submit(
                &handle,
                &RunPrompt::from("use the next generation choice"),
                prepared,
                bindings,
            )
            .await
            .unwrap();
        assert!(matches!(result, RunSettlement::Completed), "{result:?}");
    });
}

// Catches: a headless turn returns before its background skill is collected into the parent journal.
#[test]
fn headless_completion_collects_background_skill_result_before_returning() {
    if std::env::var_os(CHILD).is_none() {
        let stub = child_case(
            "headless_completion_collects_background_skill_result_before_returning",
            "background",
            Plan {
                completions: vec![
                    Reply::Sse(tool_call(
                        "background-call",
                        "skill",
                        r#"{"kind":"invoke","name":"background-check","arguments":""}"#,
                    )),
                    Reply::Sse(text("background result")),
                    Reply::Sse(text("background result")),
                ],
                ..Plan::default()
            },
        );
        assert_eq!(
            stub.generations().len(),
            3,
            "parent and child must both execute"
        );
        return;
    }
    runtime().block_on(async {
        let mut args = args("anthropic/claude-sonnet-5-5");
        args.thinking = Some(ThinkingLevel::Low);
        args.prompt = Some("start the background check".to_owned());
        let ended = run_with_registry(
            args,
            |_| &NullSink,
            CancelToken::new(),
            Arc::new(EvidenceAdmissionService::new()),
            None,
        )
        .await;
        assert_eq!(ended.termination.exit_code(), 0, "{:?}", ended.termination);
        let root = std::env::current_dir().unwrap();
        let store = session::jsonl(session::store_root(None).unwrap(), root);
        let entries: Vec<_> = SessionStore::read(&store, &ended.session)
            .unwrap()
            .collect()
            .await;
        let terminals: Vec<_> = entries
            .into_iter()
            .filter_map(|entry| match entry.unwrap() {
                SessionEntry::Event(DomainEvent {
                    kind: EventKind::SubagentSettled { terminal, .. },
                    ..
                }) => Some(terminal),
                _ => None,
            })
            .collect();
        assert_eq!(
            terminals.len(),
            1,
            "background result must settle exactly once before return"
        );
        assert!(
            matches!(
                &terminals[0],
                ego_core::event::domain::SubagentTerminal::Returned { .. }
            ),
            "{terminals:?}"
        );
    });
}

// Catches: production wiring refusing every background task or accidentally retaining child delegation.
#[test]
fn headless_background_task_launch_uses_the_durable_child_owner() {
    if std::env::var_os(CHILD).is_none() {
        let stub = child_case(
            "headless_background_task_launch_uses_the_durable_child_owner",
            "background-task",
            Plan {
                completions: vec![
                    Reply::Sse(tool_call(
                        "task-call",
                        "task",
                        r#"{"name":"check","prompt":"Return a short result","background":true}"#,
                    )),
                    Reply::Sse(text("background result")),
                    Reply::Sse(text("background result")),
                ],
                ..Plan::default()
            },
        );
        let generations = stub.generations();
        assert_eq!(
            generations.len(),
            3,
            "parent and durable child must execute"
        );
        assert!(
            generations.iter().any(|request| !request["tools"]
                .as_array()
                .is_some_and(|tools| tools.iter().any(|tool| tool["name"] == "task"))),
            "the child cannot delegate"
        );
        return;
    }
    runtime().block_on(async {
        let mut args = args("anthropic/claude-sonnet-5-5");
        args.thinking = Some(ThinkingLevel::Low);
        args.prompt = Some("start the background check".to_owned());
        let ended = run_with_registry(args, |_| &NullSink, CancelToken::new(), Arc::new(EvidenceAdmissionService::new()), None).await;
        assert_eq!(ended.termination.exit_code(), 0, "{:?}", ended.termination);
        let store = session::jsonl(session::store_root(None).unwrap(), std::env::current_dir().unwrap());
        let entries: Vec<_> = SessionStore::read(&store, &ended.session).unwrap().collect().await;
        let events: Vec<_> = entries.into_iter().filter_map(|entry| match entry.unwrap() { SessionEntry::Event(event) => Some(event), _ => None }).collect();
        assert_eq!(events.iter().filter(|event| matches!(event.kind, EventKind::SubagentSpawned { .. })).count(), 1);
        assert_eq!(events.iter().filter(|event| matches!(event.kind, EventKind::SubagentCompletionDelivered { .. })).count(), 1);
        assert!(events.iter().any(|event| matches!(&event.kind, EventKind::ToolCallCompleted { ok: true, result } if result.model_text().contains("background agent"))));
    });
}

#[test]
fn background_task_cannot_reset_the_parent_token_allowance() {
    // Catches: a durable child starting with a fresh token ledger after the family hit its ceiling.
    if std::env::var_os(CHILD).is_none() {
        let response = tool_call(
            "task-call",
            "task",
            r#"{"name":"check","prompt":"Return a short result","background":true}"#,
        )
        .replace("\"prompt_tokens\":10", "\"prompt_tokens\":996")
        .replace("\"total_tokens\":14", "\"total_tokens\":1000");
        let stub = child_case(
            "background_task_cannot_reset_the_parent_token_allowance",
            "family-tokens",
            Plan {
                completions: vec![Reply::Sse(response), Reply::Sse(text("must not run"))],
                ..Plan::default()
            },
        );
        assert_eq!(
            stub.generations().len(),
            1,
            "the exhausted family's child must not submit a provider request"
        );
        return;
    }
    runtime().block_on(async {
        let mut args = args("anthropic/claude-sonnet-5-5");
        args.thinking = Some(ThinkingLevel::Low);
        args.budget = Some("1k".to_owned());
        args.prompt = Some("start a background check".to_owned());
        let ended = run_with_registry(
            args,
            |_| &NullSink,
            CancelToken::new(),
            Arc::new(EvidenceAdmissionService::new()),
            None,
        )
        .await;
        assert_eq!(ended.termination.exit_code(), 5, "{:?}", ended.termination);
    });
}

#[test]
fn background_task_budget_stop_is_delivered_as_failure_data() {
    // Catches: RunSettlement::Completed erasing a child's budget exit and returning a normal answer.
    if std::env::var_os(CHILD).is_none() {
        child_case(
            "background_task_budget_stop_is_delivered_as_failure_data",
            "child-budget",
            Plan {
                completions: vec![
                    Reply::Sse(tool_call(
                        "task-call",
                        "task",
                        r#"{"name":"check","prompt":"Return a short result","background":true}"#,
                    )),
                    Reply::Sse(
                        tool_call("read-child", "read", r#"{"path":"missing"}"#)
                            .replace("\"prompt_tokens\":10", "\"prompt_tokens\":1000")
                            .replace("\"total_tokens\":14", "\"total_tokens\":1004"),
                    ),
                    Reply::Sse(
                        tool_call("read-child", "read", r#"{"path":"missing"}"#)
                            .replace("\"prompt_tokens\":10", "\"prompt_tokens\":1000")
                            .replace("\"total_tokens\":14", "\"total_tokens\":1004"),
                    ),
                ],
                ..Plan::default()
            },
        );
        return;
    }
    runtime().block_on(async {
        let mut args = args("anthropic/claude-sonnet-5-5");
        args.thinking = Some(ThinkingLevel::Low);
        args.budget = Some("1k".to_owned());
        args.prompt = Some("start a background check".to_owned());
        let ended = run_with_registry(
            args,
            |_| &NullSink,
            CancelToken::new(),
            Arc::new(EvidenceAdmissionService::new()),
            None,
        )
        .await;
        let store = session::jsonl(
            session::store_root(None).unwrap(),
            std::env::current_dir().unwrap(),
        );
        let entries: Vec<_> = SessionStore::read(&store, &ended.session)
            .unwrap()
            .collect()
            .await;
        let delivered = entries
            .into_iter()
            .find_map(|entry| match entry.unwrap() {
                SessionEntry::Event(DomainEvent {
                    kind: EventKind::SubagentCompletionDelivered { text, .. },
                    ..
                }) => Some(text),
                _ => None,
            })
            .expect("child completion delivered");
        assert!(delivered.contains("Child ended: Budget"), "{delivered}");
    });
}

#[derive(Debug)]
struct IdleExecSeat {
    first: std::sync::atomic::AtomicBool,
    wake: crossbeam::channel::Receiver<repl::Line>,
    sender: crossbeam::channel::Sender<repl::Line>,
}

impl Reads for IdleExecSeat {
    fn read(&self) -> repl::Line {
        if self.first.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return repl::Line::Submitted("start background work".into());
        }
        self.wake
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap_or(repl::Line::Ended)
    }
}

impl InteractiveSeat for IdleExecSeat {
    fn wake_background_exec(&self) {
        // Catches: a duplicate/stale notification buys a second continuation.
        for _ in 0..2 {
            let _ = self.sender.try_send(repl::Line::HostCommand(
                repl::HostCommand::BackgroundExecSettled,
            ));
        }
    }
    fn has_human(&self) -> bool {
        false
    }
    fn read_startup_model(
        &self,
        _: Arc<Mutex<ModelPicker>>,
        _: Option<String>,
    ) -> crate::model_picker::PickerOutcome {
        panic!("explicit model must not open picker")
    }
    fn trust(&self, _: &Path, _: &str) -> Trust {
        Trust::Trusted
    }
    fn permission(&self, _: &PermissionRequest) -> PermissionDecision {
        PermissionDecision::Deny
    }
}

// Catches: process settlement stays invisible while the input owner waits for a user.
#[test]
#[cfg(unix)]
fn idle_background_exec_settlement_starts_one_continuation() {
    let Ok(case) = std::env::var(CHILD) else {
        for (case, command, timeout, expected) in [
            (
                "exec-success",
                "sleep 1.2; printf WAKE-RESULT",
                None,
                "WAKE-RESULT",
            ),
            (
                "exec-failure",
                "sleep 0.2; printf FAILED-RESULT; exit 7",
                None,
                "exited code 7",
            ),
            (
                "exec-timeout",
                "sleep 10",
                Some(100),
                "timed out after 100 ms",
            ),
        ] {
            let stub = child_case(
                "idle_background_exec_settlement_starts_one_continuation",
                case,
                Plan {
                    completions: vec![
                        Reply::Sse(tool_call(
                            "exec-launch",
                            "exec",
                            &serde_json::json!({"argv":["/bin/sh", "-c", command],
                                "background":true, "timeout_ms":timeout})
                            .to_string(),
                        )),
                        Reply::Sse(text("waiting for background work")),
                        Reply::Sse(text("background outcome received")),
                    ],
                    ..Plan::default()
                },
            );
            let requests = stub.generations();
            assert_eq!(
                requests.len(),
                3,
                "{case}: one automatic continuation, no extra input"
            );
            let continuation = requests[2].to_string();
            assert!(continuation.contains(expected), "{case}: {continuation}");
        }
        return;
    };
    runtime().block_on(async {
        let (sender, wake) = crossbeam::channel::bounded(2);
        let seat = Arc::new(IdleExecSeat {
            first: std::sync::atomic::AtomicBool::new(true),
            wake,
            sender,
        });
        let mut arguments = args("anthropic/fixture-vision");
        arguments.mode = Some(ego_core::config::Mode::Yolo);
        let ended = execute(
            arguments,
            |_| &NullSink,
            CancelToken::new(),
            None,
            RunHost {
                driver: Driver::Interactive {
                    seat,
                    between: &|| {},
                },
                elicitor: None,
                status: None,
                diagnostics: &Stderr,
                text_output: None,
                evidence: Arc::new(EvidenceAdmissionService::new()),
            },
        )
        .await
        .unwrap_or_else(|error| panic!("{case}: driver refused: {error:?}"));
        assert_eq!(ended.termination.exit_code(), 0);
        assert_eq!(ended.state.detached_execs.len(), 1);
        assert!(
            ended.state.detached_execs.values().all(Option::is_some),
            "{case}: the idle session never consumed its process outcome"
        );
        assert_eq!(
            ended.state.turns.len(),
            2,
            "one launch turn and one continuation"
        );
    });
}
