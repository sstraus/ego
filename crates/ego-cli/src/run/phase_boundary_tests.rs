//! Behavioral contracts for the extracted driver phases.
use super::*;
use ego_core::control::{ControlError, ControlSelect, SessionControlState, SessionControls};
use ego_core::event::domain::DomainEvent;
use std::collections::VecDeque;

#[path = "../../tests/support/mod.rs"]
mod support;
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
            "image" => assert!(chips.iter().any(|chips| chips == &["clipboard"])),
            "remove" => {
                let staged = chips
                    .iter()
                    .position(|chips| chips == &["clipboard"])
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
