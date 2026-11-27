//! Provider-boundary regression for the real builtin catalogue, without paid calls.
use crate::support;

use support::{Ego, Plan, Stub};

#[test]
fn builtin_catalogue_does_not_send_root_unions() {
    // Catches: one builtin root union makes the entire Anthropic-backed request fail.
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub);
    ego.home_file(
        "skills/schema-check/SKILL.md",
        "---\nname: schema-check\ndescription: Check schema fixture\n---\nCheck the schema.\n",
    );
    let finished = ego
        .args(["--model", "ollama/qwen3", "--sandbox", "off"])
        .arg("Describe the available tools without calling them")
        .run();
    assert_eq!(finished.code, Some(0), "{}", finished.stderr);
    let requests = stub.generations();
    let tools = requests[0]["tools"].as_array().expect("wire tools");
    for name in [
        "notebook_edit",
        "worklist",
        "skill",
        "ask_question",
        "task",
        "goal",
    ] {
        assert!(
            tools.iter().any(|tool| tool["function"]["name"] == name),
            "the real catalogue must include {name}"
        );
    }
    // Catches: a flat schema accidentally requires fields from every variant,
    // or excludes insert-at-start null IDs or the optional bare flag.
    for (name, arguments) in [
        (
            "goal",
            serde_json::json!({"operation":"create","objective":"verify artifact"}),
        ),
        (
            "goal",
            serde_json::json!({"operation":"cancel","goal_id":"00000000-0000-0000-0000-000000000001"}),
        ),
        (
            "notebook_edit",
            serde_json::json!({"path":"sample.ipynb","edit_mode":"replace","cell_id":"cell-1","new_source":"x"}),
        ),
        (
            "notebook_edit",
            serde_json::json!({"path":"sample.ipynb","edit_mode":"insert","cell_id":null,"cell_type":"markdown","new_source":"x"}),
        ),
        (
            "notebook_edit",
            serde_json::json!({"path":"sample.ipynb","edit_mode":"insert","cell_type":"code","new_source":"x"}),
        ),
        (
            "notebook_edit",
            serde_json::json!({"path":"sample.ipynb","edit_mode":"delete","cell_id":"cell-1"}),
        ),
        (
            "worklist",
            serde_json::json!({"operation":"replace","reason":null,"steps":[],"id":null,"status":null}),
        ),
        (
            "worklist",
            serde_json::json!({"operation":"step","reason":"external dependency","steps":null,"id":"step-1","status":"blocked"}),
        ),
        (
            "skill",
            serde_json::json!({"kind":"invoke","name":"schema-check","arguments":"","bare":true}),
        ),
        (
            "skill",
            serde_json::json!({"kind":"read_resource","invocation":"0198c7f4-9c1f-7a2b-8c3d-4e5f60718293","path":"SKILL.md"}),
        ),
    ] {
        let schema = &tools
            .iter()
            .find(|tool| tool["function"]["name"] == name)
            .unwrap()["function"]["parameters"];
        let validator = jsonschema::validator_for(schema).expect("valid advertised schema");
        assert!(
            validator.is_valid(&arguments),
            "{name} rejects valid arguments {arguments}"
        );
    }
    assert_worklist_wire_schemas(
        &tools
            .iter()
            .find(|tool| tool["function"]["name"] == "worklist")
            .unwrap()["function"],
    );
    let failures: Vec<_> = tools
        .iter()
        .filter_map(|tool| {
            let function = &tool["function"];
            let schema = &function["parameters"];
            (schema["type"] != "object"
                || ["oneOf", "allOf", "anyOf"]
                    .iter()
                    .any(|keyword| schema.get(keyword).is_some()))
            .then(|| function["name"].as_str().unwrap().to_owned())
        })
        .collect();
    assert!(
        failures.is_empty(),
        "incompatible builtin schemas: {failures:?}"
    );
}

#[test]
fn flat_builtin_schemas_do_not_weaken_variant_admission() {
    // Catches: flattening the advertised fields also makes variant-specific
    // required fields, discriminants, or unknown fields optional at dispatch.
    use serde_json::json;
    use support::{Reply, text, tool_call};

    let cases = [
        (
            "goal",
            json!({"objective":"untyped creation"}),
            "missing field `operation`",
        ),
        (
            "goal",
            json!({"operation":"cancel","objective":"wrong sibling"}),
            "cancel requires only goal_id",
        ),
        (
            "goal",
            json!({"operation":"create","objective":"  "}),
            "nonempty objective",
        ),
        (
            "notebook_edit",
            json!({"path":"sample.ipynb","edit_mode":"replace","new_source":"x"}),
            "missing field `cell_id`",
        ),
        (
            "notebook_edit",
            json!({"path":"sample.ipynb","edit_mode":"insert","cell_type":"raw","new_source":"x"}),
            "unknown variant `raw`",
        ),
        (
            "notebook_edit",
            json!({"path":"sample.ipynb","edit_mode":"delete","cell_id":"cell-1","new_source":"x"}),
            "unknown field `new_source`",
        ),
        (
            "worklist",
            json!({"operation":"replace","reason":null}),
            "missing field `steps`",
        ),
        (
            "worklist",
            json!({"operation":"step","reason":null,"steps":null,"id":"step-1"}),
            "missing field `status`",
        ),
        (
            "worklist",
            json!({"operation":"replace","reason":null,"steps":[],"id":"step-1","status":null}),
            "field `id` must be null for worklist operation `replace`",
        ),
        (
            "skill",
            json!({"kind":"invoke","arguments":""}),
            "missing field `name`",
        ),
        (
            "skill",
            json!({"kind":"read_resource","invocation":"0198c7f4-9c1f-7a2b-8c3d-4e5f60718293"}),
            "missing field `path`",
        ),
        (
            "skill",
            json!({"kind":"invoke","name":"schema-check","arguments":"","path":"SKILL.md"}),
            "field `path` does not belong to skill kind `invoke`",
        ),
    ];
    for (name, arguments, expected) in cases {
        let stub = Stub::start(Plan {
            completions: vec![
                Reply::Sse(tool_call("invalid-variant", name, &arguments.to_string())),
                Reply::Sse(text("The invalid call was rejected.")),
            ],
            ..Plan::default()
        });
        let ego = Ego::against(&stub);
        ego.home_file(
            "skills/schema-check/SKILL.md",
            "---\nname: schema-check\ndescription: Check schema fixture\n---\nCheck the schema.\n",
        );
        let finished = ego
            .args([
                "--model",
                "ollama/qwen3",
                "--sandbox",
                "off",
                "--mode",
                "yolo",
            ])
            .arg("Check variant admission")
            .run();
        assert_eq!(finished.code, Some(0), "{name}: {}", finished.stderr);
        let requests = stub.generations();
        let response = requests[1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| {
                message["role"] == "tool" && message["tool_call_id"] == "invalid-variant"
            })
            .expect("invalid call must return a tool diagnostic");
        assert!(
            response["content"].as_str().unwrap().contains(expected),
            "{name} {arguments}: {response}"
        );
    }
}

#[test]
fn recorded_flat_skill_calls_admit_instructions_with_empty_arguments() {
    // Catches: schema-advertised empty sibling fields or omitted arguments reject invoke.
    use support::{Reply, text, tool_call};

    let fixtures: serde_json::Value =
        serde_json::from_str(include_str!("../fixtures/652-recorded-skill-calls.json")).unwrap();
    let mut failures = Vec::new();
    for fixture in fixtures.as_array().unwrap() {
        let arguments = &fixture["payload"];
        let stub = Stub::start(
            Plan {
                completions: vec![
                    Reply::Sse(tool_call("recorded-skill", "skill", &arguments.to_string())),
                    Reply::Sse(text("done")),
                ],
                ..Plan::default()
            }
            .clear_skill_reviews(),
        );
        let ego = Ego::against(&stub);
        ego.home_file(
            "skills/amber-check/SKILL.md",
            "---\nname: amber-check\ndescription: Recorded call fixture\n---\nRECORDED-SKILL-BODY [$ARGUMENTS]\n",
        );
        let finished = ego
            .args([
                "--model",
                "ollama/qwen3",
                "--sandbox",
                "off",
                "--mode",
                "yolo",
            ])
            .arg("Replay the recorded skill call")
            .run();
        assert_eq!(finished.code, Some(0), "{}", finished.stderr);
        let requests = stub.conversation_generations();
        let admitted = requests[1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| {
                message["content"].as_str().is_some_and(|content| {
                    content.contains("Admitted skill instruction")
                        && content.contains("RECORDED-SKILL-BODY []")
                })
            });
        if !admitted {
            failures.push(format!("{arguments}: {}", requests[1]["messages"]));
        }
    }
    assert!(
        failures.is_empty(),
        "recorded calls not admitted: {failures:#?}"
    );
}

// Catches: the affected Responses mapping or another supported schema projection
// removes nullable inactive fields, re-expands an incompatible required set, or
// makes a nullable status enum reject null. The catalogue is from the real binary.
fn assert_worklist_wire_schemas(function: &serde_json::Value) {
    use ego_ai::wire::{
        anthropic_messages, gemini_interactions, kimi_openai, openai_codex_responses,
        openai_responses,
    };
    use ego_core::generator::types::{
        Conversation, GenRequest, Message, Role, ThinkingLevel, ToolDescriptor,
    };
    use serde_json::{Value, json};

    let request = GenRequest {
        model: "gpt-6.1-sol".into(),
        messages: Conversation::new(vec![Message::text(
            Role::User,
            "Track the authorized actions",
        )])
        .unwrap(),
        tools: vec![ToolDescriptor {
            name: "worklist".into(),
            description: function["description"].as_str().unwrap().into(),
            parameters: function["parameters"].clone(),
        }],
        thinking: ThinkingLevel::Medium,
        max_output_tokens: Some(4096),
        parallel_tool_calls: false,
        web_search: None,
        cache: None,
    };
    let codex: Value =
        serde_json::from_slice(&openai_codex_responses::request_body(&request).unwrap()).unwrap();
    let responses: Value =
        serde_json::from_slice(&openai_responses::request_body(&request).unwrap()).unwrap();
    let anthropic: Value =
        serde_json::from_slice(&anthropic_messages::request_body(&request).unwrap()).unwrap();
    let gemini: Value =
        serde_json::from_slice(&gemini_interactions::request_body(&request).unwrap()).unwrap();
    let kimi: Value =
        serde_json::from_slice(&kimi_openai::request_body(&request).unwrap()).unwrap();
    let replace = json!({"operation":"replace","reason":null,"steps":[
        {"id":"inspect","text":"Inspect the artifact","status":"pending","reason":null},
        {"id":"publish","text":"Await publication authority","status":"blocked","reason":"external dependency"}
    ],"id":null,"status":null});
    let step =
        json!({"operation":"step","reason":null,"steps":null,"id":"inspect","status":"completed"});
    let mut failures = Vec::new();
    for (profile, schema) in [
        ("ollama", &function["parameters"]),
        ("openai-codex/gpt-6.1-sol", &codex["tools"][0]["parameters"]),
        ("responses", &responses["tools"][0]["parameters"]),
        ("anthropic", &anthropic["tools"][0]["input_schema"]),
        ("gemini", &gemini["tools"][0]["parameters"]),
        ("kimi", &kimi["tools"][0]["function"]["parameters"]),
    ] {
        assert_eq!(schema["type"], "object", "{profile}: {schema}");
        assert_eq!(
            schema["required"],
            json!(["operation", "steps", "id", "status", "reason"]),
            "{profile}"
        );
        assert_eq!(schema["additionalProperties"], false, "{profile}");
        assert!(
            ["oneOf", "anyOf", "allOf"]
                .iter()
                .all(|key| schema.get(key).is_none()),
            "{profile}"
        );
        assert_eq!(
            schema["properties"]["steps"]["type"],
            json!(["array", "null"]),
            "{profile}"
        );
        assert_eq!(
            schema["properties"]["id"]["type"],
            json!(["string", "null"]),
            "{profile}"
        );
        assert_eq!(
            schema["properties"]["status"]["type"],
            json!(["string", "null"]),
            "{profile}"
        );
        let validator = jsonschema::validator_for(schema).expect("valid emitted worklist schema");
        for arguments in [
            &replace,
            &step,
            &json!({"operation":"replace","reason":null,"steps":[],"id":null,"status":null}),
        ] {
            if !validator.is_valid(arguments) {
                failures.push(format!("{profile} rejects canonical call {arguments}"));
            }
        }
        for canonical in [&replace, &step] {
            for key in ["operation", "steps", "id", "status", "reason"] {
                let mut missing = canonical.clone();
                missing.as_object_mut().unwrap().remove(key);
                if validator.is_valid(&missing) {
                    failures.push(format!("{profile} accepts missing {key}: {missing}"));
                }
            }
        }
        for invalid in [
            json!({"operation":"unknown","steps":[],"id":null,"status":null}),
            json!({"operation":"step","reason":null,"steps":null,"id":"inspect","status":"unknown"}),
            json!({"operation":"replace","reason":null,"steps":[],"id":null,"status":null,"extra":true}),
            json!({"operation":"replace","reason":null,"steps":[{"id":"a","text":"Action","status":"pending","extra":true,"reason":null}],"id":null,"status":null}),
        ] {
            if validator.is_valid(&invalid) {
                failures.push(format!(
                    "{profile} accepts malformed schema input {invalid}"
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// Catches: a schema-valid nullable worklist is rejected at real dispatch, or
// rejected model calls mutate durable work/goal state (including after reopen).
#[cfg(unix)]
#[test]
fn nullable_worklist_dispatch_replays_exact_actions_and_rejects_atomically() {
    use ego_core::{
        event::domain::EventKind,
        event::id::SessionId,
        plan::{PlanStep, PlanUpdate, StepStatus},
        session::ReplaySource,
        state::SessionState,
        store::session_store::SessionEntry,
    };
    use ego_tools::store::{JsonlStore, OsIo};
    use serde_json::json;
    use support::{FIXTURE_SESSION_ID, Places, Reply, text, tool_call};

    // These are independently specified model inputs, not a projection of PlanUpdate.
    let invalid = [
        ("malformed-json", "{".to_owned()),
        ("missing-reason", json!({"operation":"step","steps":null,"id":"inspect","status":"completed"}).to_string()),
        ("missing-nested-reason", json!({"operation":"replace","reason":null,"steps":[{"id":"a","text":"Action","status":"pending"}],"id":null,"status":null}).to_string()),
        ("forged-history", json!({"operation":"replace","reason":null,"steps":[],"id":null,"status":null,"started":["inspect"]}).to_string()),
        ("root-array", "[]".to_owned()),
        ("unknown-operation", json!({"operation":"merge","steps":null,"id":null,"status":null}).to_string()),
        ("missing-operation", json!({"steps":[],"id":null,"status":null}).to_string()),
        ("null-operation", json!({"operation":null,"steps":[],"id":null,"status":null}).to_string()),
        ("missing-replace-steps", json!({"operation":"replace","reason":null,"id":null,"status":null}).to_string()),
        ("missing-replace-id", json!({"operation":"replace","reason":null,"steps":[],"status":null}).to_string()),
        ("missing-replace-status", json!({"operation":"replace","reason":null,"steps":[],"id":null}).to_string()),
        ("missing-step-steps", json!({"operation":"step","reason":null,"id":"inspect","status":"completed"}).to_string()),
        ("missing-step-id", json!({"operation":"step","reason":null,"steps":null,"status":"completed"}).to_string()),
        ("missing-step-status", json!({"operation":"step","reason":null,"steps":null,"id":"inspect"}).to_string()),
        ("null-replace-steps", json!({"operation":"replace","reason":null,"steps":null,"id":null,"status":null}).to_string()),
        ("null-step-id", json!({"operation":"step","reason":null,"steps":null,"id":null,"status":"completed"}).to_string()),
        ("null-step-status", json!({"operation":"step","reason":null,"steps":null,"id":"inspect","status":null}).to_string()),
        ("wrong-steps-type", json!({"operation":"replace","reason":null,"steps":{},"id":null,"status":null}).to_string()),
        ("wrong-id-type", json!({"operation":"step","reason":null,"steps":null,"id":1,"status":"completed"}).to_string()),
        ("inactive-id", json!({"operation":"replace","reason":null,"steps":[],"id":"","status":null}).to_string()),
        ("inactive-status", json!({"operation":"replace","reason":null,"steps":[],"id":null,"status":"pending"}).to_string()),
        ("inactive-steps", json!({"operation":"step","reason":null,"steps":[],"id":"inspect","status":"completed"}).to_string()),
        ("invalid-status", json!({"operation":"step","reason":null,"steps":null,"id":"inspect","status":"done"}).to_string()),
        ("invalid-nested-status", json!({"operation":"replace","reason":null,"steps":[{"id":"a","text":"Action","status":"done","reason":null}],"id":null,"status":null}).to_string()),
        ("missing-nested-status", json!({"operation":"replace","reason":null,"steps":[{"id":"a","text":"Action"}],"id":null,"status":null}).to_string()),
        ("extra-root", json!({"operation":"replace","reason":null,"steps":[],"id":null,"status":null,"extra":true}).to_string()),
        ("extra-nested", json!({"operation":"replace","reason":null,"steps":[{"id":"a","text":"Action","status":"pending","extra":true,"reason":null}],"id":null,"status":null}).to_string()),
        ("unknown-id", json!({"operation":"step","reason":null,"steps":null,"id":"absent","status":"completed"}).to_string()),
        ("duplicate-id", json!({"operation":"replace","reason":null,"steps":[{"id":"a","text":"First","status":"pending","reason":null},{"id":"a","text":"Duplicate","status":"blocked","reason":"external dependency"}],"id":null,"status":null}).to_string()),
        ("empty-id", json!({"operation":"replace","reason":null,"steps":[{"id":"","text":"Action","status":"pending","reason":null}],"id":null,"status":null}).to_string()),
        ("control-id", json!({"operation":"replace","reason":null,"steps":[{"id":"a\n","text":"Action","status":"pending","reason":null}],"id":null,"status":null}).to_string()),
        ("empty-text", json!({"operation":"replace","reason":null,"steps":[{"id":"a","text":" ","status":"pending","reason":null}],"id":null,"status":null}).to_string()),
        ("reopen-terminal", json!({"operation":"step","reason":null,"steps":null,"id":"inspect","status":"pending"}).to_string()),
    ];
    let replace = r#"{"operation":"replace","reason":null,"steps":[{"id":"inspect","text":"Inspect the artifact","status":"pending","reason":null},{"id":"publish","text":"Await publication authority","status":"blocked","reason":"external dependency"}],"id":null,"status":null}"#;
    let step =
        r#"{"operation":"step","reason":null,"steps":null,"id":"inspect","status":"completed"}"#;
    let mut completions = vec![
        Reply::Sse(text("The independent checked objective is complete.")),
        Reply::Sse(tool_call("replace-valid", "worklist", replace)),
        Reply::Sse(tool_call("step-valid", "worklist", step)),
    ];
    // One turn stops after seven consecutive failures of one tool (loop guard), so the
    // rejected inputs are spread over turns that each stay below that limit.
    let chunks = invalid.chunks(6).collect::<Vec<_>>();
    for chunk in &chunks {
        completions.extend(
            chunk
                .iter()
                .map(|(name, arguments)| Reply::Sse(tool_call(name, "worklist", arguments))),
        );
        completions.push(Reply::Sse(text("Checked these worklist inputs.")));
    }
    completions.push(Reply::Sse(tool_call(
        "clear-valid",
        "worklist",
        r#"{"operation":"replace","reason":null,"steps":[],"id":null,"status":null}"#,
    )));
    completions.push(Reply::Sse(text("Finished checking worklist admission.")));
    let stub = Stub::start(Plan {
        completions,
        ..Plan::default()
    });
    let places = Places::new();
    let evidence = places.workspace_path().join("worklist-goal-evidence.json");
    let checker_script = places.file(
        "worklist-goal-check.sh",
        "cat > \"$1\"\nprintf '%s\\n' '{\"verdict\":\"achieved\",\"reason\":\"independent objective verified\"}'\n",
    );
    let checker = json!(["/bin/sh", checker_script, evidence]).to_string();
    let seeded = places
        .ego(&stub)
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--mode",
            "yolo",
            "--session-id",
            FIXTURE_SESSION_ID,
            "--goal",
            "Preserve the checked objective",
            "--goal-check-command",
            &checker,
            "--allow",
            "exec(/bin/sh *)",
            "Finish only the independent checked objective",
        ])
        .run();
    assert_eq!(seeded.code, Some(0), "{}", seeded.stderr);
    let load = || {
        // Reopen the production store rather than inspecting a fabricated reducer state.
        JsonlStore::new(
            places.home_path().join("sessions"),
            places.workspace_path(),
            Box::new(OsIo),
        )
        .entries(&FIXTURE_SESSION_ID.parse::<SessionId>().unwrap())
        .unwrap()
        .unwrap()
        .into_iter()
        .filter_map(|entry| match entry {
            SessionEntry::Event(event) => Some(event),
            _ => None,
        })
        .collect::<Vec<_>>()
    };
    let checked: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&evidence).unwrap_or_else(|error| {
            panic!("the real goal evaluator ran: {error}; {}", seeded.stderr)
        }))
        .unwrap();
    assert_eq!(checked["objective"], "Preserve the checked objective");
    let before = load();
    let baseline = SessionState::replay(before.clone());
    let goal = baseline.goal.clone().expect("real checked goal");
    assert_eq!(goal.objective, "Preserve the checked objective");
    assert_eq!(goal.state, ego_core::goal::GoalState::Achieved);
    for _ in 0..=chunks.len() {
        let finished = places
            .ego(&stub)
            .subcommand("resume")
            .args([
                FIXTURE_SESSION_ID,
                "--model",
                "ollama/qwen3",
                "--sandbox",
                "off",
                "--mode",
                "yolo",
                "--allow",
                "exec(/bin/sh *)",
                "Verify canonical and malformed worklist inputs without changing the checked objective",
            ])
            .run();
        assert_eq!(finished.code, Some(0), "{}", finished.stderr);
    }
    let events = load();
    let expected = vec![
        PlanStep {
            id: "inspect".into(),
            text: "Inspect the artifact".into(),
            status: StepStatus::Completed,
            reason: None,
        },
        PlanStep {
            id: "publish".into(),
            text: "Await publication authority".into(),
            status: StepStatus::Blocked,
            reason: Some("external dependency".into()),
        },
    ];
    let mut seen = std::collections::BTreeSet::new();
    let mut failures = Vec::new();
    for (index, event) in events.iter().enumerate().skip(before.len()) {
        if let EventKind::ToolCallCompleted { ok, .. } = &event.kind {
            let replay = SessionState::replay(events[..=index].iter().cloned());
            let item = &replay.items[&event.env.item.expect("tool result item")];
            let id = item.call_id.as_deref().expect("provider call id");
            if invalid.iter().any(|(name, _)| *name == id) {
                seen.insert(id.to_owned());
                if *ok || replay.plan.steps != expected || replay.goal.as_ref() != Some(&goal) {
                    failures.push(format!(
                        "{id}: ok={ok}, plan={:?}, goal={:?}",
                        replay.plan, replay.goal
                    ));
                }
            } else if id == "replace-valid" || id == "step-valid" || id == "clear-valid" {
                seen.insert(id.to_owned());
                assert!(*ok, "{id}: {:?}", item.tool_result);
                let mut actions = expected.clone();
                if id == "replace-valid" {
                    actions[0].status = StepStatus::Pending;
                }
                if id == "clear-valid" {
                    actions.clear();
                }
                assert_eq!(replay.plan.steps, actions, "{id}");
                assert_eq!(replay.goal.as_ref(), Some(&goal), "{id}");
            }
        }
    }
    for (name, _) in &invalid {
        if !seen.contains(*name) {
            failures.push(format!("missing real rejected tool completion {name}"));
        }
    }
    for name in ["replace-valid", "step-valid", "clear-valid"] {
        assert!(seen.contains(name), "missing {name}");
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    let updates = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::PlanUpdated { update } => Some(update),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        updates,
        vec![
            &PlanUpdate::Replace {
                steps: vec![
                    PlanStep {
                        status: StepStatus::Pending,
                        ..expected[0].clone()
                    },
                    expected[1].clone()
                ]
            },
            &PlanUpdate::Step {
                id: "inspect".into(),
                status: StepStatus::Completed,
                reason: None,
            },
            &PlanUpdate::Replace { steps: vec![] },
        ]
    );
    let final_state = SessionState::replay(events);
    assert!(final_state.plan.steps.is_empty());
    assert_eq!(final_state.goal, Some(goal));
}
