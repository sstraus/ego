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
            .map(|i| Reply::Sse(text(&format!("GOAL-KZG-ANSWER-{i}"))))
            .collect();
        replies.push(Reply::Sse(text(checkpoint)));
        replies.push(Reply::Sse(text("GOAL-KZG-RESUMED")));
        let stub = Stub::start(Plan {
            completions: replies,
            ..Plan::default()
        });
        let places = support::Places::new();
        places.file("AGENTS.md", "# goal ancestry fixture\n");
        let checker = places.file(
            "checker.sh",
            // A constant reason triggers the five-observation blocker before
            // the 20-turn boundary. Derive progress from actual session input.
            r#"input=$(cat)
step=$(printf '%s' "$input" | grep -o 'GOAL-KZG-ANSWER-[0-9]*' | tail -n 1)
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
            &format!("GOAL-KZG-ANSWER-{continuations}"),
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
        until_normalized(&reader, &mut output, start, "compacted session");
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
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|record| record["payload"]["event"]["kind"]["kind"] == "GoalContinuation")
            .count();
        assert_eq!(count, continuations);
        let target = std::fs::read_dir(places.home_path().join("sessions"))
            .unwrap()
            .map(Result::unwrap)
            .find(|entry| entry.file_name() != support::FIXTURE_SESSION_ID)
            .expect("compaction successor")
            .file_name()
            .to_string_lossy()
            .into_owned();
        let log = std::fs::read_to_string(
            places
                .home_path()
                .join("sessions")
                .join(&target)
                .join("events.jsonl"),
        )
        .unwrap();
        let mut seen = std::collections::HashSet::new();
        for line in log.lines() {
            let record: serde_json::Value = serde_json::from_str(line).unwrap();
            let event = &record["payload"]["event"];
            if event.is_null() {
                continue;
            }
            let parent = &event["env"]["parent"];
            assert!(
                parent.is_null() || seen.contains(parent),
                "unresolved parent: {event}"
            );
            seen.insert(event["env"]["id"].clone());
        }
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
        assert!(resumed.stdout.contains("GOAL-KZG-RESUMED"));
        assert_eq!(stub.generations().len(), continuations + 3);
    }
}
