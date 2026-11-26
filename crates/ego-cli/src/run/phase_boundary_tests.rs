//! Behavioral contracts for the extracted driver phases.
use super::*;
use ego_core::control::{ControlError, ControlSelect, SessionControlState, SessionControls};
use ego_core::event::domain::DomainEvent;
use std::collections::VecDeque;

use super::test_support as support;
use support::{Plan, Reply, Stub, text, tool_call};

const CHILD: &str = "EGO_450_PHASE_CHILD";

fn child_case(test: &str, case: &str, plan: Plan) -> Stub {
    let stub = Stub::start(plan);
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let filter_config = if case == "task-idle-filter" {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tools/protocol-filter/dlp.py")
            .canonicalize()
            .unwrap();
        format!(
            "[auth.anthropic.protocol_filter]\ncommand = '{}'\n",
            path.display()
        )
    } else {
        String::new()
    };
    std::fs::write(
        home.path().join("config.toml"),
        format!("model = 'anthropic/claude-sonnet-5-5'\nthinking = 'low'\nbudget = '100k'\nsandbox = 'off'\n{filter_config}"),
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
    if case.starts_with("pressure-native-") {
        let config = home.path().join("config.toml");
        let text = std::fs::read_to_string(&config)
            .unwrap()
            .replace("budget = '100k'", "budget = '1000k'");
        std::fs::write(config, text).unwrap();
    }
    if case == "codex-phase-cache" {
        std::fs::write(home.path().join("config.toml"), "sandbox = 'off'\nconnection = 'fixture'\n[connections.fixture]\nname = 'Fixture'\nvendor = 'openai'\naccess = 'oauth'\n").unwrap();
        std::fs::write(home.path().join("auth.json"), serde_json::json!({
            "connection:fixture": {"type":"o_auth", "access":
                "head.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdF8xIn19.sig"}
        }).to_string()).unwrap();
    }
    if case.starts_with("native-task-account-") {
        let mut config = String::from(
            r#"
sandbox = "off"
budget = "100k"
connection = "source"
[task]
model = "openai/gpt-6-luna"
[connections.source]
name = "Source account"
vendor = "anthropic"
access = "key"
key_env = "ANTHROPIC_API_KEY"
"#,
        );
        if case != "native-task-account-missing" {
            config.push_str(
                r#"
[connections.target]
name = "Target account"
vendor = "openai"
access = "key"
key_env = "OPENAI_API_KEY"
"#,
            );
        }
        if case == "native-task-account-ambiguous" {
            config.push_str(
                r#"
[connections.other]
name = "Other account"
vendor = "openai"
access = "key"
key_env = "OTHER_OPENAI_API_KEY"
"#,
            );
        }
        std::fs::write(home.path().join("config.toml"), config).unwrap();
    }
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
        .env("OPENAI_API_KEY", "test-target-key")
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
                    resume_after_root: None,
                    root_transition: RootTransitionSlot::default(),
                    seed_opening_prompt: true,
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
            }
            .clear_skill_reviews(),
        );
        assert_eq!(
            stub.conversation_generations().len(),
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
            "the first parent call, durable child and parent result must execute"
        );
        let root_role = "You are coordinating delegated work for this session.";
        let worker_role = "You are an ego child agent.";
        assert!(generations[0].to_string().contains(root_role));
        let child = generations
            .iter()
            .find(|request| {
                !request["tools"]
                    .as_array()
                    .is_some_and(|tools| tools.iter().any(|tool| tool["name"] == "task"))
            })
            .expect("the child cannot delegate");
        assert!(
            child.to_string().contains(worker_role),
            "worker role reaches the first child request"
        );
        assert!(
            !child.to_string().contains(root_role),
            "root role cannot leak into child"
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
                        tool_call("read-child", "read", r#"{"files":[{"path":"missing"}]}"#)
                            .replace("\"prompt_tokens\":10", "\"prompt_tokens\":1000")
                            .replace("\"total_tokens\":14", "\"total_tokens\":1004"),
                    ),
                    Reply::Sse(
                        tool_call("read-child", "read", r#"{"files":[{"path":"missing"}]}"#)
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
#[cfg(unix)]
struct IdleExecSeat {
    first: std::sync::atomic::AtomicBool,
    first_prompt: &'static str,
    wake: crossbeam::channel::Receiver<repl::Line>,
    sender: crossbeam::channel::Sender<repl::Line>,
    idle_marker: Option<std::path::PathBuf>,
}

#[cfg(unix)]
impl Reads for IdleExecSeat {
    fn read(&self) -> repl::Line {
        if self.first.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return repl::Line::Submitted(self.first_prompt.into());
        }
        if let Some(marker) = &self.idle_marker {
            std::fs::write(marker, "idle").unwrap();
            // The task test closes input explicitly after its continuation.
            return self
                .wake
                .recv_timeout(std::time::Duration::from_secs(30))
                .expect("timed out waiting for task settlement or fixture shutdown");
        }
        self.wake
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap_or(repl::Line::Ended)
    }
}

#[cfg(unix)]
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
                "sleep 1.2; printf FAILED-RESULT; exit 7",
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
            first_prompt: "start background work",
            wake,
            sender,
            idle_marker: None,
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
                    resume_after_root: None,
                    root_transition: RootTransitionSlot::default(),
                    seed_opening_prompt: true,
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

// Catches: unchanged pending wakes restart an active goal, or duplicate native
// completion wakes buy another main/checker generation after its existing wait.
#[test]
#[cfg(unix)]
fn waiting_goal_ignores_pending_wakes_and_consumes_native_completion_once() {
    if std::env::var_os(CHILD).is_none() {
        let stub = child_case(
            "waiting_goal_ignores_pending_wakes_and_consumes_native_completion_once",
            "goal-pending-wakes",
            Plan {
                completions: vec![
                    Reply::Sse(tool_call("launch", "exec", &serde_json::json!({
                        "argv":["/bin/sh", "-c", "while [ ! -f release-process ]; do sleep 0.01; done; printf GOAL-NATIVE-RECEIPT"],
                        "background":true, "timeout_ms":2000,
                    }).to_string())),
                    Reply::Sse(text("waiting")),
                    Reply::Sse(text(r#"{"verdict":"continue","reason":"receipt unavailable"}"#)),
                    Reply::Sse(text("still waiting")),
                    Reply::Sse(text(r#"{"verdict":"continue","reason":"process pending"}"#)),
                    Reply::Sse(text("no new evidence")),
                    Reply::Sse(text("received native receipt")),
                    Reply::Sse(text(r#"{"verdict":"achieved","reason":"native receipt verified"}"#)),
                ],
                ..Plan::default()
            },
        );
        let requests = stub.generations();
        assert_eq!(
            requests.len(),
            8,
            "pending and duplicate wakes generated work"
        );
        assert!(requests[6].to_string().contains("GOAL-NATIVE-RECEIPT"));
        assert!(requests[7].to_string().contains("GOAL-NATIVE-RECEIPT"));
        return;
    }
    #[derive(Debug)]
    struct GoalSeat {
        inner: IdleExecSeat,
        idle_reads: std::sync::atomic::AtomicUsize,
    }
    impl Reads for GoalSeat {
        fn read(&self) -> repl::Line {
            if self
                .inner
                .first
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                return repl::Line::Submitted(self.inner.first_prompt.into());
            }
            match self
                .idle_reads
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            {
                0 | 1 => return repl::Line::HostCommand(repl::HostCommand::BackgroundExecSettled),
                2 => std::fs::write("release-process", "ready").unwrap(),
                _ => {}
            }
            self.inner
                .wake
                .recv_timeout(std::time::Duration::from_secs(3))
                .unwrap_or(repl::Line::Ended)
        }
    }
    impl InteractiveSeat for GoalSeat {
        fn wake_background_exec(&self) {
            self.inner.wake_background_exec();
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
    runtime().block_on(async {
        let (sender, wake) = crossbeam::channel::bounded(2);
        let seat = Arc::new(GoalSeat {
            inner: IdleExecSeat {
                first: std::sync::atomic::AtomicBool::new(true),
                first_prompt: "start background work",
                wake,
                sender,
                idle_marker: None,
            },
            idle_reads: std::sync::atomic::AtomicUsize::new(0),
        });
        let mut arguments = args("anthropic/fixture-vision");
        arguments.mode = Some(ego_core::config::Mode::Yolo);
        arguments.goal = Some("verify the native process receipt".into());
        let ended = execute(
            arguments,
            |_| &NullSink,
            CancelToken::new(),
            None,
            RunHost {
                driver: Driver::Interactive {
                    seat: seat.clone(),
                    between: &|| {},
                    resume_after_root: None,
                    root_transition: RootTransitionSlot::default(),
                    seed_opening_prompt: true,
                },
                elicitor: None,
                status: None,
                diagnostics: &Stderr,
                text_output: None,
                evidence: Arc::new(EvidenceAdmissionService::new()),
            },
        )
        .await
        .unwrap();
        assert_eq!(ended.termination.exit_code(), 0);
        assert_eq!(
            ended.state.goal.unwrap().state,
            ego_core::goal::GoalState::Achieved
        );
        assert!(ended.state.detached_execs.values().all(Option::is_some));
        assert!(seat.idle_reads.load(std::sync::atomic::Ordering::SeqCst) >= 4);
    });
}

// Catches: a settled durable task never reaches an idle parent without a user
// prompt, or a configured filter rejects that internal continuation.
#[test]
#[cfg(unix)]
fn idle_background_task_settlement_starts_one_continuation() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    const PROMPT: &str = "Wait for the parent to become idle, then report completion";
    if std::env::var_os(CHILD).is_none() {
        for (case, model, effort) in [
            ("task-idle", "ollama/qwen3", "off"),
            ("task-idle-filter", "anthropic/claude-sonnet-5-5", "low"),
        ] {
            let stub = child_case(
                "idle_background_task_settlement_starts_one_continuation",
                case,
                Plan {
                    completions: vec![
                        Reply::Sse(tool_call(
                            "task-launch",
                            "task",
                            &serde_json::json!({
                                "name":"idle check", "prompt":PROMPT, "background":true,
                            "model":model, "effort":effort
                            })
                            .to_string(),
                        )),
                        Reply::Sse(text("waiting for background task")),
                        Reply::Sse(text("background task outcome received")),
                    ],
                    prompt_completions: vec![(
                        PROMPT.into(),
                        vec![
                    Reply::Sse(tool_call("wait-idle", "exec", &serde_json::json!({
                        "argv":["/bin/sh", "-c", "while [ ! -f parent-idle ]; do sleep 0.01; done"],
                        "timeout_ms":2000
                    }).to_string())),
                    Reply::Sse(text("IDLE-TASK-RESULT")),
                ],
                    )],
                    ..Plan::default()
                },
            );
            let requests = stub.generations();
            assert_eq!(
                requests.len(),
                5,
                "three parent requests and two child requests; stale wake must not generate"
            );
            assert!(
                requests
                    .last()
                    .unwrap()
                    .to_string()
                    .contains("IDLE-TASK-RESULT")
            );
            if case == "task-idle-filter" {
                let first = requests[0].to_string();
                assert!(
                    first.contains("__EGO_DLP_"),
                    "the real filter must mask the source prompt"
                );
                assert!(!first.contains("owner@example.test"));
                assert!(requests.last().unwrap().to_string().contains("__EGO_DLP_"));
            }
        }
        return;
    }
    // The live CLI uses a multithread runtime: children must progress during seat.read().
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let (sender, wake) = crossbeam::channel::unbounded();
            let shutdown = sender.clone();
            let completed_turns = AtomicUsize::new(0);
            let seat = Arc::new(IdleExecSeat {
                first: AtomicBool::new(true),
                first_prompt: if std::env::var(CHILD).unwrap() == "task-idle-filter" {
                    "start background work; owner@example.test"
                } else {
                    "start background work"
                },
                wake,
                sender,
                idle_marker: Some(std::env::current_dir().unwrap().join("parent-idle")),
            });
            let mut arguments = args(if std::env::var(CHILD).unwrap() == "task-idle-filter" {
                "anthropic/claude-sonnet-5-5"
            } else {
                "ollama/qwen3"
            });
            arguments.mode = Some(ego_core::config::Mode::Yolo);
            if std::env::var(CHILD).unwrap() == "task-idle-filter" {
                arguments.thinking = Some(ThinkingLevel::Low);
            }
            let ended = execute(
                arguments,
                |_| &NullSink,
                CancelToken::new(),
                None,
                RunHost {
                    driver: Driver::Interactive {
                        seat,
                        between: &|| {
                            if completed_turns.fetch_add(1, Ordering::SeqCst) == 1 {
                                // Keep any queued duplicate wakes ahead of EOF.
                                shutdown.send(repl::Line::Ended).unwrap();
                            }
                        },
                        resume_after_root: None,
                        root_transition: RootTransitionSlot::default(),
                        seed_opening_prompt: true,
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
            assert_eq!(ended.termination.exit_code(), 0, "{:?}", ended.termination);
            assert!(
                ended
                    .state
                    .items
                    .values()
                    .any(|item| item.text.contains("IDLE-TASK-RESULT")),
                "the child must really complete, not just launch or fail; items: {:?}",
                ended
                    .state
                    .items
                    .values()
                    .filter(|item| item.text.contains("Background agent")
                        || item.text.contains("IDLE-")
                        || item.text.contains("protocol_filter"))
                    .map(|item| item.text.chars().take(240).collect::<String>())
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                ended.state.turns.len(),
                2,
                "one launch turn and one automatic continuation while idle"
            );
            assert_eq!(ended.state.delivered_subagents.len(), 1);
        });
}

// Catches: a failed output owner cancels the process token, but the independent
// interactive turn keeps streaming and reports success instead of settling.
#[test]
fn host_output_failure_cancels_the_active_interactive_turn() {
    if std::env::var_os(CHILD).is_none() {
        let stub = child_case(
            "host_output_failure_cancels_the_active_interactive_turn",
            "host-output-failure",
            Plan {
                completions: vec![support::paced(
                    "OUTPUT-807-FAILURE",
                    std::time::Duration::from_millis(100),
                )],
                ..Plan::default()
            },
        );
        assert_eq!(
            stub.generations().len(),
            1,
            "no generation after host failure"
        );
        return;
    }
    struct FailedOutput(CancelToken);
    impl EventSink for FailedOutput {
        fn publish(&self, event: &PublicEvent, _: &SessionState) {
            if matches!(event, PublicEvent::ItemDelta { .. }) {
                // Same authority as Stdout::failed and own_terminal's read error.
                self.0.cancel(CancelCause::Host);
            }
        }
    }
    runtime().block_on(async {
        let cancel = CancelToken::new();
        let sink = FailedOutput(cancel.clone());
        let seat = Arc::new(ClipboardSeat {
            images: repl::ImageDraft::default(),
            case: "host-output-failure".into(),
            lines: Mutex::new(VecDeque::from([repl::Line::Submitted(
                "respond while the output owner fails".into(),
            )])),
            chips: Mutex::new(Vec::new()),
            notices: Mutex::new(Vec::new()),
            inserted: Mutex::new(Vec::new()),
        });
        let ended = execute(
            args("ollama/qwen3"),
            |_| &sink,
            cancel.clone(),
            None,
            RunHost {
                driver: Driver::Interactive {
                    seat,
                    between: &|| {},
                    resume_after_root: None,
                    root_transition: RootTransitionSlot::default(),
                    seed_opening_prompt: true,
                },
                elicitor: None,
                status: None,
                diagnostics: &Stderr,
                text_output: None,
                evidence: Arc::new(EvidenceAdmissionService::new()),
            },
        )
        .await
        .expect("admitted interactive driver");
        assert_eq!(cancel.cause(), Some(CancelCause::Host));
        assert!(
            matches!(
                ended.termination,
                RunTermination::Signalled(ego_core::run::Signal::Term)
            ),
            "failed host must not report a successful active turn: {:?}",
            ended.termination
        );
        assert!(
            ended.state.turns.values().any(|turn| matches!(
                turn.exit_reason,
                Some(ego_core::event::domain::ExitReason::Cancelled(
                    CancelCause::Host
                ))
            )),
            "the journal must settle the active turn with the host cause"
        );
    });
}

#[cfg(unix)]
#[derive(Debug)]
struct PressureNativeSeat {
    notes: Mutex<Vec<String>>,
    reads: std::sync::atomic::AtomicUsize,
    sessions: Mutex<Vec<SessionId>>,
    availability: Mutex<repl::actions::Availability>,
    wake: crossbeam::channel::Receiver<repl::Line>,
    sender: crossbeam::channel::Sender<repl::Line>,
    case: String,
    cancel: CancelToken,
}

#[cfg(unix)]
impl Diagnostics for PressureNativeSeat {
    fn note(&self, message: &str) {
        self.notes.lock().unwrap().push(message.to_owned());
    }
}

#[cfg(unix)]
impl Reads for PressureNativeSeat {
    fn read(&self) -> repl::Line {
        let read = self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        assert!(read < 8, "pressure hold must not busy retry");
        if read == 0 {
            let root = PathBuf::from(std::env::var_os("EGO_HOME").unwrap()).join("sessions");
            let ids: Vec<SessionId> = std::fs::read_dir(root)
                .unwrap()
                .filter_map(|entry| {
                    let entry = entry.unwrap();
                    entry
                        .path()
                        .join("events.jsonl")
                        .exists()
                        .then(|| entry.file_name().to_str().unwrap().parse().unwrap())
                })
                .collect();
            assert_eq!(ids.len(), 1, "fixture starts one actual source session");
            self.sessions.lock().unwrap().extend(ids);
            return repl::Line::Submitted("launch native work and wait for its receipt".into());
        }
        if read == 1 {
            assert!(
                self.notes
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|line| line
                        .contains("compaction is waiting for background commands to finish")),
                "the terminal must explain the context hold before waiting for input"
            );
            assert_eq!(
                self.sessions.lock().unwrap().len(),
                1,
                "pressure replaced the command owner before settlement"
            );
            assert_eq!(
                *self.availability.lock().unwrap(),
                repl::actions::Availability::Enabled
            );
            // The process itself records its PID. This is not an injected task label.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !Path::new("native.pid").exists() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "native process never started"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            if self.case.ends_with("cancel") {
                self.cancel.cancel(CancelCause::Host);
                return repl::Line::Ended;
            }
            if self.case.ends_with("manual") {
                return repl::Line::HostCommand(repl::HostCommand::Action(
                    repl::actions::ActionId::Compact,
                ));
            }
            if self.case.ends_with("quit") {
                return repl::Line::Ended;
            }
            if self.case.ends_with("input") {
                return repl::Line::Submitted("PRIORITY-INPUT-1144".into());
            }
        }
        if let Ok(line) = self.wake.try_recv() {
            return line;
        }
        if self.sessions.lock().unwrap().len() == 2 {
            return repl::Line::Ended;
        }
        std::fs::write("release-native", "release").unwrap();
        self.wake
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("native completion must wake the held driver")
    }
}

#[cfg(unix)]
impl InteractiveSeat for PressureNativeSeat {
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
    fn confirm_compaction(&self, _: SessionId) -> bool {
        true
    }
    fn activate_session(&self, id: SessionId) -> bool {
        self.sessions.lock().unwrap().push(id);
        true
    }
    fn set_compaction_availability(&self, availability: repl::actions::Availability) {
        *self.availability.lock().unwrap() = availability;
    }
    fn wake_background_exec(&self) {
        // A coalesced completion notification cannot buy a second generation.
        for _ in 0..2 {
            let _ = self.sender.try_send(repl::Line::HostCommand(
                repl::HostCommand::BackgroundExecSettled,
            ));
        }
    }
}

// Catches: automatic pressure replaces the real native owner; a held hard-limit
// turn calls the provider; queued input is lost; or explicit EOF stops cleaning up.
#[test]
#[cfg(unix)]
fn pressure_recovery_preserves_native_owner_until_real_settlement() {
    const CHECKPOINT: &str = "# Objective\nCollect native work.\n# Binding constraints and preferences\nNone.\n# Completed work and evidence\nNative work settled.\n# Work in progress\nVerify completion.\n# Decisions and rationale\nNone.\n# Blockers and uncertainty\nNone.\n# Ordered next steps\nFinish.\n# Critical references\nNone.";
    let Ok(case) = std::env::var(CHILD) else {
        for (case, used) in [
            ("pressure-native-threshold", 115_200),
            ("pressure-native-hard", 128_000),
            ("pressure-native-input", 128_000),
            ("pressure-native-quit", 128_000),
            ("pressure-native-cancel", 128_000),
            ("pressure-native-manual", 128_000),
        ] {
            let stub = child_case(
                "pressure_recovery_preserves_native_owner_until_real_settlement", case,
                Plan { completions: vec![
                    Reply::Sse(tool_call("launch-native", "exec", &serde_json::json!({
                        "argv":["/bin/sh", "-c", "printf '%s' $$ > native.pid; while [ ! -f release-native ]; do sleep 0.01; done; printf NATIVE-1144-RECEIPT"],
                        "background":true, "timeout_ms":15000,
                    }).to_string()).replace(
                        "\"prompt_tokens\":10,\"completion_tokens\":4,\"total_tokens\":14",
                        &format!("\"prompt_tokens\":{used},\"completion_tokens\":0,\"total_tokens\":{used}"),
                    )),
                    Reply::Sse(text(CHECKPOINT)),
                    Reply::Sse(text("native recovery finished")),
                ], ..Plan::default() },
            );
            let requests = stub.generations();
            assert_eq!(
                requests.len(),
                if case.ends_with("quit") || case.ends_with("cancel") {
                    1
                } else if case.ends_with("manual") {
                    2
                } else {
                    3
                },
                "{case}: extra generation while held or after duplicate wake"
            );
            if !case.ends_with("quit") && !case.ends_with("cancel") && !case.ends_with("manual") {
                let summary = requests[1].to_string();
                assert!(
                    summary.contains("NATIVE-1144-RECEIPT"),
                    "{case}: recovery did not receive the real command output: {summary}"
                );
                if case.ends_with("input") {
                    assert!(
                        summary.contains("PRIORITY-INPUT-1144"),
                        "pending input vanished: {summary}"
                    );
                }
            }
        }
        return;
    };
    runtime().block_on(async {
        let (sender, wake) = crossbeam::channel::bounded(4);
        let cancel = CancelToken::new();
        let seat = Arc::new(PressureNativeSeat {
            notes: Mutex::new(Vec::new()),
            reads: std::sync::atomic::AtomicUsize::new(0),
            sessions: Mutex::new(Vec::new()),
            availability: Mutex::new(repl::actions::Availability::Enabled),
            wake,
            sender,
            case: case.clone(),
            cancel: cancel.clone(),
        });
        let mut arguments = args("anthropic/fixture-vision");
        arguments.mode = Some(ego_core::config::Mode::Yolo);
        let ended = execute(
            arguments,
            |_| &NullSink,
            cancel.clone(),
            None,
            RunHost {
                driver: Driver::Interactive {
                    seat: seat.clone(),
                    between: &|| {},
                    resume_after_root: None,
                    root_transition: RootTransitionSlot::default(),
                    seed_opening_prompt: true,
                },
                elicitor: None,
                status: None,
                diagnostics: seat.as_ref(),
                text_output: None,
                evidence: Arc::new(EvidenceAdmissionService::new()),
            },
        )
        .await
        .unwrap();
        if case.ends_with("cancel") {
            assert!(cancel.is_cancelled());
        } else {
            assert_eq!(
                ended.termination.exit_code(),
                0,
                "{case}: {:?}",
                ended.termination
            );
        }
        let sessions = seat.sessions.lock().unwrap();
        assert_eq!(
            sessions.len(),
            if case.ends_with("quit") || case.ends_with("cancel") {
                1
            } else {
                2
            }
        );
        let journal = std::fs::read_to_string(
            PathBuf::from(std::env::var_os("EGO_HOME").unwrap())
                .join("sessions")
                .join(sessions[0].to_string())
                .join("events.jsonl"),
        )
        .unwrap();
        let events: Vec<serde_json::Value> = journal
            .lines()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line).unwrap()["payload"]["event"]["kind"]
                    .clone()
            })
            .collect();
        let started: Vec<_> = events
            .iter()
            .filter(|event| event["kind"] == "DetachedExecStarted")
            .collect();
        assert_eq!(
            started.len(),
            1,
            "the real command was not registered exactly once"
        );
        let settled: Vec<_> = events
            .iter()
            .filter(|event| event["kind"] == "DetachedExecSettled")
            .collect();
        if !case.ends_with("quit") && !case.ends_with("cancel") && !case.ends_with("manual") {
            assert_eq!(settled.len(), 1, "completion was lost or delivered twice");
            assert_eq!(
                started[0]["id"], settled[0]["snapshot"]["id"],
                "recovery changed the actual command ID"
            );
            assert!(journal.contains("NATIVE-1144-RECEIPT"));
        }
        let pid = std::fs::read_to_string("native.pid").unwrap();
        let probe = std::process::Command::new("/bin/kill")
            .args(["-0", pid.trim()])
            .output()
            .unwrap();
        assert!(
            !probe.status.success(),
            "owned native child survived driver cleanup"
        );
        assert_eq!(
            Path::new("release-native").exists(),
            !case.ends_with("quit") && !case.ends_with("cancel") && !case.ends_with("manual")
        );
    });
}

/// Windows refuses `--offline`: it cannot keep spawned processes off the network.
/// The account admission under test does not depend on it.
const ACCOUNT_NETWORK: &str = if cfg!(windows) { "on" } else { "off" };

// Catches: run-first B is silently replaced by default A before the first selection event;
// an explicit full clear/unbound log adopts A; or an explicit operator override is ignored.
#[test]
fn account_findings_run_first_resume_preserves_admitted_account_and_refuses_implicit_adoption() {
    const ACCOUNT_CHILD: &str = "EGO_ACCOUNT_FINDINGS_RESUME_CHILD";
    if std::env::var_os(ACCOUNT_CHILD).is_none() {
        for case in [
            "bound",
            "cleared",
            "unbound",
            "override",
            "numbered_override",
            "legacy",
            "preflight",
        ] {
            let home = tempfile::tempdir().unwrap();
            let workspace = tempfile::tempdir().unwrap();
            let stub = Stub::start(Plan {
                completions: vec![Reply::Status(400)],
                ..Plan::default()
            });
            std::fs::write(home.path().join("config.toml"), format!("connection='A'\nmodel='anthropic/claude-sonnet-5-5'\nthinking='low'\nsandbox='off'\nnetwork='{ACCOUNT_NETWORK}'\n[connections.A]\nname='Same label'\nvendor='anthropic'\naccess='key'\nkey_env='ACCOUNT_A_KEY'\n[connections.B]\nname='Same label'\nvendor='anthropic'\naccess='key'\nkey_env='ACCOUNT_B_KEY'\n[account_numbers.anthropic]\nA=1\nB=2\n")).unwrap();
            if case == "legacy" {
                std::fs::write(home.path().join("config.toml"), format!("model='anthropic/claude-sonnet-5-5'\nthinking='low'\nsandbox='off'\nnetwork='{ACCOUNT_NETWORK}'\n")).unwrap();
            }
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "run::phase_boundary_tests::account_findings_run_first_resume_preserves_admitted_account_and_refuses_implicit_adoption", "--nocapture"])
                .env_clear().envs(support::coverage_environment()).envs(support::windows_system_environment())
                .env("PATH", support::fixture_path()).env("RUST_MIN_STACK", "16777216")
                .env("TMPDIR", std::env::temp_dir()).env("HOME", home.path()).env("EGO_HOME", home.path())
                .env("ANTHROPIC_BASE_URL", format!("http://{}", stub.address))
                .env("ACCOUNT_A_KEY", "synthetic-account-A-key").env("ACCOUNT_B_KEY", "synthetic-account-B-key")
                .env("ANTHROPIC_API_KEY", "unrelated-ambient-key").env(ACCOUNT_CHILD, case)
                .current_dir(workspace.path()).output().unwrap();
            assert!(
                output.status.success(),
                "{case}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let requests = stub.requests();
            let generations: Vec<_> = requests
                .iter()
                .filter(|request| request.path.ends_with("/messages"))
                .collect();
            if ["cleared", "unbound", "numbered_override"].contains(&case) {
                assert!(
                    requests.is_empty(),
                    "{case}: an unbound or mismatched session must refuse before HTTP"
                );
            } else {
                assert_eq!(generations.len(), 1, "{case}: {requests:?}");
                assert_eq!(
                    generations[0].headers.get("x-api-key").map(String::as_str),
                    Some(if case == "override" {
                        "synthetic-account-A-key"
                    } else if case == "legacy" {
                        "unrelated-ambient-key"
                    } else {
                        "synthetic-account-B-key"
                    }),
                    "{case}"
                );
                assert!(!generations[0].headers.contains_key("authorization"));
            }
        }
        return;
    }
    runtime().block_on(async {
        use ego_core::run::{RequestDigest, RunCommandOutcome, StartRequest};
        use ego_core::store::session_store::SessionEntry;
        use futures::TryStreamExt as _;
        let case = std::env::var(ACCOUNT_CHILD).unwrap();
        let root = std::env::current_dir().unwrap().canonicalize().unwrap();
        let sessions = root.join("sessions");
        let store = Arc::new(session::jsonl(sessions.clone(), root.clone()));
        let mut executor = CliRunExecutor::for_acp(Some(sessions.clone()), SessionShape::default(), store.clone(), Arc::new(EvidenceAdmissionService::new()));
        executor.args.offline = ACCOUNT_NETWORK == "off";
        let controller = RunController::new(store.clone(), Arc::new(executor), Arc::new(SystemClock), Arc::new(CliChildIds), RunRetention::new(NonZeroUsize::new(8).unwrap()).unwrap());
        let session_id = SessionId::new();
        let selections = RunSelections::empty().with_connection(if case == "unbound" || case == "legacy" { None } else { Some("B".into()) });
        let admitted = controller.start(StartRequest::new(session_id, RunRoot::new(root).unwrap(), RequestId::from_uuid(uuid::Uuid::now_v7()), RequestDigest::parse("owned-account-admission").unwrap(), selections, "admitted but not executed"), RunExecutionBindings::new(Arc::new(Sinks), None)).await.unwrap();
        assert!(matches!(admitted, RunCommandOutcome::Admitted(_)));
        // Deliberately stop at the production admission boundary: no execute/submit.
        drop(admitted);
        drop(controller);
        let entries: Vec<_> = ego_core::store::session_store::SessionStore::read(store.as_ref(), &session_id).unwrap().try_collect().await.unwrap();
        assert!(entries.iter().any(|entry| matches!(entry, SessionEntry::RunTransaction(_))));
        assert!(!entries.iter().any(|entry| matches!(entry, SessionEntry::Event(event) if matches!(event.kind, EventKind::SessionSelectionsSelected { .. }))));
        if case == "cleared" {
            let opened = session::open(store.as_ref(), &session_id, &SystemClock).unwrap();
            let mut journal = Journal::new(session_id, &opened.writer, &SystemClock, &NullSink, Producer::Main, opened.state.clone());
            journal.append(TurnId::new(), None, None, EventKind::SessionSelectionsSelected { selections: RunSelections::empty() }).unwrap();
        }
        let mut arguments = args("anthropic/claude-sonnet-5-5");
        arguments.thinking = Some(ThinkingLevel::Low);
        arguments.session = crate::cli::SessionChoice::Resume(Some(crate::cli::ResumeTarget::Id(session_id.to_string())));
        arguments.session_dir = Some(sessions);
        arguments.offline = ACCOUNT_NETWORK == "off";
        arguments.prompt = Some("resume the interrupted admitted session".into());
        if case == "override" { arguments.connection = Some("A".into()); }
        if case == "numbered_override" { arguments.account = std::num::NonZeroU32::new(1); }
        let preflight = if case == "preflight" {
            Some(super::composition_root::execution_preflight(&arguments, &Driver::Once, &CancelToken::new(), &mut None, &Stderr, None).await.unwrap())
        } else { None };
        let result = execute(arguments, |_| &NullSink, CancelToken::new(), preflight, RunHost { driver: Driver::Once, elicitor: None, status: None, diagnostics: &Stderr, text_output: None, evidence: Arc::new(EvidenceAdmissionService::new()) }).await;
        if case == "numbered_override" {
            assert!(matches!(result, Err(RunTermination::UsageError { message }) if message.contains("saved session uses another account")));
        } else if case == "cleared" || case == "unbound" {
            assert!(matches!(result, Err(RunTermination::UsageError { message }) if message.contains("no committed connection") && message.contains("start a new session")));
        } else {
            let ended = result.unwrap_or_else(|error| panic!("resume refused before the owned HTTP boundary: {error:?}"));
            assert!(matches!(&ended.termination, RunTermination::UsageError { message } if message.contains("the provider returned HTTP 400")), "{:?}", ended.termination);
        }
    });
}

// Catches: port reconstruction reusing a previous phase's socket, or discarding
// the socket between main generations that belong to one admitted phase.
#[test]
fn codex_cache_transport_retains_only_the_current_native_input_phase() {
    use ego_ai::http::{Http, HttpCall, HttpWebSocket, PreparedHttpCall};
    use ego_core::permit::TransportError;
    use futures::future::BoxFuture;
    use serde_json::{Value, json};
    if std::env::var_os(CHILD).is_none() {
        child_case(
            "codex_cache_transport_retains_only_the_current_native_input_phase",
            "codex-phase-cache",
            Plan::default(),
        );
        return;
    }

    #[derive(Default)]
    struct Recording(Arc<Mutex<Vec<Vec<Value>>>>);
    struct Socket {
        id: usize,
        requests: Arc<Mutex<Vec<Vec<Value>>>>,
        replies: VecDeque<Value>,
    }
    impl Http for Recording {
        fn begin_call(&self) -> Box<dyn HttpCall> {
            panic!("fixture must use the WebSocket transport")
        }
        fn open_websocket(
            &self,
            _: PreparedHttpCall,
        ) -> BoxFuture<'static, Result<Box<dyn HttpWebSocket>, TransportError>> {
            let id = {
                let mut requests = self.0.lock().unwrap();
                let id = requests.len();
                requests.push(Vec::new());
                id
            };
            let socket = Socket {
                id,
                requests: Arc::clone(&self.0),
                replies: VecDeque::new(),
            };
            Box::pin(async move { Ok(Box::new(socket) as Box<dyn HttpWebSocket>) })
        }
    }
    impl HttpWebSocket for Socket {
        fn send(&mut self, call: PreparedHttpCall) -> BoxFuture<'_, Result<(), TransportError>> {
            let (_, _, _, request) = call.into_parts();
            let index = {
                let mut requests = self.requests.lock().unwrap();
                let socket = &mut requests[self.id];
                socket.push(serde_json::from_slice(request.body()).unwrap());
                socket.len()
            };
            let items = if (self.id, index) == (1, 1) {
                let tool = json!({"type":"function_call","id":"fc1","call_id":"phase-tool","name":"read","arguments":"{\"path\":\"phase-fixture.txt\"}"});
                self.replies
                    .push_back(json!({"type":"response.output_item.added","item":tool}));
                vec![tool]
            } else {
                Vec::new()
            };
            self.replies.push_back(json!({"type":"response.completed","response":{"id":format!("resp_{}_{index}", self.id),"status":"completed","output":items}}));
            Box::pin(async { Ok(()) })
        }
        fn receive(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, TransportError>> {
            let reply = self
                .replies
                .pop_front()
                .map(|reply| serde_json::to_vec(&reply).unwrap());
            Box::pin(async move { Ok(reply) })
        }
    }

    std::fs::write("phase-fixture.txt", "phase-local tool result").unwrap();
    let recording = Arc::new(Recording::default());
    crate::model::TEST_DRIVER_PROVIDER_HTTP.with(|port| port.replace(Some(recording.clone())));
    runtime().block_on(async {
        let seat = Arc::new(ClipboardSeat {
            case: "codex-phase-cache".into(),
            images: repl::ImageDraft::default(),
            lines: Mutex::new(VecDeque::from([
                repl::Line::Submitted("first native prompt".into()),
                repl::Line::Submitted("second native prompt".into()),
            ])),
            chips: Mutex::new(Vec::new()),
            notices: Mutex::new(Vec::new()),
            inserted: Mutex::new(Vec::new()),
        });
        let mut config = args("openai-codex/gpt-6.1-sol");
        config.thinking = Some(ThinkingLevel::Low);
        let run = execute(
            config,
            |_| &NullSink,
            CancelToken::new(),
            None,
            RunHost {
                driver: Driver::Interactive {
                    seat,
                    between: &|| {},
                    resume_after_root: None,
                    root_transition: RootTransitionSlot::default(),
                    seed_opening_prompt: true,
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
        assert_eq!(run.termination.exit_code(), 0);
    });
    let requests = recording.0.lock().unwrap();
    assert_eq!(requests.iter().map(Vec::len).collect::<Vec<_>>(), [1, 2]);
    let first = &requests[0][0];
    let fresh = &requests[1][0];
    let continuation = &requests[1][1];
    assert!(first.get("previous_response_id").is_none());
    assert!(fresh.get("previous_response_id").is_none());
    for text in ["first native prompt", "second native prompt"] {
        assert!(fresh["input"].to_string().contains(text));
    }
    assert_eq!(continuation["previous_response_id"], "resp_1_1");
    let delta = continuation["input"].as_array().unwrap();
    assert_eq!(delta.len(), 1);
    assert_eq!(delta[0]["type"], "function_call_output");
    assert_eq!(delta[0]["call_id"], "phase-tool");
}

// Catches: a background call acknowledges an inherited or ambiguous account
// instead of requiring an explicit model and one compatible saved account.
#[test]
fn native_task_acknowledges_only_a_unique_explicit_foreign_account() {
    let Ok(case) = std::env::var(CHILD) else {
        for (case, explicit) in [
            ("native-task-account-explicit", true),
            ("native-task-account-default", false),
            ("native-task-account-missing", true),
            ("native-task-account-ambiguous", true),
        ] {
            let mut call = serde_json::json!({"name":"account-check","prompt":"check account selection","background":true});
            if explicit {
                call["model"] = serde_json::json!("openai/gpt-6-luna");
            }
            child_case(
                "native_task_acknowledges_only_a_unique_explicit_foreign_account",
                case,
                Plan {
                    completions: vec![
                        Reply::Sse(tool_call("account-check", "task", &call.to_string())),
                        Reply::Sse(text("done")),
                    ],
                    ..Plan::default()
                },
            );
        }
        return;
    };
    let refusal = match case.as_str() {
        "native-task-account-explicit" => None,
        "native-task-account-default" => Some("must explicitly select a model"),
        "native-task-account-missing" => Some("no saved account"),
        "native-task-account-ambiguous" => Some("multiple saved accounts"),
        _ => panic!("unexpected account case {case}"),
    };
    runtime().block_on(async {
        let mut args = args("anthropic/claude-sonnet-5-5");
        args.thinking = Some(ThinkingLevel::Low);
        args.prompt = Some("launch account check".to_owned());
        let ended = run_with_registry(args, |_| &NullSink, CancelToken::new(), Arc::new(EvidenceAdmissionService::new()), None).await;
        assert_eq!(ended.termination.exit_code(), 0, "{:?}", ended.termination);
        let store = session::jsonl(session::store_root(None).unwrap(), std::env::current_dir().unwrap());
        let events: Vec<_> = SessionStore::read(&store, &ended.session).unwrap().map(|entry| entry.unwrap()).filter_map(|entry| async move {
            match entry { SessionEntry::Event(event) => Some(event), _ => None }
        }).collect().await;
        let replies: Vec<_> = events.iter().filter_map(|event| match &event.kind {
            EventKind::ToolCallCompleted { ok, result } => Some((*ok, result.model_text())), _ => None
        }).collect();
        assert_eq!(events.iter().filter(|event| matches!(&event.kind, EventKind::SubagentRequested { .. })).count(), usize::from(refusal.is_none()), "task replies: {replies:?}");
        assert_eq!(events.iter().filter(|event| matches!(&event.kind, EventKind::SubagentSpawned { .. })).count(), usize::from(refusal.is_none()));
        if let Some(refusal) = refusal {
            assert!(replies.iter().any(|(ok, text)| !ok && text.contains(refusal)), "task replies: {replies:?}");
            assert!(!replies.iter().any(|(ok, text)| *ok && text.contains("background agent")));
        } else {
            assert!(events.iter().any(|event| matches!(&event.kind, EventKind::SubagentRequested { selections, .. }
                if selections.connection() == Some("target") && selections.model().is_some_and(|model| model.0 == "openai/gpt-6-luna"))));
            assert!(events.iter().any(|event| matches!(&event.kind, EventKind::ToolCallCompleted { ok: true, result }
                if result.model_text().contains("background agent"))));
        }
    });
}
