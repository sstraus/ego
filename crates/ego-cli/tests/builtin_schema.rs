//! Provider-boundary regression for the real builtin catalogue, without paid calls.
mod support;

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
        "plan",
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
            "plan",
            serde_json::json!({"operation":"replace","steps":[]}),
        ),
        (
            "plan",
            serde_json::json!({"operation":"step","id":"step-1","status":"blocked"}),
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
            "plan",
            json!({"operation":"replace"}),
            "missing field `steps`",
        ),
        (
            "plan",
            json!({"operation":"step","id":"step-1"}),
            "missing field `status`",
        ),
        (
            "plan",
            json!({"operation":"replace","steps":[],"id":"step-1"}),
            "unknown field `id`",
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
        serde_json::from_str(include_str!("fixtures/652-recorded-skill-calls.json")).unwrap();
    let mut failures = Vec::new();
    for fixture in fixtures.as_array().unwrap() {
        let arguments = &fixture["payload"];
        let stub = Stub::start(Plan {
            completions: vec![
                Reply::Sse(tool_call("recorded-skill", "skill", &arguments.to_string())),
                Reply::Sse(text("done")),
            ],
            ..Plan::default()
        });
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
        let requests = stub.generations();
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
