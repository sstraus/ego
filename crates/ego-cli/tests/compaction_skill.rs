//! public process demonstration for compaction and skill authority.
//!
//! One home/workspace pair is shared by the source run, compaction process, and
//! target resume process. The provider sees the requests, while the filesystem
//! mutation between source and target proves that the target uses its fresh
//! catalogue and the source's admitted instruction remains durable history.

mod support;

use serde_json::Value;
use support::{FIXTURE_SESSION_ID, MODEL, Places, Plan, Reply, Stub};

const RAW_ARGUMENTS: &str = "SOURCE-RAW-ARGUMENTS";
const TARGET_ARGUMENTS: &str = "TARGET-RAW-ARGUMENTS";
const CHECKPOINT_MARKER: &str = "CHECKPOINT-IMR-UNIQUE-MARKER";
const SOURCE_SKILL: &str = "alpha";
const TARGET_SKILL: &str = "beta";

fn skill_file(name: &str, body: &str) -> String {
    format!(
        "---\nname: {name}\ndescription: process fixture\nuser-invocable: true\ndisable-model-invocation: true\n---\n{body}\n"
    )
}

fn message_texts(request: &Value) -> impl Iterator<Item = &str> {
    request["messages"]
        .as_array()
        .expect("provider request has messages")
        .iter()
        .filter_map(|message| message["content"].as_str())
}

fn request_contains(request: &Value, expected: &str) -> bool {
    message_texts(request).any(|text| text.contains(expected))
}

fn checkpoint() -> String {
    format!(
        "# Objective\n{CHECKPOINT_MARKER}\n# Binding constraints and preferences\nPreserve durable authority.\n# Completed work and evidence\nSource skill was admitted.\n# Work in progress\nReopen the compacted target.\n# Decisions and rationale\nUse the public process seam.\n# Blockers and uncertainty\nNone.\n# Ordered next steps\nInvoke the fresh target skill.\n# Critical references\ncrates/ego-cli/tests/compaction_skill.rs"
    )
}

fn fixture_plan() -> Plan {
    Plan {
        installed: vec![MODEL.to_owned()],
        context_length: Some(32_768),
        completions: vec![
            Reply::Sse(support::text("source complete")),
            Reply::Sse(support::text(&checkpoint())),
            Reply::Sse(support::text("target complete")),
        ],
        ..Plan::default()
    }
}

#[test]
fn compaction_target_carries_old_skill_history_and_reopens_with_fresh_catalogue() {
    let stub = Stub::start(fixture_plan());
    let places = Places::new();
    let ego = places.ego(&stub);
    ego.home_file(
        "config.toml",
        "model = \"ollama/qwen3\"\nthinking = \"off\"\nsandbox = \"off\"\n",
    );
    ego.home_file(
        "skills/alpha/SKILL.md",
        &skill_file(SOURCE_SKILL, "SOURCE-A-RENDERED :: $ARGUMENTS"),
    );

    let source = places
        .ego(&stub)
        .conversing()
        .args(["--sandbox", "off", "--session-id", FIXTURE_SESSION_ID])
        .run_pty(&format!("/skills invoke {SOURCE_SKILL} {RAW_ARGUMENTS}"));
    assert_eq!(
        source.code,
        Some(0),
        "source process failed: {}",
        source.stderr
    );

    let source_request = stub
        .generations()
        .into_iter()
        .next()
        .expect("source typed skill contacted the provider");
    assert!(
        request_contains(&source_request, "Admitted skill instruction"),
        "source request did not carry an admitted instruction: {source_request}"
    );
    assert!(
        request_contains(
            &source_request,
            &format!("SOURCE-A-RENDERED :: {RAW_ARGUMENTS}"),
        ),
        "source request did not carry the rendered raw arguments: {source_request}"
    );

    ego.home_file(
        "skills/alpha/SKILL.md",
        &skill_file(SOURCE_SKILL, "SOURCE-A-MUTATED-CURRENT :: $ARGUMENTS"),
    );
    ego.home_file(
        "skills/beta/SKILL.md",
        &skill_file(TARGET_SKILL, "TARGET-B-FRESH :: $ARGUMENTS"),
    );

    // Catches: a cold compaction silently selects the current default rather
    // than the source generation's admitted model.
    ego.home_file(
        "config.toml",
        "model = \"ollama/not-the-source-model\"\nthinking = \"off\"\nsandbox = \"off\"\n",
    );
    let compacted = places
        .ego(&stub)
        .subcommand("compact")
        .args([
            FIXTURE_SESSION_ID,
            "--request-id",
            "0198c7f4-9c1f-7a2b-8c3d-4e5f60718294",
            "--output",
            "json",
        ])
        .run();
    assert_eq!(
        compacted.code,
        Some(0),
        "compaction process failed: {}",
        compacted.stderr
    );
    let compaction_request = stub
        .generations()
        .into_iter()
        .nth(1)
        .expect("compaction contacted the provider");
    assert!(
        request_contains(
            &compaction_request,
            &format!("SOURCE-A-RENDERED :: {RAW_ARGUMENTS}"),
        ),
        "compaction lost source's durable admitted skill material: {compaction_request}"
    );
    assert!(
        !request_contains(&compaction_request, "SOURCE-A-MUTATED-CURRENT"),
        "compaction used current source bytes instead of durable material: {compaction_request}"
    );

    ego.home_file(
        "config.toml",
        "model = \"ollama/qwen3\"\nthinking = \"off\"\nsandbox = \"off\"\n",
    );
    let target = compacted.report()["target_session_id"]
        .as_str()
        .expect("compact JSON has target_session_id")
        .to_owned();
    assert_ne!(target, FIXTURE_SESSION_ID);

    let resumed = places
        .ego(&stub)
        .subcommand("resume")
        .arg(&target)
        .args(["--sandbox", "off"])
        .run_pty(&format!("/skills invoke {TARGET_SKILL} {TARGET_ARGUMENTS}"));
    assert_eq!(
        resumed.code,
        Some(0),
        "target process failed: {}",
        resumed.stderr
    );

    let target_request = stub
        .generations()
        .into_iter()
        .nth(2)
        .expect("target skill contacted the provider");
    assert!(
        request_contains(&target_request, CHECKPOINT_MARKER),
        "target request lost the durable compaction checkpoint: {target_request}"
    );
    assert!(
        request_contains(
            &target_request,
            &format!("TARGET-B-FRESH :: {TARGET_ARGUMENTS}"),
        ),
        "target request did not render the fresh catalogue skill: {target_request}"
    );
    assert!(
        !request_contains(&target_request, "SOURCE-A-RENDERED")
            && !request_contains(&target_request, "SOURCE-A-MUTATED-CURRENT"),
        "target request carried stale source skill material: {target_request}"
    );
}

// Catches: cold compaction replaces a restricted source catalogue with defaults.
#[test]
fn cold_compaction_preserves_restricted_source_tools() {
    let stub = Stub::start(fixture_plan());
    let places = Places::new();
    places.ego(&stub).home_file(
        "config.toml",
        r#"model = "ollama/qwen3"
thinking = "off"
sandbox = "off"
"#,
    );
    let source = places
        .ego(&stub)
        .args([
            "--sandbox",
            "off",
            "--tools",
            "read",
            "--session-id",
            FIXTURE_SESSION_ID,
        ])
        .arg("Reply READY.")
        .run();
    assert_eq!(source.code, Some(0), "{}", source.stderr);
    let compacted = places
        .ego(&stub)
        .subcommand("compact")
        .arg(FIXTURE_SESSION_ID)
        .args(["--output", "json"])
        .run();
    assert_eq!(compacted.code, Some(0), "{}", compacted.stderr);
    let requests = stub.generations();
    assert_eq!(requests[0]["tools"].as_array().unwrap().len(), 1);
    assert_eq!(
        requests[1]["tools"], requests[0]["tools"],
        "cold compaction must retain the exact source tool prefix"
    );
}

// Catches: cold compaction uses current thinking instead of source admission.
#[test]
fn cold_compaction_recovers_source_thinking() {
    let stub = Stub::start(fixture_plan());
    let places = Places::new();
    places.ego(&stub).home_file(
        "config.toml",
        "model = \"ollama/qwen3\"\nthinking = \"off\"\nsandbox = \"off\"\n",
    );
    let source = places
        .ego(&stub)
        .args([
            "--sandbox",
            "off",
            "--thinking",
            "low",
            "--tools",
            "read",
            "--session-id",
            FIXTURE_SESSION_ID,
        ])
        .arg("Reply READY.")
        .run();
    assert_eq!(source.code, Some(0), "{}", source.stderr);
    let compacted = places
        .ego(&stub)
        .subcommand("compact")
        .arg(FIXTURE_SESSION_ID)
        .args(["--output", "json"])
        .run();
    assert_eq!(compacted.code, Some(0), "{}", compacted.stderr);
    let requests = stub.generations();
    assert_eq!(requests[0]["reasoning_effort"], "low");
    assert_eq!(
        requests[1]["reasoning_effort"], requests[0]["reasoning_effort"],
        "cold compaction must preserve source thinking"
    );
}

// Catches: a trailing Judge/Hook/Subagent admission replaces Main cache affinity.
#[test]
fn cold_compaction_ignores_trailing_judge_admission() {
    use ego_core::event::id::{GenerationId, Seq};
    use ego_core::generator::attempt::AttemptRef;
    use ego_core::store::codec::encode_record;
    let stub = Stub::start(fixture_plan());
    let places = Places::new();
    let source = places
        .ego(&stub)
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--thinking",
            "low",
            "--tools",
            "read",
            "--session-id",
            FIXTURE_SESSION_ID,
        ])
        .arg("Reply READY.")
        .run();
    assert_eq!(source.code, Some(0), "{}", source.stderr);
    let log = places
        .home_path()
        .join("sessions")
        .join(FIXTURE_SESSION_ID)
        .join("events.jsonl");
    let image = std::fs::read_to_string(&log).unwrap();
    let records: Vec<Value> = image
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let mut admission = records
        .iter()
        .rev()
        .find_map(|record| {
            let evidence = &record["payload"]["evidence"];
            (evidence["kind"] == "generation_admission").then(|| evidence.clone())
        })
        .expect("source admission exists");
    admission["role"] = serde_json::json!("judge");
    admission["thinking"] = serde_json::json!("off");
    admission["target"]["canonical_model"] = serde_json::json!("ollama/not-the-main-model");
    admission["target"]["wire_model"] = serde_json::json!("not-the-main-model");
    admission["attempt"] = serde_json::to_value(AttemptRef::first(GenerationId::new())).unwrap();
    let payload = serde_json::json!({"kind":"evidence", "evidence":admission});
    let seq: Seq = serde_json::from_value(serde_json::json!(
        records.last().unwrap()["seq"].as_u64().unwrap() + 1
    ))
    .unwrap();
    let encoded = encode_record(seq, &payload).unwrap();
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().append(true).open(log).unwrap();
    file.write_all(&encoded).unwrap();
    file.write_all(b"\n").unwrap();
    drop(file);
    let compacted = places
        .ego(&stub)
        .subcommand("compact")
        .arg(FIXTURE_SESSION_ID)
        .args(["--output", "json"])
        .run();
    assert_eq!(compacted.code, Some(0), "{}", compacted.stderr);
    let requests = stub.generations();
    assert_eq!(
        requests[1]["model"], requests[0]["model"],
        "trailing judge must not replace Main model"
    );
    assert_eq!(
        requests[1]["reasoning_effort"], "low",
        "trailing judge must not replace Main effort"
    );
    assert_eq!(requests[1]["tools"], requests[0]["tools"]);
}
