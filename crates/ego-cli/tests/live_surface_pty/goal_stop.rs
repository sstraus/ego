use super::*;

struct ReleaseOnDrop(Arc<ReleaseGate>);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}

// Catches: native stop waits for generation, kills the current reply, or a
// stale evaluator schedules another goal turn after cancellation is durable.
// The next goal must be admitted even when a pre-reply prompt was painted.
#[test]
fn native_goal_stop_settles_during_main_and_checker_without_ending_session() {
    for checking in [false, true] {
        let gate = ReleaseGate::new();
        let Reply::Paced { head, .. } = paced_line("GOAL-STOP-HELD\n", Duration::ZERO) else {
            unreachable!()
        };
        let held = Reply::GatedPaced {
            head: if checking { String::new() } else { head },
            tail: text(if checking {
                "{\"verdict\":\"continue\",\"reason\":\"stale unmet condition\"}"
            } else {
                "GOAL-STOP-REPLY-FINISHED"
            }),
            gate: Arc::clone(&gate),
        };
        let mut completions = if checking {
            vec![Reply::Sse(text("GOAL-STOP-MAIN-DONE")), held]
        } else {
            vec![held]
        };
        let (ordinary, ordinary_gate) = gated_paced_line("GOAL-STOP-ORDINARY-DONE\n");
        completions.extend([
            ordinary,
            Reply::Sse(text("GOAL-STOP-NEXT-DONE")),
            Reply::Sse(text(
                "{\"verdict\":\"achieved\",\"reason\":\"next artifact verified\"}",
            )),
        ]);
        let stub = Stub::start(Plan {
            completions,
            ..Plan::default()
        });
        let _release_held = ReleaseOnDrop(Arc::clone(&gate));
        let ordinary_gate = ReleaseOnDrop(ordinary_gate);
        let ego = Ego::against(&stub).conversing().args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--trust-workspace",
            "--session-id",
            support::FIXTURE_SESSION_ID,
        ]);
        let journal = ego
            .home_path()
            .join("sessions")
            .join(support::FIXTURE_SESSION_ID)
            .join("events.jsonl");
        let (session, mut input, reader) = PtySession::start(&ego, 40, 120, Some((40, 120)));
        let mut output = Vec::new();
        until_prompt_from(&reader, &mut output, 0);
        input
            .write_all(b"/goal preserve the original objective\r")
            .unwrap();
        input.flush().unwrap();
        let held_count = if checking { 2 } else { 1 };
        until(
            &reader,
            &mut output,
            if checking {
                b"GOAL-STOP-MAIN-DONE"
            } else {
                b"GOAL-STOP-HELD"
            },
        );
        // Consume real terminal ticks until the held checker request is visible.
        // The bounded receive is also a deadline; no sleep or blind delay.
        let deadline = std::time::Instant::now() + PROGRESS_DEADLINE;
        while stub.generations().len() < held_count {
            output.extend(
                reader
                    .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                    .expect("held generation started"),
            );
        }
        for (command, expected) in [
            (
                "/goal show\r",
                "goal active: preserve the original objective",
            ),
            ("/name show\r", "name: (none)"),
            (
                "/goal stop\r",
                "goal cancelled: preserve the original objective",
            ),
            (
                "/goal stop\r",
                "goal cancelled: preserve the original objective",
            ),
        ] {
            let start = output.len();
            input.write_all(command.as_bytes()).unwrap();
            input.flush().unwrap();
            until_normalized_without_new_generation(
                &reader,
                &mut output,
                start,
                expected,
                &stub,
                held_count,
            );
        }
        let persisted = std::fs::read_to_string(&journal).unwrap();
        let events = persisted
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter_map(|record| record["payload"].get("event").cloned())
            .map(|event| {
                serde_json::from_value::<ego_core::event::domain::DomainEvent>(event).unwrap()
            })
            .collect::<Vec<_>>();
        let stopped = ego_core::state::SessionState::replay(events.clone())
            .goal
            .unwrap();
        assert_eq!(stopped.state, ego_core::goal::GoalState::Cancelled);
        assert_eq!(stopped.objective, "preserve the original objective");
        assert!(stopped.achieved_by.is_none());
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event.kind,
                    ego_core::event::domain::EventKind::GoalTransitioned {
                        transition: ego_core::goal::GoalTransition::Cancelled { .. },
                        ..
                    }
                ))
                .count(),
            1
        );
        let start = output.len();
        gate.release();
        if checking {
            until_prompt_from(&reader, &mut output, start);
        } else {
            until_prompt_after_reply(&reader, &mut output, b"GOAL-STOP-REPLY-FINISHED");
        }
        assert_eq!(
            stub.generations().len(),
            held_count,
            "stale continuation after stop"
        );
        if !checking {
            assert!(
                normalized(&output).contains("GOAL-STOP-REPLY-FINISHED"),
                "stop killed the admitted reply"
            );
        }
        // Keep a real draft repaint in the transcript before the held reply.
        let start = output.len();
        let ordinary_prompt = "ordinary input still works";
        input.write_all(ordinary_prompt.as_bytes()).unwrap();
        input.flush().unwrap();
        until_normalized(&reader, &mut output, start, ordinary_prompt);
        until_prompt_from(&reader, &mut output, start);
        input.write_all(b"\r").unwrap();
        input.flush().unwrap();

        let ordinary_reply = b"GOAL-STOP-ORDINARY-DONE";
        until(&reader, &mut output, ordinary_reply);
        ordinary_gate.0.release();
        until_prompt_after_reply(&reader, &mut output, ordinary_reply);

        input
            .write_all(b"/goal verify the next objective\r")
            .unwrap();
        input.flush().unwrap();
        until_prompt_after_reply(&reader, &mut output, b"GOAL-STOP-NEXT-DONE");
        input.write_all(b"/quit\r").unwrap();
        input.flush().unwrap();
        assert!(session.quit(input).success());
        let persisted = std::fs::read_to_string(&journal).unwrap();
        let events = persisted
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter_map(|record| record["payload"].get("event").cloned())
            .map(|event| {
                serde_json::from_value::<ego_core::event::domain::DomainEvent>(event).unwrap()
            })
            .collect::<Vec<_>>();
        let current = ego_core::state::SessionState::replay(events).goal.unwrap();
        assert_ne!(current.id, stopped.id);
        assert_eq!(current.objective, "verify the next objective");
        assert_eq!(current.state, ego_core::goal::GoalState::Achieved);
        assert_eq!(stub.generations().len(), held_count + 3);
    }
}

// Catches: native goal stop is rejected behind an outstanding permission modal,
// or stopping the goal implicitly answers/cancels the admitted tool request.
#[test]
fn native_goal_stop_crosses_permission_wait_without_answering_or_killing_the_tool() {
    let mut session = modal_session_with(vec![
        Reply::Sse(tool_call("goal-stop-modal", "exec", SEQ_ASK.1)),
        Reply::Sse(text("GOAL-STOP-MODAL-REPLY-FINISHED")),
    ]);
    session.press(b"/goal preserve the permission-wait session\r");
    session.wait("[d] details");
    assert_eq!(session.stub.generations().len(), 1);
    session.press(b"/goal stop\r");
    session.wait("goal cancelled: preserve the permission-wait session");
    assert_eq!(
        session.stub.generations().len(),
        1,
        "stop must not advance or answer the permission"
    );
    assert!(
        session.screen().join("\n").contains("[y] yes"),
        "permission still belongs to the operator"
    );
    session.press(b"y");
    session.wait("GOAL-STOP-MODAL-REPLY-FINISHED");
    let (generations, _) = session.finish();
    assert_eq!(
        generations.len(),
        2,
        "no goal checker or continuation after native stop"
    );
}
