use super::*;

const TEST: &str = concat!(
    module_path!(),
    "::suspended_batch_resumes_after_the_stdio_server_restarts"
);
const EXIT_AFTER_PUBLICATION: i32 = 86;

// Catches: restart loses the captured continuation, dispatches the original
// lookup twice, or sends a torn/multiple result to the model.
#[test]
fn suspended_batch_resumes_after_the_stdio_server_restarts() {
    let Ok(phase) = std::env::var(CHILD) else {
        for phase in ["suspend", "suspend_answer_recorded"] {
            assert_restart(phase, false);
        }
        assert_restart("suspend_answer_recorded", true);
        return;
    };
    drive_phase(&phase);
}

fn assert_restart(phase: &str, crash_after_publication: bool) {
    let stub = Stub::start(Plan {
        completions: replies(true),
        ..Plan::default()
    });
    let fixture = StdioFixture::new("restart", Duration::ZERO);
    fixture.child(&stub, TEST, phase);
    let log = fixture.workspace.path().join("dispatch.log");
    let before = read_dispatches(&log);
    assert_eq!(before.len(), 2, "{before:?}");
    assert_eq!(before[0]["key"], "alpha");
    assert_eq!(before[1]["key"], "bravo");
    assert_eq!(before[1]["resumed"], false);
    assert_eq!(
        stub.requests()
            .iter()
            .filter(|request| request.path.contains("chat/completions"))
            .count(),
        1
    );
    if crash_after_publication {
        let crashed = fixture.run_child(&stub, TEST, "resume_crash");
        assert_eq!(
            crashed.status.code(),
            Some(EXIT_AFTER_PUBLICATION),
            "{}",
            String::from_utf8_lossy(&crashed.stderr)
        );
        assert_eq!(last_continuation_state(fixture.home.path()), "claimed");
        fixture.child(&stub, TEST, "recover_publication");
    } else {
        fixture.child(&stub, TEST, "resume");
    }
    assert_eq!(last_continuation_state(fixture.home.path()), "completed");
    let after = read_dispatches(&log);
    assert_eq!(after.len(), 3, "{after:?}");
    assert_eq!(&after[..2], before.as_slice());
    assert_eq!(after[2]["key"], "bravo");
    assert_eq!(after[2]["resumed"], true);
    assert_ne!(after[2]["pid"], before[1]["pid"]);
    let requests: Vec<_> = stub
        .requests()
        .into_iter()
        .filter(|request| request.path.contains("chat/completions"))
        .collect();
    assert_eq!(requests.len(), 2);
    let resumed: Value = serde_json::from_str(&requests[1].body).unwrap();
    let results: Vec<_> = resumed["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "tool")
        .collect();
    assert_eq!(results.len(), 1, "{resumed}");
    assert_eq!(results[0]["tool_call_id"], "b1");
    let output: Value = serde_json::from_str(results[0]["content"].as_str().unwrap()).unwrap();
    assert_eq!(output["results"][0]["output"], "value-of-alpha");
    assert_eq!(output["results"][1]["output"], "value-of-bravo");
    assert_eq!(output["results"][2]["status"], "not_run");
}

fn drive_phase(phase: &str) {
    super::headless_driver::TEST_MCP_BATCH.store(true, Ordering::SeqCst);
    let mut args = vec!["ego".to_owned()];
    if matches!(phase, "resume" | "resume_crash" | "recover_publication") {
        let command: (String, String) =
            serde_json::from_slice(&std::fs::read("answer.json").unwrap()).unwrap();
        args.extend(["resume".to_owned(), command.0]);
        if phase == "recover_publication" {
            args.push("Report the previous lookup results.".to_owned());
        } else {
            args.extend(["--answer".to_owned(), command.1]);
        }
    } else {
        assert!(matches!(phase, "suspend" | "suspend_answer_recorded"));
    }
    args.extend(
        [
            "--model",
            "ollama/qwen3",
            "--thinking",
            "off",
            "--sandbox",
            "off",
            "--trust-workspace",
            "--mode",
            "yolo",
        ]
        .map(str::to_owned),
    );
    let parsed = crate::cli::parse(args).unwrap();
    let mut args = match parsed {
        crate::cli::Invocation::Interactive(mut args) => {
            args.prompt = Some("Look up the three keys.".to_owned());
            args
        }
        crate::cli::Invocation::Resume(args) => args,
        _ => panic!("expected start or resume arguments"),
    };
    args.no_hooks = true;
    let publication = PublicationBoundary {
        exit: phase == "resume_crash",
    };
    let sink: &dyn EventSink = if matches!(phase, "resume" | "resume_crash") {
        &publication
    } else {
        &NullSink
    };
    let ended = runtime().block_on(run_with_registry(
        *args,
        |_| sink,
        CancelToken::new(),
        Arc::new(EvidenceAdmissionService::new()),
        None,
    ));
    if matches!(phase, "resume" | "resume_crash" | "recover_publication") {
        assert_ne!(phase, "resume_crash", "the crash sink must run");
        assert_eq!(ended.termination.exit_code(), 0, "{:?}", ended.termination);
    } else {
        assert!(
            matches!(ended.termination, RunTermination::QuestionRequired { .. }),
            "{:?}",
            ended.termination
        );
        let pending = ended
            .state
            .pending_question()
            .expect("durable pending MCP question");
        let ego_core::question::QuestionMode::Round(round) = &pending.request().mode else {
            panic!("expected MCP elicitation round");
        };
        let key = round.requests.keys().next().expect("fixture form key");
        let answer = json!({"confirmed":true,"responses":{(key):{"choice":"yes"}}});
        if phase == "suspend_answer_recorded" {
            // Recreate the durable cut immediately after the turn records the
            // answer. This opens the real session writer but never calls the
            // MCP adapter, so the continuation journal still owns an unclaimed
            // operation and the stdio process has already been closed.
            let store = session::jsonl(
                session::store_root(None).unwrap(),
                std::env::current_dir().unwrap().canonicalize().unwrap(),
            );
            let opened =
                session::Opened::from(session::open(&store, &ended.session, &SystemClock).unwrap());
            let mut journal = Journal::new(
                ended.session,
                opened.store(),
                &SystemClock,
                &NullSink,
                Producer::Main,
                opened.state(),
            );
            journal
                .append(
                    pending.turn(),
                    Some(pending.item_id()),
                    Some(pending.request_event()),
                    EventKind::QuestionSettled {
                        outcome: QuestionOutcome::Answered(
                            QuestionAnswer::from_json(pending.request(), &answer.to_string())
                                .unwrap(),
                        ),
                    },
                )
                .unwrap();
        }
        let command = (
            ended.session.to_string(),
            format!("{}={answer}", pending.item_id()),
        );
        std::fs::write("answer.json", serde_json::to_vec(&command).unwrap()).unwrap();
    }
}

struct PublicationBoundary {
    exit: bool,
}

impl EventSink for PublicationBoundary {
    fn publish(&self, event: &PublicEvent, _state: &SessionState) {
        if !matches!(
            event,
            PublicEvent::ItemCompleted { text: Some(text), .. } if text == "value-of-bravo"
        ) {
            return;
        }
        // EventSink observes the commit after the session writer makes it
        // durable. If Completed is already present, a crash before this
        // append can lose the only result while the claim is terminal.
        let home = std::env::var_os("EGO_HOME").unwrap();
        assert_eq!(
            last_continuation_state(std::path::Path::new(&home)),
            "claimed"
        );
        if self.exit {
            std::process::exit(EXIT_AFTER_PUBLICATION);
        }
    }
}

fn last_continuation_state(home: &std::path::Path) -> String {
    let records = std::fs::read_to_string(home.join("mcp/continuations.jsonl")).unwrap();
    let last: Value = serde_json::from_str(records.lines().last().unwrap()).unwrap();
    last["record"]["state"].as_str().unwrap().to_owned()
}

// Catches: a repeated `resume --answer` after a crash that followed the
// durable result dispatches the MCP call a second time, or poisons the claim so
// that the next normal open can no longer settle it.
#[test]
fn repeated_answer_after_published_crash_never_redispatches_and_next_open_settles_the_claim() {
    let stub = Stub::start(Plan {
        completions: replies(true),
        ..Plan::default()
    });
    let fixture = StdioFixture::new("restart", Duration::ZERO);
    fixture.child(&stub, TEST, "suspend_answer_recorded");
    let log = fixture.workspace.path().join("dispatch.log");
    let crashed = fixture.run_child(&stub, TEST, "resume_crash");
    assert_eq!(crashed.status.code(), Some(EXIT_AFTER_PUBLICATION));
    assert_eq!(last_continuation_state(fixture.home.path()), "claimed");
    assert_eq!(read_dispatches(&log).len(), 3);

    // The command an operator would retype. Its exit status may be an error;
    // the effect must not be sent again.
    let repeat = fixture.run_child(&stub, TEST, "resume");
    let repeated = read_dispatches(&log);
    assert_eq!(
        repeated.len(),
        3,
        "second MCP dispatch after repeat: {repeated:?}\n{}\n{}",
        String::from_utf8_lossy(&repeat.stdout),
        String::from_utf8_lossy(&repeat.stderr)
    );
    fixture.child(&stub, TEST, "recover_publication");
    assert_eq!(last_continuation_state(fixture.home.path()), "completed");
    assert_eq!(read_dispatches(&log).len(), 3);
}

// Catches: a crash after the stdio server answered but before the session
// published the result re-sends the lookup (double effect), or hides that the
// effect may already have happened.
#[test]
fn unpublished_claim_is_never_redispatched_on_answer_or_prompt() {
    let stub = Stub::start(Plan {
        completions: replies(true),
        ..Plan::default()
    });
    let fixture = StdioFixture::new("restart", Duration::ZERO);
    fixture.child(&stub, TEST, "suspend_answer_recorded");
    let log = fixture.workspace.path().join("dispatch.log");
    let crashed = fixture.run_child(&stub, TEST, "resume_crash");
    assert_eq!(crashed.status.code(), Some(EXIT_AFTER_PUBLICATION));
    assert_eq!(read_dispatches(&log).len(), 3);
    // Recreate the crash cut before the session append of the result: the
    // claim is durable, the result is not.
    let sessions = fixture.home.path().join("sessions");
    let events = std::fs::read_dir(&sessions)
        .unwrap()
        .map(|entry| entry.unwrap().path().join("events.jsonl"))
        .find(|path| path.is_file())
        .unwrap();
    let text = std::fs::read_to_string(&events).unwrap();
    let mut lines: Vec<&str> = text.lines().collect();
    assert!(lines.pop().unwrap().contains("ToolCallCompleted"));
    std::fs::write(&events, lines.join("\n") + "\n").unwrap();
    assert_eq!(last_continuation_state(fixture.home.path()), "claimed");

    let answer = fixture.run_child(&stub, TEST, "resume");
    assert!(
        String::from_utf8_lossy(&answer.stderr).contains("may have been sent"),
        "{}",
        String::from_utf8_lossy(&answer.stderr)
    );
    assert_eq!(read_dispatches(&log).len(), 3, "redispatch on --answer");
    fixture.run_child(&stub, TEST, "recover_publication");
    assert_eq!(read_dispatches(&log).len(), 3, "redispatch on prompt");
    assert_eq!(last_continuation_state(fixture.home.path()), "claimed");
}
