// Catches: a real foreground child publishes activity but never starts or settles
// as interrupted on the host's public event stream.
#[test]
fn interrupted_foreground_child_activity_has_paired_lifecycle() {
    let fx = Fixture::new(vec![]);
    let cancel = CancelToken::new();
    let generator = Arc::new(ConcurrentCancelGenerator {
        calls: AtomicUsize::new(0),
        cancel: cancel.clone(),
    });
    let (exit, state) = run_tasks(&fx, &generator, cancel, None);
    assert_eq!(exit, ExitReason::Cancelled(CancelCause::User));
    assert_eq!(generator.calls.load(Ordering::SeqCst), 3);
    let events: Vec<_> = fx
        .sink
        .events()
        .iter()
        .map(|event| serde_json::to_value(event).unwrap())
        .collect();
    let tasks: Vec<_> = state
        .items
        .iter()
        .filter(|(_, item)| item.tool_name.as_deref() == Some("task"))
        .collect();
    assert_eq!(tasks.len(), 2);
    let mut children = BTreeSet::new();
    for (id, _) in tasks {
        let item = json!(id);
        let progress = events
            .iter()
            .position(|event| event["type"] == "child_progress" && event["item"] == item)
            .expect("foreground child really published activity");
        let lifecycle: Vec<_> = events
            .iter()
            .enumerate()
            .filter(|(_, event)| event["type"] == "child_lifecycle" && event["item"] == item)
            .collect();
        assert_eq!(
            lifecycle.len(),
            2,
            "foreground child activity must have exactly one start and interrupted end"
        );
        let (start_index, start) = lifecycle[0];
        let (end_index, end) = lifecycle[1];
        assert!(start_index < progress && progress < end_index);
        assert_eq!(start["phase"], "started");
        assert_eq!(end["phase"], "settled");
        assert_eq!(end["outcome"], "interrupted");
        assert_eq!(start["child"]["turn"], end["child"]["turn"]);
        assert_ne!(start["child"]["turn"], start["turn"]);
        assert!(children.insert(start["child"]["turn"].as_str().unwrap().to_owned()));
        assert_eq!(start["child"]["kind"], "foreground");
        assert!(start["child"].get("run").is_none());
        assert!(start["child"].get("invocation").is_none());
    }
}

// Catches: a successful foreground child never closes, or invalid task arguments
// manufacture admission before the execution owner actually starts.
#[test]
fn foreground_lifecycle_starts_only_admitted_children_and_settles_completed() {
    for invalid in [false, true] {
        let fx = Fixture::new(vec![]);
        let args = if invalid {
            json!({"name":"missing", "prompt":"Inspect", "agent":"absent"})
        } else {
            json!({"name":"ordinary", "prompt":"Inspect"})
        };
        let generator = Arc::new(FakeGenerator::new([
            vec![
                call("task", "task", args),
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![AiEvent::Stop(StopReason::Stop)],
            vec![AiEvent::Stop(StopReason::Stop)],
        ]));
        let (exit, _) = run_tasks(&fx, &generator, CancelToken::new(), None);
        assert_eq!(exit, ExitReason::Completed);
        let lifecycle: Vec<_> = fx
            .sink
            .events()
            .into_iter()
            .filter(|event| matches!(event, PublicEvent::ChildLifecycle { .. }))
            .collect();
        if invalid {
            assert!(
                lifecycle.is_empty(),
                "refusal before admission is not a child start"
            );
            assert_eq!(generator.calls().len(), 2);
        } else {
            assert_eq!(lifecycle.len(), 2);
            assert!(matches!(
                lifecycle[0],
                PublicEvent::ChildLifecycle {
                    phase: ego_core::public::ChildPhase::Started,
                    ..
                }
            ));
            assert!(matches!(
                lifecycle[1],
                PublicEvent::ChildLifecycle {
                    phase: ego_core::public::ChildPhase::Settled {
                        outcome: ego_core::public::ChildOutcome::Completed
                    },
                    ..
                }
            ));
        }
    }
}
