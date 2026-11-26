//! Named account identity at the packaged CLI and ACP stdio boundary.

use crate::support::{AcpHost, Ego, Places, Plan, Reply, Stub};

const SESSION: &str = "00000000-0000-0000-0000-000000000815";
const KEY_A: &str = "sk-ant-TestOnly0506";
const KEY_B: &str = "sk-ant-TestOnly0507";
const AMBIENT_KEY: &str = "sk-ant-TestOnly0508";
const AMBIENT_TOKEN: &str = "sk-ant-oatTestOnly0509";

/// Catches: a provider-keyed OAuth credential authorizes an unbound request,
/// or shadows the explicitly selected named OAuth credential.
#[test]
fn named_connections_only_selected_slot_can_authorize_a_generation() {
    use ego_cli::auth::{AuthEntry, AuthStore};

    let places = Places::new();
    let stub = Stub::start(Plan {
        completions: vec![Reply::Status(400)],
        ..Plan::default()
    });
    let config_path = places.home_path().join("config.toml");
    let config =
        "model = 'anthropic/claude-opus-5-5'\nthinking = 'low'\nsandbox = 'off'\nnetwork = 'on'\n";
    std::fs::write(&config_path, config).unwrap();
    let store = AuthStore::new(places.home_path());
    let entry = |token: &str| AuthEntry::OAuth {
        access: token.to_owned().into(),
        refresh: None,
        expires: None,
        scope_binding: None,
    };
    let retired = "sk-ant-oatTestOnly0510";
    let selected = "sk-ant-oatTestOnly0511";
    store.put("anthropic", entry(retired)).unwrap();
    let ego = || {
        Ego::at(places.home_path(), places.workspace_path(), Some(&stub))
            .platform_sandbox()
            .env("ANTHROPIC_API_KEY", "")
            .env("CLAUDE_CODE_OAUTH_TOKEN", "")
    };

    let unbound = ego().args(["--output", "json", "Say hello."]).run();
    assert_ne!(unbound.code, Some(0));
    assert!(
        unbound.stderr.contains("ego auth login anthropic"),
        "{}",
        unbound.stderr
    );
    assert!(stub.generations().is_empty(), "retired slot crossed HTTP");

    store.put("connection:work", entry(selected)).unwrap();
    std::fs::write(
        &config_path,
        format!(
            "{config}\n[connections.work]\nname = 'Work'\nvendor = 'anthropic'\naccess = 'oauth'\n"
        ),
    )
    .unwrap();
    let bound = ego()
        .args(["--connection", "work", "--output", "json", "Say hello."])
        .run();
    let requests: Vec<_> = stub
        .requests()
        .into_iter()
        .filter(|request| request.path.ends_with("/messages"))
        .collect();
    assert_eq!(requests.len(), 1, "{}", bound.stderr);
    assert_eq!(
        requests[0].headers.get("authorization").map(String::as_str),
        Some(format!("Bearer {selected}").as_str()),
    );
    assert!(
        !requests[0]
            .headers
            .values()
            .any(|value| value.contains(retired))
    );
}

/// Catches: explicit CLI identity is replaced by the default, ACP reconnect
/// selects by an editable label, or a removed account falls back to ambient auth.
/// Scenario: explicit B -> renamed B on ACP reconnect -> removed B on reconnect.
#[test]
fn named_connections_packaged_reconnect_keeps_b_and_removed_b_refuses_before_http() {
    let places = Places::new();
    // This is an owned transport refusal, not a fabricated provider stream.
    // A terminal 400 makes each admitted prompt send exactly one request.
    let stub = Stub::start(Plan {
        completions: vec![Reply::Status(400)],
        ..Plan::default()
    });
    configure(&places, Some("Work"));

    let seeded = invocation(&places, &stub)
        .args([
            "--connection",
            "account_b",
            "--session-id",
            SESSION,
            "--output",
            "json",
            "Select the explicit account.",
        ])
        .run();
    assert_eq!(seeded.code, Some(2), "{}", seeded.stderr);
    assert!(
        seeded.stderr.contains("the provider returned HTTP 400"),
        "{}",
        seeded.stderr
    );
    assert_b_request(&stub, 1, "explicit CLI selection overrides default A");

    // User configuration still defaults to A. Rename B to A's label as well:
    // neither the default nor a matching label may replace the durable ID.
    configure(&places, Some("Personal"));
    let mut reconnected = host(&places, &stub);
    let loaded = load(&mut reconnected, &places);
    assert!(loaded["result"].is_object(), "{loaded}");
    let answer = reconnected.request(
        "session/prompt",
        serde_json::json!({
            "sessionId": SESSION,
            "prompt": [{"type": "text", "text": "Keep the persisted account."}],
        }),
    );
    assert_eq!(
        answer["error"]["message"], "the turn failed: the provider returned HTTP 400",
        "{answer}"
    );
    reconnected.finish();
    assert_b_request(&stub, 2, "ACP reconnect after a duplicate-label rename");

    // A and both ambient credential sources remain valid synthetic values.
    // Removal must refuse the stable ID rather than silently using any of them.
    configure(&places, None);
    let before_reconnect = stub.requests().len();
    let mut removed = host(&places, &stub);
    let refused = load(&mut removed, &places);
    let message = refused["error"]["message"]
        .as_str()
        .expect("missing durable account must refuse session/load");
    assert!(
        message.contains("the selected account is no longer saved")
            && !message.contains("account_b"),
        "{refused}"
    );
    removed.finish();
    assert_eq!(
        stub.requests().len(),
        before_reconnect,
        "removed account reached HTTP during ACP initialization or session/load: {:?}",
        stub.requests()
    );
}

/// Catches: resuming a different provider sends no request because the saved account is pinned,
/// a same-provider override loses the saved account, or a destination with multiple
/// accounts is chosen without an explicit selection.
#[test]
fn resume_model_override_uses_the_account_of_the_selected_provider() {
    let places = Places::new();
    let stub = Stub::start(Plan {
        completions: vec![Reply::Status(400); 3],
        ..Plan::default()
    });
    configure(&places, Some("Work"));
    let path = places.home_path().join("config.toml");
    let config = format!(
        "{}\n[connections.openai_work]\nname='OpenAI Work'\nvendor='openai'\naccess='key'\nkey_env='PROCESS_OPENAI_API_KEY'\n",
        std::fs::read_to_string(&path).unwrap()
    );
    std::fs::write(&path, &config).unwrap();

    let seeded = invocation(&places, &stub)
        .args([
            "--connection",
            "account_b",
            "--session-id",
            SESSION,
            "First turn.",
        ])
        .run();
    assert!(
        seeded.stderr.contains("the provider returned HTTP 400"),
        "{}",
        seeded.stderr
    );
    let resume = |model| {
        invocation(&places, &stub)
            .env("PROCESS_OPENAI_API_KEY", "synthetic-openai-work-key")
            .subcommand("resume")
            .args([SESSION, "--model", model, "Continue."])
            .run()
    };

    let same = resume("anthropic/claude-sonnet-5-5");
    assert!(
        same.stderr.contains("the provider returned HTTP 400"),
        "{}",
        same.stderr
    );
    assert_b_request(&stub, 2, "same-provider override keeps the saved account");

    std::fs::write(
        &path,
        format!(
            "{config}\n[connections.openai_other]\nname='Other OpenAI Account'\nvendor='openai'\naccess='key'\nkey_env='OTHER_OPENAI_API_KEY'\n"
        ),
    )
    .unwrap();
    let before_refusal = stub.requests().len();
    let refused = resume("openai/gpt-6.1-sol");
    assert_eq!(refused.code, Some(2), "{}", refused.stderr);
    assert!(
        refused
            .stderr
            .contains("from provider anthropic with provider openai")
            && refused.stderr.contains("multiple openai accounts match")
            && refused
                .stderr
                .contains("Rerunning the same command cannot succeed")
            && refused.stderr.matches("ego auth status openai").count() == 1
            && refused.stderr.contains(&format!(
                "ego resume {SESSION} --model openai/gpt-6.1-sol --account N"
            )),
        "{}",
        refused.stderr
    );
    assert_eq!(
        stub.requests().len(),
        before_refusal,
        "ambiguous resume sent a request"
    );
    std::fs::write(&path, config).unwrap();

    let changed = resume("openai/gpt-6.1-sol");
    assert!(
        changed.stderr.contains("the provider returned HTTP 400"),
        "{}",
        changed.stderr
    );
    let requests = stub.requests();
    let openai: Vec<_> = requests
        .iter()
        .filter(|request| request.path.ends_with("/responses"))
        .collect();
    assert_eq!(openai.len(), 1, "{requests:?}");
    assert_eq!(
        openai[0].headers.get("authorization").map(String::as_str),
        Some("Bearer synthetic-openai-work-key")
    );
}

/// Catches: a fork resumes through default A because the target seed lost source B.
#[test]
fn named_connections_packaged_fork_inherits_the_source_account_before_resume() {
    let places = Places::new();
    let stub = Stub::start(Plan {
        completions: vec![Reply::Status(400)],
        ..Plan::default()
    });
    configure(&places, Some("Work"));
    let seeded = invocation(&places, &stub)
        .args([
            "--connection",
            "account_b",
            "--session-id",
            SESSION,
            "--output",
            "json",
            "Seed account B before fork.",
        ])
        .run();
    assert_eq!(seeded.code, Some(2), "{}", seeded.stderr);
    assert!(
        seeded.stderr.contains("the provider returned HTTP 400"),
        "{}",
        seeded.stderr
    );
    assert_b_request(&stub, 1, "fork source");
    let forked = invocation(&places, &stub)
        .subcommand("fork")
        .arg(SESSION)
        .run();
    assert_eq!(forked.code, Some(0), "{}", forked.stderr);
    let target = forked.stdout.trim();
    let _: ego_core::event::id::SessionId =
        target.parse().expect("fork publishes its new session ID");
    let resumed = invocation(&places, &stub)
        .subcommand("resume")
        .args([target, "--output", "json", "Use the inherited account."])
        .run();
    assert_eq!(resumed.code, Some(2), "{}", resumed.stderr);
    assert!(
        resumed.stderr.contains("the provider returned HTTP 400"),
        "{}",
        resumed.stderr
    );
    assert_b_request(&stub, 2, "fork target resume without a connection override");
}

/// Catches: rename/resume/search or logout A replaces the persisted B identity,
/// drops native citations on replay, or damages B's saved credential.
/// Local credentials and published protocol examples do not prove browser login
/// or live subscription entitlement.
#[test]
fn named_connections_saved_b_search_survives_rename_resume_and_logout_a() {
    use ego_cli::auth::{AuthEntry, AuthStore, StoredSecret};
    use serde_json::{Value, json};

    let places = Places::new();
    let store = AuthStore::new(places.home_path());
    for (id, token) in [
        ("account_a", "sk-ant-oatTestOnly0512"),
        ("account_b", "sk-ant-oatTestOnly0513"),
    ] {
        store
            .put(
                &format!("connection:{id}"),
                AuthEntry::OAuth {
                    access: StoredSecret::new(token.as_bytes().to_vec()),
                    refresh: None,
                    expires: None,
                    scope_binding: None,
                },
            )
            .unwrap();
    }
    let config_path = places.home_path().join("config.toml");
    let config = "connection = \"account_a\"\nmodel = \"anthropic/claude-opus-5-5\"\nthinking = \"low\"\nsandbox = \"off\"\nnetwork = \"on\"\n\n[connections.account_a]\nname = \"Personal\"\nvendor = \"anthropic\"\naccess = \"oauth\"\n\n[connections.account_b]\nname = \"Work\"\nvendor = \"anthropic\"\naccess = \"oauth\"\n";
    std::fs::write(&config_path, config).unwrap();

    // Published documentation data, not a live response. Streaming framing is
    // explicit test input, following the hosted-search decoder regression.
    let response: Value = serde_json::from_str(include_str!(
        "../../../ego-ai/tests/goldens/hosted-search/anthropic-response.json"
    ))
    .unwrap();
    let frame = |value: Value| format!("data: {value}\n\n");
    let mut sse = frame(json!({"type":"message_start","message":{"usage":response["usage"]}}));
    for (index, block) in response["content"].as_array().unwrap().iter().enumerate() {
        let mut start = block.clone();
        if block["type"] == "text" {
            start["text"] = json!("");
            start.as_object_mut().unwrap().remove("citations");
        }
        sse.push_str(&frame(
            json!({"type":"content_block_start","index":index,"content_block":start}),
        ));
        if block["type"] == "text" {
            sse.push_str(&frame(json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":block["text"]}})));
            for citation in block["citations"].as_array().into_iter().flatten() {
                sse.push_str(&frame(json!({"type":"content_block_delta","index":index,"delta":{"type":"citations_delta","citation":citation}})));
            }
        }
        sse.push_str(&frame(json!({"type":"content_block_stop","index":index})));
    }
    sse.push_str(&frame(json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":931,"server_tool_use":{"web_search_requests":1}}})));
    sse.push_str(&frame(json!({"type":"message_stop"})));
    let stub = Stub::start(Plan {
        completions: vec![Reply::WireSse(sse)],
        ..Plan::default()
    });
    let ego =
        || Ego::at(places.home_path(), places.workspace_path(), Some(&stub)).platform_sandbox();
    let assert_search = |finished: crate::support::Finished, searches: u64| {
        assert_eq!(finished.code, Some(0), "{}", finished.stderr);
        let report = finished.report();
        assert_eq!(report["ok"], true, "{report}");
        assert!(
            report["result"]
                .to_string()
                .contains("Claude Shannon was born on April 30, 1916"),
            "{report}"
        );
        assert!(
            report["result"].to_string().contains(
                "[Claude Shannon - Wikipedia](https://en.wikipedia.org/wiki/Claude_Shannon)"
            ),
            "{report}"
        );
        // Resume reports cumulative session usage, not only this invocation.
        assert_eq!(report["usage"]["web_search_requests"], searches, "{report}");
    };
    assert_search(
        ego()
            .args([
                "--connection",
                "account_b",
                "--session-id",
                SESSION,
                "--tools",
                "web_search",
                "--output",
                "json",
                "Search for Claude Shannon's birth date.",
            ])
            .run(),
        1,
    );

    // Labels are owner-editable configuration, not credential or session IDs.
    // Keep default A to make a lost durable B binding observable on resume.
    std::fs::write(
        &config_path,
        config.replace("name = \"Work\"", "name = \"Renamed Work\""),
    )
    .unwrap();
    assert_search(
        ego()
            .subcommand("resume")
            .args([
                SESSION,
                "--tools",
                "web_search",
                "--output",
                "json",
                "Search again using the same saved account.",
            ])
            .run(),
        2,
    );
    let logged_out = ego()
        .subcommand("auth")
        .args(["--connection", "account_a", "logout"])
        .run();
    assert_eq!(logged_out.code, Some(0), "{}", logged_out.stderr);
    let saved = store.read().unwrap();
    assert!(!saved.contains_key("connection:account_a"));
    let AuthEntry::OAuth {
        access,
        refresh,
        expires,
        ..
    } = &saved["connection:account_b"]
    else {
        panic!("B must remain OAuth")
    };
    assert_eq!(access.expose_secret_bytes(), b"sk-ant-oatTestOnly0513");
    assert!(refresh.is_none());
    assert!(expires.is_none());
    assert_search(
        ego()
            .subcommand("resume")
            .args([
                SESSION,
                "--tools",
                "web_search",
                "--output",
                "json",
                "Search once more after logging out A.",
            ])
            .run(),
        3,
    );

    let requests = stub.requests();
    let generations: Vec<_> = requests
        .iter()
        .filter(|request| request.path.ends_with("/messages"))
        .collect();
    assert_eq!(generations.len(), 3, "{requests:?}");
    for (index, request) in generations.iter().enumerate() {
        assert_eq!(
            request.headers.get("authorization").map(String::as_str),
            Some("Bearer sk-ant-oatTestOnly0513"),
            "{request:?}"
        );
        assert!(!request.headers.contains_key("x-api-key"), "{request:?}");
        let body: Value = serde_json::from_str(&request.body).unwrap();
        assert!(
            body["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["type"] == "web_search_20250305" && tool["name"] == "web_search"),
            "tools={} messages={}",
            body["tools"],
            body["messages"]
        );
        if index > 0 {
            let history = body["messages"].to_string();
            assert!(
                history.contains("https://en.wikipedia.org/wiki/Claude_Shannon"),
                "tools={} messages={}",
                body["tools"],
                body["messages"]
            );
            assert!(
                history.contains("web_search_result_location"),
                "tools={} messages={}",
                body["tools"],
                body["messages"]
            );
            assert!(
                history.contains("srvtoolu_01WYG3ziw53XMcoyKL4XcZmE"),
                "tools={} messages={}",
                body["tools"],
                body["messages"]
            );
        }
    }
    assert!(
        requests.iter().all(|request| request
            .headers
            .values()
            .all(|value| !value.contains("sk-ant-oatTestOnly0512"))),
        "{requests:?}"
    );
}

/// Catches: named-account composition exposes the unrelated legacy subscription
/// image backend; also protects legacy-only and explicit API-key image access.
#[test]
fn named_connections_never_offer_legacy_subscription_images() {
    use ego_cli::auth::{AuthEntry, AuthStore, StoredSecret};

    let places = Places::new();
    AuthStore::new(places.home_path())
        .put(
            "openai-codex",
            AuthEntry::OAuth {
                access: StoredSecret::new(b"synthetic-legacy-image-account".to_vec()),
                refresh: None,
                expires: None,
                scope_binding: None,
            },
        )
        .unwrap();
    // An owned terminal transport status, not a fabricated provider response.
    let stub = Stub::start(Plan {
        completions: vec![Reply::Status(400)],
        ..Plan::default()
    });
    let mut failures = Vec::new();
    for (case, named, explicit, api_key, expected_image) in [
        ("explicit named B", true, true, false, false),
        ("implicit single B", true, false, false, false),
        (
            "retired slot cannot offer images",
            false,
            false,
            false,
            false,
        ),
        ("explicit API image key", true, true, true, true),
    ] {
        let accounts = if named {
            "[connections.account_b]\nname='Work'\nvendor='anthropic'\naccess='key'\nkey_env='PROCESS_B_API_KEY'\n"
        } else {
            ""
        };
        // Windows cannot confine IP networking, so it refuses a configured `off`.
        let network = if cfg!(windows) { "" } else { "network='off'\n" };
        std::fs::write(
            places.home_path().join("config.toml"),
            format!("model='anthropic/claude-opus-5-5'\nthinking='low'\nsandbox='off'\n{network}{accounts}"),
        )
        .unwrap();
        let mut command = invocation(&places, &stub).platform_sandbox();
        if explicit {
            command = command.args(["--connection", "account_b"]);
        }
        if api_key {
            command = command.env("OPENAI_API_KEY", "synthetic-explicit-image-api-key");
        }
        let before = stub.requests().len();
        let finished = command
            .args(["--offline", "--output", "json", "Inspect available tools."])
            .run();
        assert_eq!(finished.code, Some(2), "{case}: {}", finished.stderr);
        assert!(
            finished.stderr.contains("the provider returned HTTP 400"),
            "{case}: {}",
            finished.stderr
        );
        let requests = stub.requests();
        assert_eq!(requests.len(), before + 1, "{case}: {requests:?}");
        let request = requests.last().unwrap();
        if named {
            assert_eq!(
                request.headers.get("x-api-key").map(String::as_str),
                Some(KEY_B),
                "{case}"
            );
        }
        let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
        let offered = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "image_gen");
        if offered != expected_image {
            failures.push(format!(
                "{case}: image_gen offered={offered}, expected={expected_image}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

fn configure(places: &Places, b_name: Option<&str>) {
    // These scenarios exercise named identity against an owned loopback stub.
    // Child network confinement has its own positive and negative controls.
    let b = b_name.map_or_else(String::new, |name| {
        format!(
            "\n[connections.account_b]\nname = \"{name}\"\nvendor = \"anthropic\"\naccess = \"key\"\nkey_env = \"PROCESS_B_API_KEY\"\n"
        )
    });
    std::fs::write(
        places.home_path().join("config.toml"),
        format!(
            "connection = \"account_a\"\nmodel = \"anthropic/claude-opus-5-5\"\nthinking = \"low\"\nsandbox = \"off\"\nnetwork = \"on\"\n\n[connections.account_a]\nname = \"Personal\"\nvendor = \"anthropic\"\naccess = \"key\"\nkey_env = \"PROCESS_A_API_KEY\"\n{b}"
        ),
    )
    .expect("write test-owned account configuration");
}

fn invocation(places: &Places, stub: &Stub) -> Ego {
    Ego::at(places.home_path(), places.workspace_path(), Some(stub))
        .platform_sandbox()
        .env("PROCESS_A_API_KEY", KEY_A)
        .env("PROCESS_B_API_KEY", KEY_B)
        .env("ANTHROPIC_API_KEY", AMBIENT_KEY)
        .env("CLAUDE_CODE_OAUTH_TOKEN", AMBIENT_TOKEN)
}

fn host(places: &Places, stub: &Stub) -> AcpHost {
    let mut host = AcpHost::adopt(
        invocation(places, stub)
            .subcommand("acp")
            .spawn_conversing(),
    );
    let initialized = host.initialize();
    assert!(initialized["result"].is_object(), "{initialized}");
    host
}

fn load(host: &mut AcpHost, places: &Places) -> serde_json::Value {
    host.request(
        "session/load",
        serde_json::json!({
            "sessionId": SESSION,
            "cwd": places.workspace_path(),
            "mcpServers": [],
        }),
    )
}

fn assert_b_request(stub: &Stub, count: usize, boundary: &str) {
    let requests = stub.requests();
    let generations: Vec<_> = requests
        .iter()
        .filter(|request| request.path.ends_with("/messages"))
        .collect();
    assert_eq!(generations.len(), count, "{boundary}: {requests:?}");
    for request in generations {
        assert_eq!(
            request.headers.get("x-api-key").map(String::as_str),
            Some(KEY_B),
            "{boundary}: {request:?}"
        );
        assert!(
            !request.headers.contains_key("authorization"),
            "{boundary}: ambient OAuth must not replace the explicit key: {request:?}"
        );
    }
    for request in &requests {
        for value in request.headers.values() {
            assert!(
                ![KEY_A, AMBIENT_KEY, AMBIENT_TOKEN]
                    .iter()
                    .any(|forbidden| value.contains(forbidden)),
                "{boundary}: another account or ambient credential crossed HTTP: {request:?}"
            );
        }
    }
}
