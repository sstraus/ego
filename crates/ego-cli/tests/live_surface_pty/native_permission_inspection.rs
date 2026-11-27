use super::*;

// Catches: explicit rule edits wait for the model, bypass atomic publication,
// overwrite one another, or leak staged rules into the admitted inventory.
#[test]
fn permission_rule_edits_publish_before_the_parent_is_released() {
    let (reply, gate) = gated_paced_line("PERMISSION-EDITS-1169-HELD");
    let stub = Stub::start(Plan {
        completions: vec![reply],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "off",
        "--trust-workspace",
    ]);
    std::fs::write(ego.home_path().join("config.toml"), "# rule edit fixture\n").unwrap();
    let config = || {
        toml::from_str::<toml::Value>(
            &std::fs::read_to_string(ego.home_path().join("config.toml")).unwrap(),
        )
        .unwrap()
    };
    let (session, mut input, reader) = PtySession::start(&ego, 50, 140, Some((50, 140)));
    struct ReleaseOnDrop(Arc<ReleaseGate>);
    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            self.0.release();
        }
    }
    let _release = ReleaseOnDrop(gate.clone());
    let mut output = Vec::new();
    until_prompt_from(&reader, &mut output, 0);
    input.write_all(b"hold permission edits parent\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, 0, "PERMISSION-EDITS-1169-HELD");
    let mut ids = Vec::<String>::new();
    for (step, (choice, effect)) in [(2, "allow"), (3, "deny"), (4, "both"), (4, "both")]
        .into_iter()
        .enumerate()
    {
        let menu_mark = output.len();
        input.write_all(b"/permissions\r").unwrap();
        input.flush().unwrap();
        until_normalized(
            &reader,
            &mut output,
            menu_mark,
            "Choose permission operation",
        );
        input.write_all(format!("{choice}\r").as_bytes()).unwrap();
        input.flush().unwrap();
        let prompt = if choice == 4 {
            "Canonical permission rule ID: "
        } else {
            "Canonical permission rule: "
        };
        until_normalized(&reader, &mut output, menu_mark, prompt);
        let mark = output.len();
        let value = if choice == 4 {
            ids[step - 2].as_str()
        } else if choice == 3 {
            "read(/outside/permission-1169)"
        } else {
            "list(**)"
        };
        input.write_all(format!("{value}\r").as_bytes()).unwrap();
        input.flush().unwrap();
        let deadline = Instant::now() + PROGRESS_DEADLINE;
        loop {
            let visible = normalized(&output[mark..]);
            if visible.contains("durable")
                || visible.contains("pending until the current turn ends")
                || visible.contains("Messages to be submitted after next tool call")
                || visible.contains("policy control failed:")
            {
                break;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "rule edit receipt: {visible}");
            output.extend(reader.recv_timeout(remaining).unwrap_or_else(|error| {
                panic!("{effect} rule edit receipt: {error}; terminal: {visible}")
            }));
        }
        let receipt = crate::support::notice_prose(&normalized(&output[mark..]));
        assert!(
            receipt.contains("stored_changed") && receipt.contains("durable"),
            "{effect}: {receipt}"
        );
        assert!(
            !receipt.contains("pending until the current turn ends"),
            "{receipt}"
        );
        if choice != 4 {
            ids.push(
                receipt
                    .split_whitespace()
                    .find(|word| word.starts_with("r1:"))
                    .expect("published rule ID")
                    .to_owned(),
            );
        }
        let saved = config();
        let rules = saved.get("permissions");
        assert!(
            rules.is_some() || step == 3,
            "nonempty permissions disappeared: {saved}"
        );
        for key in ["allow", "deny"] {
            let actual = rules
                .and_then(|rules| rules.get(key))
                .and_then(toml::Value::as_array)
                .cloned()
                .unwrap_or_default();
            let expected = match (key, step) {
                ("allow", 0 | 1) => vec![toml::Value::String("list(**)".into())],
                ("deny", 1 | 2) => {
                    vec![toml::Value::String("read(/outside/permission-1169)".into())]
                }
                _ => vec![],
            };
            assert_eq!(actual, expected, "{effect}: persisted {key}");
        }
        if choice == 3 {
            let inventory_mark = output.len();
            input.write_all(b"/permissions\r").unwrap();
            input.flush().unwrap();
            until_normalized(
                &reader,
                &mut output,
                inventory_mark,
                "Choose permission operation",
            );
            let mark = output.len();
            input.write_all(b"1\r").unwrap();
            input.flush().unwrap();
            until_normalized(
                &reader,
                &mut output,
                mark,
                "Full identities: ego rules ls --json",
            );
            let inventory = normalized(&output[mark..]);
            for staged in ["list(**)", "read(/outside/permission-1169)"] {
                assert!(
                    !inventory.contains(staged),
                    "staged rules became admitted: {inventory}"
                );
            }
        }
    }
    assert_eq!(stub.generations().len(), 1, "rule edits reached the model");
    gate.release();
    let mark = output.len();
    until_prompt_from(&reader, &mut output, mark);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
}

// Catches: read-only policy inventory is queued behind the held model, has no
// output sink, mutates user policy, or submits another model prompt.
#[test]
fn permission_list_renders_admitted_rules_before_the_parent_is_released() {
    let (reply, gate) = gated_paced_line("PERMISSION-INVENTORY-1169-HELD");
    let stub = Stub::start(Plan {
        completions: vec![reply],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "off",
        "--trust-workspace",
    ]);
    std::fs::write(
        ego.home_path().join("config.toml"),
        "# Permission inventory fixture: retain these exact bytes.\n",
    )
    .unwrap();
    let config = || std::fs::read(ego.home_path().join("config.toml")).unwrap();
    let (session, mut input, reader) = PtySession::start(&ego, 40, 120, Some((40, 120)));
    struct ReleaseOnDrop(Arc<ReleaseGate>);
    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            self.0.release();
        }
    }
    let _release = ReleaseOnDrop(gate.clone());
    let mut output = Vec::new();
    until_prompt_from(&reader, &mut output, 0);
    input
        .write_all(b"hold permission inventory parent\r")
        .unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, 0, "PERMISSION-INVENTORY-1169-HELD");
    let before = config();
    input.write_all(b"/permissions\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, 0, "Choose permission operation");
    let mark = output.len();
    input.write_all(b"1\r").unwrap();
    input.flush().unwrap();
    // A queue receipt is a conclusive failure, not a reason to wait for a
    // minute or accidentally release the parent before taking the snapshot.
    let deadline = Instant::now() + PROGRESS_DEADLINE;
    loop {
        let visible = normalized(&output[mark..]);
        if visible.contains("Full identities: ego rules ls --json")
            || visible.contains("pending until the current turn ends")
        {
            break;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "permission inventory: {visible}");
        output.extend(
            reader
                .recv_timeout(remaining)
                .expect("permission inventory receipt"),
        );
    }
    let during = normalized(&output[mark..]);
    let generations_before_release = stub.generations().len();
    let after_inspection = config();
    gate.release();
    until_prompt_from(&reader, &mut output, mark);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert!(
        during.contains("Full identities: ego rules ls --json"),
        "inventory waited for the parent: {during}"
    );
    for header in ["Effect", "Rule", "Source"] {
        assert!(during.contains(header), "{during}");
    }
    assert_eq!(
        generations_before_release, 1,
        "permission inspection submitted to the model"
    );
    assert_eq!(
        before, after_inspection,
        "read-only inspection saved user policy"
    );
}

// Catches: Explain has no concurrent handler/output, loses its prepared call
// across the native menus, ignores admitted denies, or becomes a model prompt.
#[test]
fn permission_explain_reports_admitted_deny_before_the_parent_is_released() {
    let (reply, gate) = gated_paced_line("PERMISSION-EXPLAIN-1169-HELD");
    let stub = Stub::start(Plan {
        completions: vec![reply],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "off",
        "--trust-workspace",
    ]);
    std::fs::write(
        ego.home_path().join("config.toml"),
        "[permissions]\ndeny = [\"list(**)\"]\n",
    )
    .unwrap();
    let config = || std::fs::read(ego.home_path().join("config.toml")).unwrap();
    let (session, mut input, reader) = PtySession::start(&ego, 60, 140, Some((60, 140)));
    struct ReleaseOnDrop(Arc<ReleaseGate>);
    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            self.0.release();
        }
    }
    let _release = ReleaseOnDrop(gate.clone());
    let mut output = Vec::new();
    until_prompt_from(&reader, &mut output, 0);
    input
        .write_all(b"hold permission explanation parent\r")
        .unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, 0, "PERMISSION-EXPLAIN-1169-HELD");
    let before = config();
    input.write_all(b"/permissions\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, 0, "Choose permission operation");
    let tools_mark = output.len();
    input.write_all(b"6\r").unwrap();
    input.flush().unwrap();
    until_normalized(
        &reader,
        &mut output,
        tools_mark,
        "Find paths by glob pattern",
    );
    // Select from the actual rendered catalogue rather than assuming a native
    // tool's registration index remains fixed.
    let menu = normalized(&output[tools_mark..]);
    let choice = menu
        .lines()
        .find_map(|line| {
            let (number, label) = line.trim_start_matches([' ', '>']).split_once(". ")?;
            label
                .starts_with("Find paths by glob pattern")
                .then(|| number.parse::<usize>().ok())
                .flatten()
        })
        .expect("glob option in admitted tool menu");
    input
        .write_all("\x1b[B".repeat(choice - 1).as_bytes())
        .unwrap();
    input.write_all(b"\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, tools_mark, "pattern: ");
    let pattern_mark = output.len();
    input.write_all(b"*.rs\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, pattern_mark, "path: ");
    let mark = output.len();
    input.write_all(b".\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, mark, "WOULD_EXECUTE");
    let during = crate::support::notice_prose(&normalized(&output[mark..]));
    let generations_before_release = stub.generations().len();
    let after_inspection = config();
    gate.release();
    until_prompt_from(&reader, &mut output, mark);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    for expected in [
        "TOOL glob",
        "OP 0 list",
        "OUTCOME denied",
        "SETTLEMENT not_run",
        "WOULD_EXECUTE false",
    ] {
        assert!(during.contains(expected), "missing {expected}: {during}");
    }
    assert!(
        !during.contains("pending until the current turn ends"),
        "{during}"
    );
    assert_eq!(generations_before_release, 1, "Explain reached the model");
    assert_eq!(before, after_inspection, "Explain changed user policy");
}
