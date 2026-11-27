use super::*;

// Catches: PostCompact appends a source-session parent into the clean successor,
// so a live session accepts turns but refuses /goal achieve and cold resume.
#[cfg(unix)]
#[test]
fn goal_continuations_compact_without_cross_session_parent() {
    // Three continuations reproduce the report without waiting for twenty;
    // zero and the default ceiling show that compaction, not the count, owns it.
    for continuations in [3, 0, 20] {
        let checkpoint = "# Objective\nFinish the checked objective.\n# Binding constraints and preferences\nNone.\n# Completed work and evidence\nSource turns finished.\n# Work in progress\nOperator verification.\n# Decisions and rationale\nNone.\n# Blockers and uncertainty\nNone.\n# Ordered next steps\nVerify completion.\n# Critical references\nNone.";
        let mut replies: Vec<_> = (0..=continuations)
            .flat_map(|i| {
                // Actual admitted work permits further goal continuations;
                // prose-only turns correctly stop after two empty continuations.
                [
                    Reply::Sse(tool_call(
                        &format!("read-progress-{i}"),
                        "read",
                        r#"{"files":[{"path":"progress.txt"}]}"#,
                    )),
                    Reply::Sse(text(&format!("GOAL-528-ANSWER-{i}"))),
                ]
            })
            .collect();
        replies.push(Reply::Sse(text(checkpoint)));
        replies.push(Reply::Sse(text("GOAL-528-RESUMED")));
        let stub = Stub::start(Plan {
            completions: replies,
            ..Plan::default()
        });
        let places = support::Places::new();
        places.file("AGENTS.md", "# goal ancestry fixture\n");
        places.file("progress.txt", "Objective verification input.\n");
        let checker = places.file(
            "checker.sh",
            // A constant reason triggers the five-observation blocker before
            // the 20-turn boundary. Derive progress from actual session input.
            r#"input=$(cat)
step=$(printf '%s' "$input" | grep -o 'GOAL-528-ANSWER-[0-9]*' | tail -n 1)
printf '{"verdict":"continue","reason":"Verification remains after %s"}\n' "$step"
"#,
        );
        let command = serde_json::json!(["/bin/sh", checker]).to_string();
        let turns = continuations.to_string();
        let ego = places.ego(&stub).conversing().args([
            "--model",
            "ollama/qwen3",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--sandbox",
            "off",
            "--goal",
            "Finish the checked objective",
            "--goal-turns",
            &turns,
            "--goal-check-command",
            &command,
            "--allow",
            "exec(/bin/sh *)",
        ]);
        let (session, mut input, reader) = PtySession::start(&ego, 24, 100, Some((24, 100)));
        let mut output = Vec::new();
        until(&reader, &mut output, b"Trust project files");
        let start = output.len();
        input.write_all(b"yes\r").unwrap();
        input.flush().unwrap();
        until_prompt_from(&reader, &mut output, start);
        let start = output.len();
        input.write_all(b"finish this task\r").unwrap();
        input.flush().unwrap();
        until_normalized(
            &reader,
            &mut output,
            start,
            &format!("GOAL-528-ANSWER-{continuations}"),
        );
        // Intermediate goal continuations also draw empty composers. Only
        // the final stop receipt establishes that /compact can run idle.
        until_normalized(&reader, &mut output, start, "Goal stopped:");
        let stopped = start + find(&output[start..], b"Goal stopped:");
        until_prompt_from(&reader, &mut output, stopped);
        drain_until_quiet(&reader, &mut output);
        let start = output.len();
        input.write_all(b"/compact\r").unwrap();
        input.flush().unwrap();
        until_normalized(&reader, &mut output, start, "Compacting");
        until_successor_ready(&reader, &mut output, start, "compact", &stub);
        drain_until_quiet(&reader, &mut output);
        let start = output.len();
        // A host-command repaint can precede its metadata write. The helper
        // follows it with a visible /help driver round trip before inspection.
        submit_local_action(
            &reader,
            &mut output,
            &mut input,
            &stub,
            "goal achieve",
            None,
        );
        drain_until_quiet(&reader, &mut output);
        let metadata_output = normalized(&output[start..]);
        input.write_all(b"/quit\r").unwrap();
        input.flush().unwrap();
        assert!(session.quit(input).success());
        assert!(
            metadata_output.contains("goal achieved: Finish the checked objective"),
            "metadata failed after {continuations} continuations: {metadata_output}"
        );
        assert!(!metadata_output.contains("session metadata operation failed"));

        let source_log = places
            .home_path()
            .join("sessions")
            .join(support::FIXTURE_SESSION_ID)
            .join("events.jsonl");
        let source_events = std::fs::read_to_string(source_log).unwrap();
        let count = source_events
            .lines()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line).expect("source journal JSON")
            })
            .filter(|record| record["payload"]["event"]["kind"]["kind"] == "GoalContinuation")
            .count();
        assert_eq!(count, continuations);
        let listed = places
            .ego(&stub)
            .subcommand("ls")
            .args(["--output", "json"])
            .run();
        assert_eq!(listed.code, Some(0), "{}", listed.stderr);
        let listing = listed.report();
        let successors: Vec<_> = listing["sessions"]
            .as_array()
            .expect("saved sessions")
            .iter()
            .filter(|row| row["id"] != support::FIXTURE_SESSION_ID)
            .collect();
        assert_eq!(successors.len(), 1, "expected one successor: {listing}");
        let successor = successors[0];
        assert_eq!(successor["lineage"]["kind"], "compaction", "{listing}");
        assert_eq!(
            successor["lineage"]["source"],
            support::FIXTURE_SESSION_ID,
            "{listing}"
        );
        let target = successor["id"]
            .as_str()
            .expect("compaction successor ID")
            .to_owned();
        let log = std::fs::read_to_string(
            places
                .home_path()
                .join("sessions")
                .join(&target)
                .join("events.jsonl"),
        )
        .unwrap();
        let mut seen = std::collections::HashSet::new();
        let mut seeds = 0;
        for line in log.lines() {
            let record: serde_json::Value = serde_json::from_str(line).unwrap();
            let event = &record["payload"]["event"];
            if event.is_null() {
                continue;
            }
            serde_json::from_value::<ego_core::event::id::EventId>(event["env"]["id"].clone())
                .expect("valid semantic event ID");
            if event["kind"]["kind"] == "CompactionSeeded" {
                assert_eq!(
                    event["kind"]["source"],
                    support::FIXTURE_SESSION_ID,
                    "wrong compaction source: {event}"
                );
                seeds += 1;
            }
            let parent = &event["env"]["parent"];
            assert!(
                parent.is_null() || seen.contains(parent),
                "unresolved parent: {event}"
            );
            seen.insert(event["env"]["id"].clone());
        }
        assert!(!seen.is_empty(), "successor journal has no semantic events");
        assert_eq!(
            seeds, 1,
            "successor journal must contain one compaction seed"
        );
        let metadata = places
            .ego(&stub)
            .subcommand("resume")
            .without_fixture_thinking()
            .args([&target, "--goal-operation", "show", "--output", "json"])
            .run();
        assert_eq!(metadata.code, Some(0), "{}", metadata.stderr);
        assert_eq!(metadata.report()["goal"]["state"], "achieved");
        let resumed = places
            .ego(&stub)
            .subcommand("resume")
            .args([
                &target,
                "--sandbox",
                "off",
                "--model",
                "ollama/qwen3",
                "Verify the completed work",
            ])
            .run();
        assert_eq!(resumed.code, Some(0), "{}", resumed.stderr);
        assert!(resumed.stdout.contains("GOAL-528-RESUMED"));
        assert_eq!(stub.generations().len(), 2 * (continuations + 1) + 2);
    }
}
