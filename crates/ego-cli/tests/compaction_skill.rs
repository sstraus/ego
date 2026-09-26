//! regression case's public process demonstration for compaction and skill authority.
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
const CHECKPOINT_MARKER: &str = "REGRESSION-MARKER-UNIQUE-MARKER";
const SOURCE_SKILL: &str = "alpha";
const TARGET_SKILL: &str = "beta";

fn skill_file(name: &str, body: &str) -> String {
    format!(
        "---\nname: {name}\ndescription: regression case process fixture\nuser-invocable: true\ndisable-model-invocation: true\n---\n{body}\n"
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
