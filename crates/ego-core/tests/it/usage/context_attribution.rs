fn attribution_report(
    events: Vec<DomainEvent>,
    period: UsagePeriod,
    now: u64,
) -> serde_json::Value {
    let session = SessionId::new();
    let store = FixtureStore {
        entries: BTreeMap::from([(
            session,
            events.into_iter().map(SessionEntry::Event).collect(),
        )]),
        metadata: vec![meta(session, Some("fixture/model"), now)],
        reads: Arc::new(AtomicUsize::new(0)),
    };
    let clock = CountingClock {
        now: Timestamp::from_millis(now),
        reads: Arc::new(AtomicUsize::new(0)),
    };
    let before = store.entries.clone();
    let report =
        block_on(UsageReportService::new(&store, &clock).report(&store.metadata, period)).unwrap();
    assert_eq!(
        store.entries, before,
        "observations must not rewrite history"
    );
    assert_eq!(
        report.totals.total.usage,
        Usage::IDENTITY,
        "tool estimates are not billing"
    );
    serde_json::to_value(report).unwrap()
}

fn tool_pair(
    seq: &mut Seq,
    item: ego_core::event::id::ItemId,
    at: u64,
    name: &str,
    args: &str,
    text: &str,
    ok: bool,
) -> Vec<DomainEvent> {
    let mut started = event(
        seq,
        at,
        EventKind::ToolCallStarted {
            name: name.into(),
            args: args.into(),
            call_id: Some(format!("call-{item}")),
        },
    );
    started.env.item = Some(item);
    let mut completed = event(
        seq,
        at + 1,
        EventKind::ToolCallCompleted {
            ok,
            result: ego_core::permit::ToolResult::builtin(text),
        },
    );
    completed.env.item = Some(item);
    vec![started, completed]
}

// Catches: aggregate usage hides the dominant tool or treats missing results as zero-sized evidence.
#[test]
fn recorded_long_session_reports_dominant_tool_without_billing_or_history_changes() {
    let events = include_str!("../../fixtures/context-attribution/long-session.jsonl")
        .lines()
        .map(|line| serde_json::from_str::<DomainEvent>(line).unwrap())
        .collect();
    let report = attribution_report(events, UsagePeriod::All, u64::MAX);
    let attribution = &report["sessions"][0]["context_attribution"];
    assert_eq!(attribution["source"], "durable_tool_events");
    assert_eq!(attribution["provenance"], "utf8_bytes_div_4_ceil");
    let tools = attribution["tools"]
        .as_array()
        .expect("tool context estimates must be visible");
    assert_eq!(tools[0]["tool"], "exec");
    assert_eq!(tools[0]["calls"].as_array().unwrap().len(), 80);
    assert!(tools[0]["result_estimated_tokens"].as_u64().unwrap() > 50_000);
    let unknown_results = tools
        .iter()
        .flat_map(|tool| tool["calls"].as_array().unwrap())
        .filter(|call| call["result"].is_null())
        .count();
    assert_eq!(
        unknown_results, 17,
        "excluded foreign completions remain unknown"
    );
    assert_eq!(attribution["unmatched_results"], 0);
    assert!(
        report["sessions"][0].get("context").is_none(),
        "do not invent provider occupancy"
    );
}

// Catches: raw-JSON fingerprints miss equal reads, while changed results, failures or mutations become false duplicates.
#[test]
fn duplicate_read_estimates_require_equal_arguments_success_and_result() {
    use ego_core::event::id::ItemId;
    let mut seq = Seq::default();
    let mut events = Vec::new();
    for (name, args, text, ok) in [
        ("read", r#"{"path":"a","offset":0}"#, "é🦀", true),
        ("read", r#"{ "offset": 0, "path": "a" }"#, "é🦀", true),
        ("read", r#"{"path":"a","offset":0}"#, "changed", true),
        ("read", r#"{"path":"a","offset":0}"#, "é🦀", false),
        ("write", r#"{"path":"a","offset":0}"#, "é🦀", true),
        ("write", r#"{"path":"a","offset":0}"#, "é🦀", true),
        ("read", r#"{"path":"b","offset":0}"#, "é🦀", true),
        ("read", "invalid json", "é🦀", true),
        ("read", "invalid json", "é🦀", true),
    ] {
        events.extend(tool_pair(
            &mut seq,
            ItemId::new(),
            1_000,
            name,
            args,
            text,
            ok,
        ));
    }
    let report = attribution_report(events, UsagePeriod::All, 2_000);
    let tools = report["sessions"][0]["context_attribution"]["tools"]
        .as_array()
        .unwrap();
    let reads = tools.iter().find(|tool| tool["tool"] == "read").unwrap();
    let calls = reads["calls"].as_array().unwrap();
    assert_eq!(calls[0]["result"]["bytes"], 6);
    assert_eq!(calls[0]["result"]["estimated_tokens"], 2);
    assert_eq!(calls[0]["result"]["provenance"], "builtin");
    assert_eq!(
        calls[1]["result"]["duplicate_of_request_seq"],
        calls[0]["request_seq"]
    );
    for call in calls.iter().skip(2) {
        assert!(call["result"]["duplicate_of_request_seq"].is_null());
    }
    assert_eq!(reads["duplicate_read_estimated_tokens"], 9);
    let writes = tools.iter().find(|tool| tool["tool"] == "write").unwrap();
    assert_eq!(writes["duplicate_read_estimated_tokens"], 0);
}

// Catches: positional pairing attributes parallel results to the wrong tool, and period filtering loses an earlier duplicate source.
#[test]
fn attribution_correlates_items_and_filters_requests_without_claiming_future_results() {
    use ego_core::event::id::ItemId;
    let mut seq = Seq::default();
    let first = tool_pair(&mut seq, ItemId::new(), 1_000, "read", "{}", "same", true);
    let second = tool_pair(&mut seq, ItemId::new(), 3_000, "read", "{}", "same", true);
    let mut pending = event(
        &mut seq,
        3_001,
        EventKind::ToolCallStarted {
            name: "exec".into(),
            args: "{}".into(),
            call_id: Some("pending".into()),
        },
    );
    pending.env.item = Some(ItemId::new());
    let mut unmatched = event(
        &mut seq,
        3_002,
        EventKind::ToolCallCompleted {
            ok: true,
            result: ego_core::permit::ToolResult::builtin("orphan"),
        },
    );
    unmatched.env.item = Some(ItemId::new());
    let future = tool_pair(&mut seq, ItemId::new(), 3_003, "glob", "{}", "future", true);
    let mut events = first;
    events.extend(second);
    events.push(pending);
    events.push(unmatched);
    events.extend(future);
    // Complete a later request first: results must follow item identity, not a queue.
    let mut interleaved = tool_pair(&mut seq, ItemId::new(), 3_000, "grep", "{}", "one", true);
    let mut other = tool_pair(
        &mut seq,
        ItemId::new(),
        3_000,
        "grep",
        "{\"query\":1}",
        "longer",
        true,
    );
    let mut delayed = interleaved.pop().unwrap();
    let mut earlier = other.pop().unwrap();
    events.extend(interleaved);
    events.extend(other);
    earlier.env.seq = seq;
    seq.next();
    delayed.env.seq = seq;
    events.push(earlier);
    events.push(delayed);
    let period = UsagePeriod::Last {
        seconds: NonZeroU64::new(1).unwrap(),
    };
    let report = attribution_report(events, period, 3_003);
    let attribution = &report["sessions"][0]["context_attribution"];
    let tools = attribution["tools"].as_array().unwrap();
    let reads = tools.iter().find(|tool| tool["tool"] == "read").unwrap();
    assert_eq!(reads["calls"].as_array().unwrap().len(), 1);
    assert_eq!(reads["calls"][0]["result"]["duplicate_of_request_seq"], 0);
    assert_eq!(reads["duplicate_read_estimated_tokens"], 2);
    for name in ["exec", "glob"] {
        let tool = tools.iter().find(|tool| tool["tool"] == name).unwrap();
        assert!(
            tool["calls"][0]["result"].is_null(),
            "{name} result remains unknown"
        );
    }
    assert_eq!(attribution["unmatched_results"], 1);
    let grep = tools.iter().find(|tool| tool["tool"] == "grep").unwrap();
    assert_eq!(grep["calls"][0]["result"]["bytes"], 3);
    assert_eq!(grep["calls"][1]["result"]["bytes"], 6);
}
