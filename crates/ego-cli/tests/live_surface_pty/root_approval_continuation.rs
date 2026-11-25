use super::*;

/// Catches: an approved read-only root stops at a new composer instead of
/// continuing the same request, or re-admission replays an earlier effect.
#[test]
fn approved_root_write_continues_the_same_turn_without_more_input() {
    for (sandbox, choice) in [("off", b'2'), ("off", b'3'), ("workspace", b'2')] {
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("approved.txt");
        std::fs::write(&target, "original").unwrap();
        let target = std::fs::canonicalize(target).unwrap();
        let places = support::Places::new();
        places.file("AGENTS.md", "# Root continuation fixture\n");
        let marker = places.file("effect-count.txt", "");
        let write = |id: &str| {
            Reply::Sse(tool_call(
                id,
                "write",
                &serde_json::json!({"files":[{"path":target,"contents":"approved bytes"}]})
                    .to_string(),
            ))
        };
        let stub =
            Stub::start(Plan {
                completions: vec![
                Reply::Sse(tool_call("earlier-effect", "exec", &serde_json::json!({
                    "argv":["/bin/sh", "-c", "printf x >> effect-count.txt"], "timeout_ms":5000
                }).to_string())),
                write("needs-root"),
                write("after-readmission"),
                Reply::Sse(text("ROOT-CONTINUED-WITHOUT-INPUT")),
            ],
                ..Plan::default()
            });
        let ego = places.ego(&stub).conversing().args([
            "--model",
            "ollama/qwen3",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--sandbox",
            sandbox,
            "--mode",
            "yolo",
        ]);
        ego.home_file(
            "config.toml",
            &format!("roots=[{{path='{}',access='read'}}]\n", target.display()),
        );
        let (session, mut input, reader) = PtySession::start(&ego, 40, 160, None);
        let mut output = Vec::new();
        until(&reader, &mut output, b"Trust project files");
        let trust = output.len();
        input.write_all(b"yes\r").unwrap();
        input.flush().unwrap();
        until_prompt_from(&reader, &mut output, trust);
        let start = output.len();
        input.write_all(b"write the approved file\r").unwrap();
        input.flush().unwrap();
        // Catches: identical measurements flood the prompt, or the compact
        // path hides a real execution-policy change under confinement.
        until_normalized(&reader, &mut output, start, "Esc cancels.");
        let question = normalized(&output[start..]);
        let prose = support::notice_prose(&question);
        for disclosed in [
            "Allow read and write access?",
            "Native execution authorization and exec_paths are unchanged.",
            "Saved roots do not grant reusable backend trust.",
        ] {
            assert!(prose.contains(disclosed), "missing {disclosed}: {question}");
        }
        // Terminal soft wraps may split a JSON key, not only prose words.
        let unwrapped = question.lines().collect::<String>();
        if sandbox == "off" {
            assert!(prose.contains("Measured execution policy is unchanged for both choices."));
            assert!(!unwrapped.contains("\"policy_digest\":"), "{question}");
        } else {
            for disclosed in ["Previous:", "Proposed: Session:", "Proposed: Permanent:"] {
                assert!(prose.contains(disclosed), "missing {disclosed}: {question}");
            }
            for field in ["\"capabilities\":[", "\"policy_digest\":"] {
                assert!(unwrapped.contains(field), "missing {field}: {question}");
            }
        }
        assert!(!question.contains("No execution permission is added"));
        if cfg!(target_os = "linux") {
            assert!(
                support::notice_prose(&question)
                    .contains("a read-write root also carries executable reach")
            );
        }
        input.write_all(&[choice]).unwrap();
        input.flush().unwrap();
        // No operator message follows the approval.
        until_prompt_after_reply(&reader, &mut output, b"ROOT-CONTINUED-WITHOUT-INPUT");
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "approved bytes",
            "{sandbox} {choice}"
        );
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            "x",
            "an earlier effect replayed"
        );
        input.write_all(b"/quit\r").unwrap();
        input.flush().unwrap();
        assert!(session.quit(input).success());
        let generations = stub.generations();
        assert_eq!(generations.len(), 4);
        let boundary = generations[2]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "tool" && message["tool_call_id"] == "needs-root")
            .unwrap()["content"]
            .to_string();
        assert!(boundary.contains("did not run"), "{boundary}");
        let exported = places
            .ego(&stub)
            .subcommand("export")
            .args([support::FIXTURE_SESSION_ID, "--format", "json"])
            .run();
        assert_eq!(exported.code, Some(0), "{}", exported.stderr);
        let events: Vec<serde_json::Value> = serde_json::from_str(&exported.stdout).unwrap();
        // The public export omits TurnStarted. All conversation events must
        // belong to the original user turn. A backend change can also record
        // one separate permission item for the measured root approval.
        let original = events
            .iter()
            .find(|event| event["kind"] == "user_message")
            .and_then(|event| event["turn"].as_str())
            .expect("original user turn");
        let separate = events
            .iter()
            .filter(|event| event["turn"].as_str().is_some_and(|turn| turn != original))
            .collect::<Vec<_>>();
        match separate.as_slice() {
            [] => {}
            [requested, decided] => {
                assert_eq!(requested["type"], "item_started", "{separate:?}");
                assert_eq!(requested["kind"], "permission", "{separate:?}");
                assert!(
                    requested["name"]
                        .as_str()
                        .is_some_and(|name| name.starts_with("Re-admit execution policy:")),
                    "{separate:?}"
                );
                assert_eq!(decided["type"], "item_completed", "{separate:?}");
                assert_eq!(decided["kind"], "permission", "{separate:?}");
                assert_eq!(decided["ok"], true, "{separate:?}");
                assert!(requested["item"].is_string(), "{separate:?}");
                assert_eq!(requested["item"], decided["item"], "{separate:?}");
                assert_eq!(requested["turn"], decided["turn"], "{separate:?}");
            }
            _ => panic!("unexpected events outside the original turn: {separate:?}"),
        }
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "turn_completed")
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event["kind"] == "user_message")
                .count(),
            1
        );
        let config = std::fs::read_to_string(places.home_path().join("config.toml")).unwrap();
        assert_eq!(
            config.contains("read-write"),
            choice == b'3',
            "session grant changed saved roots: {config}"
        );
    }
}
