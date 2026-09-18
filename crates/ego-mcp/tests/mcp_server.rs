//! contract for the inbound, scoped MCP server (regression case).
//!
//! These tests deliberately exercise the injected controller seam. The MCP
//! adapter owns protocol validation, connection visibility, and sanitisation;
//! the core RunController remains the only lifecycle authority.

use std::sync::Arc;

use std::collections::BTreeSet;
use std::num::NonZeroUsize;

use ego_core::ceiling::{Budget, Ceilings};
use ego_core::config::{Mode, ModelRef};
use ego_core::generator::types::ThinkingLevel;
use ego_mcp::{
    McpRunSelectionPolicy, McpRunServerConfig, McpRunServerFactory, McpServerPort,
    McpServerToolCall,
};
use serde_json::{Value, json};

#[path = "support/run_controller.rs"]
mod run_fixture;

type Controller = run_fixture::Controller;

fn call(name: &str, body: Value) -> McpServerToolCall {
    McpServerToolCall::new(
        name.to_owned(),
        body.as_object().expect("object request").clone(),
        Some("client-058".to_owned()),
    )
    .expect("valid MCP call")
}

fn config() -> Arc<McpRunServerConfig> {
    let mut ceilings = Ceilings::default();
    ceilings.apply(&"100k".parse::<Budget>().expect("typed token ceiling"));
    ceilings.apply(&"10USD".parse::<Budget>().expect("typed money ceiling"));
    config_with_ceilings(ceilings)
}

fn config_with_ceilings(ceilings: Ceilings) -> Arc<McpRunServerConfig> {
    let config = McpRunServerConfig::new(
        McpRunSelectionPolicy::new(
            BTreeSet::from([run_fixture::root()]),
            BTreeSet::from([ModelRef("model/allowed".to_owned())]),
            ThinkingLevel::High,
            BTreeSet::from([Mode::Default]),
            ceilings,
            BTreeSet::from(["ci".to_owned()]),
        )
        .expect("valid MCP selection policy"),
        NonZeroUsize::new(2).expect("concurrency"),
        NonZeroUsize::new(4096).expect("resource bound"),
    )
    .expect("valid MCP server config");
    Arc::new(config)
}

fn server_with_ceilings(controller: Arc<Controller>, ceilings: Ceilings) -> impl McpServerPort {
    McpRunServerFactory::new(
        controller,
        config_with_ceilings(ceilings),
        run_fixture::ports(),
    )
    .new_connection()
}

fn factory(
    controller: Arc<Controller>,
) -> McpRunServerFactory<
    run_fixture::SessionStoreFixture,
    run_fixture::ExecutorFixture,
    run_fixture::ClockFixture,
    run_fixture::IdFixture,
> {
    McpRunServerFactory::new(controller, config(), run_fixture::ports())
}

fn factory_with_spawner(
    controller: Arc<Controller>,
) -> (
    McpRunServerFactory<
        run_fixture::SessionStoreFixture,
        run_fixture::ExecutorFixture,
        run_fixture::ClockFixture,
        run_fixture::IdFixture,
    >,
    Arc<run_fixture::ExecutionSpawner>,
) {
    let (ports, spawner) = run_fixture::ports_with_spawner();
    (
        McpRunServerFactory::new(controller, config(), ports),
        spawner,
    )
}

fn server(controller: Arc<Controller>) -> impl McpServerPort {
    factory(controller).new_connection()
}

async fn invoke(server: &dyn McpServerPort, name: &str, body: Value) -> Value {
    let result = server.call_tool(call(name, body)).await.expect("tool call");
    serde_json::from_str(&result.model_text()).expect("sanitized JSON result")
}

#[test]
fn advertised_surface_is_exactly_five_tools_and_three_resources() {
    let (controller, _executor) = run_fixture::mcp_controller_fixture();
    let server = server(controller);
    let tools = futures::executor::block_on(server.list_tools());
    let names: Vec<_> = tools.iter().map(|tool| tool.name.as_str()).collect();
    assert_eq!(names, ["run", "continue", "cancel", "status", "compact"]);

    let resources = futures::executor::block_on(server.list_resources());
    let uris: Vec<_> = resources
        .iter()
        .map(|resource| resource.uri.as_str())
        .collect();
    assert_eq!(
        uris,
        [
            "ego://capabilities",
            "ego://runs/{run_id}/summary",
            "ego://runs/{run_id}/artifacts/{artifact_id}",
        ]
    );
}

#[test]
fn forbidden_management_method_is_rejected_before_run_publication() {
    let (controller, executor) = run_fixture::mcp_controller_fixture();
    let server = server(controller);
    let error = futures::executor::block_on(server.call_tool(call(
        "initialize",
        json!({"request_id":"00000000-0000-0000-0000-000000000057"}),
    )))
    .expect_err("management methods are not run operations");
    assert!(error.to_string().contains("invalid MCP run tool name"));
    assert!(
        !executor
            .calls()
            .iter()
            .any(|call| matches!(call, run_fixture::ExecutorCall::Prepare))
    );
}

#[test]
fn capabilities_advertise_independent_money_and_token_budget_axes() {
    let (controller, _) = run_fixture::mcp_controller_fixture();
    let server = server(controller);
    let capabilities = futures::executor::block_on(server.read_resource("ego://capabilities"))
        .expect("capabilities resource");
    let value: Value = serde_json::from_str(&capabilities.text).expect("capabilities JSON");
    assert!(value["selection_policy"]["ceilings"]["tokens"].is_number());
    assert!(value["selection_policy"]["ceilings"]["money"].is_object());
}

#[test]
fn schemas_reject_unknown_fields_and_non_subtractive_authority() {
    let (controller, _) = run_fixture::mcp_controller_fixture();
    let server = server(controller);
    for (name, body) in [
        (
            "run",
            json!({"request_id":"00000000-0000-0000-0000-000000000058","root":run_fixture::ROOT,"prompt":"x","credentials":"secret"}),
        ),
        (
            "run",
            json!({"request_id":"00000000-0000-0000-0000-000000000059","root":run_fixture::ROOT,"prompt":"x","permission_rules":[]}),
        ),
        (
            "continue",
            json!({"request_id":"00000000-0000-0000-0000-000000000060","run_id":"bad","expected_revision":1,"prompt":"x","root":"/tmp"}),
        ),
        (
            "cancel",
            json!({"request_id":"00000000-0000-0000-0000-000000000061","run_id":"bad","credentials":"secret"}),
        ),
    ] {
        let error = futures::executor::block_on(server.call_tool(call(name, body)))
            .expect_err("invalid authority must be rejected");
        assert!(error.to_string().contains("invalid"));
    }
}

#[test]
fn run_enforces_root_model_budget_mode_and_concurrency_ceilings_before_controller() {
    let (controller, executor) = run_fixture::mcp_controller_fixture();
    let server = server(controller.clone());
    for body in [
        json!({"request_id":"00000000-0000-0000-0000-000000000062","root":"/tmp","prompt":"x"}),
        json!({"request_id":"00000000-0000-0000-0000-000000000063","root":run_fixture::ROOT,"prompt":"x","model":"model/other"}),
        json!({"request_id":"00000000-0000-0000-0000-000000000064","root":run_fixture::ROOT,"prompt":"x","budget":"101k"}),
        json!({"request_id":"00000000-0000-0000-0000-000000000065","root":run_fixture::ROOT,"prompt":"x","mode":"yolo"}),
    ] {
        let error = futures::executor::block_on(server.call_tool(call("run", body)))
            .expect_err("policy ceiling must refuse");
        assert!(error.to_string().contains("not allowed") || error.to_string().contains("ceiling"));
    }
    assert_eq!(
        executor
            .calls()
            .iter()
            .filter(|call| matches!(call, run_fixture::ExecutorCall::Prepare))
            .count(),
        0,
        "refusal precedes controller admission"
    );
}

#[test]
fn run_budget_axes_are_independent_and_currency_is_not_convertible() {
    let cases = [
        ("10USD", "1k"),
        ("100k", "11USD"),
        ("10USD", "1EUR"),
        ("100k", "101k"),
        ("10USD", "11USD"),
    ];
    for (ceiling, requested) in cases {
        let mut ceilings = Ceilings::default();
        ceilings.apply(&ceiling.parse::<Budget>().unwrap());
        let (controller, executor) = run_fixture::mcp_controller_fixture();
        let server = server_with_ceilings(controller, ceilings);
        let error = futures::executor::block_on(server.call_tool(call(
            "run",
            json!({"request_id":"00000000-0000-0000-0000-000000000058","root":run_fixture::ROOT,"prompt":"x","budget":requested}),
        )))
        .expect_err("a missing, wrong-currency, or excessive axis must refuse");
        assert!(error.to_string().contains("ceiling") || error.to_string().contains("not allowed"));
        assert!(
            !executor
                .calls()
                .iter()
                .any(|call| matches!(call, run_fixture::ExecutorCall::Prepare))
        );
    }
}

#[test]
fn run_continue_cancel_status_are_adapted_to_one_injected_controller() {
    let (controller, executor) = run_fixture::mcp_controller_fixture();
    let (factory, spawner) = factory_with_spawner(controller.clone());
    let server = factory.new_connection();
    let run = futures::executor::block_on(invoke(
        &server,
        "run",
        json!({
            "request_id":"00000000-0000-0000-0000-000000000066", "root":run_fixture::ROOT, "prompt":"hello"
        }),
    ));
    let run_id = run["run_id"].as_str().expect("run id");
    assert_eq!(run["status"], "running");
    executor.allow_effects();
    for future in spawner.take_pending() {
        futures::executor::block_on(future);
    }

    let continued = futures::executor::block_on(invoke(
        &server,
        "continue",
        json!({
            "request_id":"00000000-0000-0000-0000-000000000067", "run_id":run_id, "expected_revision":2, "prompt":"again"
        }),
    ));
    assert_eq!(continued["revision"], 3);
    let status = futures::executor::block_on(invoke(&server, "status", json!({"run_id":run_id})));
    assert_eq!(status["run_id"], run_id);
    assert_eq!(
        futures::executor::block_on(invoke(
            &server,
            "cancel",
            json!({
                "request_id":"00000000-0000-0000-0000-000000000068", "run_id":run_id, "expected_revision":3
            })
        ))["status"],
        "cancelling"
    );
    assert!(
        executor
            .calls()
            .iter()
            .any(|call| matches!(call, run_fixture::ExecutorCall::Submit))
    );
    assert!(
        executor
            .calls()
            .iter()
            .any(|call| matches!(call, run_fixture::ExecutorCall::Interrupt))
    );
}

#[test]
fn admitted_execution_is_driven_separately_and_propagates_all_selections() {
    let (controller, executor) = run_fixture::mcp_controller_fixture();
    let (factory, spawner) = factory_with_spawner(controller);
    let server = factory.new_connection();
    let run = futures::executor::block_on(invoke(
        &server,
        "run",
        json!({
            "request_id":"00000000-0000-0000-0000-000000000079",
            "root":run_fixture::ROOT, "model":"model/allowed",
            "thinking":"high", "mode":"default", "budget":"40k",
            "profile":"ci", "prompt":"select"
        }),
    ));
    assert_eq!(run["status"], "running");
    assert_eq!(spawner.pending(), 1);
    let selections = executor.prepared_selections();
    assert_eq!(selections.len(), 1);
    assert_eq!(
        selections[0].model().map(|model| model.0.as_str()),
        Some("model/allowed")
    );
    assert_eq!(
        selections[0].thinking(),
        Some(ego_core::generator::types::ThinkingLevel::High)
    );
    assert_eq!(selections[0].mode(), Some(ego_core::config::Mode::Default));
    assert_eq!(
        selections[0].budget().map(ToString::to_string).as_deref(),
        Some("40000 tokens")
    );
    assert_eq!(selections[0].profile(), Some("ci"));
    executor.allow_effects();
    for future in spawner.take_pending() {
        futures::executor::block_on(future);
    }
    let status =
        futures::executor::block_on(invoke(&server, "status", json!({"run_id":run["run_id"]})));
    assert_eq!(status["status"], "settled");
}

#[test]
fn fresh_controller_replay_attaches_shared_durable_run_and_recovers() {
    let store = Arc::new(run_fixture::SessionStoreFixture::default());
    let executor = Arc::new(run_fixture::ExecutorFixture::default());
    let controller1 = run_fixture::controller_from(store.clone(), executor.clone());
    let (factory1, spawner1) = factory_with_spawner(controller1);
    let request = json!({"request_id":"00000000-0000-0000-0000-000000000080","root":run_fixture::ROOT,"prompt":"recover"});
    let first =
        futures::executor::block_on(invoke(&factory1.new_connection(), "run", request.clone()));
    assert_eq!(first["status"], "running");
    assert_eq!(spawner1.pending(), 1);
    let controller2 = run_fixture::controller_from(store, executor.clone());
    let (factory2, _) = factory_with_spawner(controller2);
    let second = futures::executor::block_on(invoke(&factory2.new_connection(), "run", request));
    assert_eq!(second["run_id"], first["run_id"]);
    assert!(
        executor
            .calls()
            .iter()
            .any(|call| matches!(call, run_fixture::ExecutorCall::Recover))
    );
}

#[test]
fn executor_capacity_refuses_third_admission_without_server_queue() {
    let (controller, executor) = run_fixture::mcp_controller_fixture();
    executor.set_capacity(2);
    let server = server(controller);
    for value in [81, 82] {
        let result = futures::executor::block_on(invoke(
            &server,
            "run",
            json!({"request_id":format!("00000000-0000-0000-0000-0000000000{value}"),"root":run_fixture::ROOT,"prompt":"held"}),
        ));
        assert_eq!(result["status"], "running");
    }
    let third = futures::executor::block_on(server.call_tool(call("run", json!({"request_id":"00000000-0000-0000-0000-000000000083","root":run_fixture::ROOT,"prompt":"refused"}))));
    assert!(third.is_err());
}

#[test]
fn failed_execution_spawn_does_not_grant_connection_visibility() {
    let (controller, _) = run_fixture::mcp_controller_fixture();
    let (factory, spawner) = factory_with_spawner(controller);
    spawner.set_fail(true);
    let server = factory.new_connection();
    let error = futures::executor::block_on(server.call_tool(call("run", json!({"request_id":"00000000-0000-0000-0000-000000000084","root":run_fixture::ROOT,"prompt":"spawn fail"}))));
    assert!(error.is_err());
    assert!(futures::executor::block_on(server.read_resource("ego://runs/00000000-0000-0000-0000-000000000000.00000000-0000-0000-0000-000000000000/summary")).is_err());
}

#[test]
fn duplicate_mutations_replay_and_digest_mismatch_does_not_repeat_effects() {
    let (controller, executor) = run_fixture::mcp_controller_fixture();
    let server = server(controller.clone());
    let request = json!({"request_id":"00000000-0000-0000-0000-000000000069", "root":run_fixture::ROOT, "prompt":"once"});
    let first = futures::executor::block_on(invoke(&server, "run", request.clone()));
    let replay = futures::executor::block_on(invoke(&server, "run", request));
    assert_eq!(first, replay);
    assert_eq!(
        executor
            .calls()
            .iter()
            .filter(|call| matches!(call, run_fixture::ExecutorCall::Prepare))
            .count(),
        1
    );
    let mismatch = futures::executor::block_on(server.call_tool(call(
        "run",
        json!({
            "request_id":"00000000-0000-0000-0000-000000000069", "root":run_fixture::ROOT, "prompt":"twice"
        }),
    )))
    .expect_err("same id with another digest must refuse");
    assert!(mismatch.to_string().contains("request") && mismatch.to_string().contains("digest"));
    assert_eq!(
        executor
            .calls()
            .iter()
            .filter(|call| matches!(call, run_fixture::ExecutorCall::Prepare))
            .count(),
        1
    );
}

#[test]
fn lease_conflict_stale_revision_and_concurrency_are_truthful() {
    let (controller, executor) = run_fixture::mcp_controller_fixture();
    let server = server(controller.clone());
    let run = futures::executor::block_on(invoke(
        &server,
        "run",
        json!({
            "request_id":"00000000-0000-0000-0000-000000000070", "root":run_fixture::ROOT, "prompt":"x"
        }),
    ));
    let run_id = run["run_id"].as_str().expect("run id");
    let occupied = futures::executor::block_on(server.call_tool(call(
        "continue",
        json!({
            "request_id":"00000000-0000-0000-0000-000000000071", "run_id":run_id, "expected_revision":1, "prompt":"y"
        }),
    )))
    .expect_err("occupied lease");
    assert!(occupied.to_string().contains("already has an active turn"));
    let stale = futures::executor::block_on(server.call_tool(call(
        "continue",
        json!({
            "request_id":"00000000-0000-0000-0000-000000000072", "run_id":run_id, "expected_revision":0, "prompt":"y"
        }),
    )))
    .expect_err("stale revision");
    assert!(stale.to_string().contains("revision"));
    assert_eq!(
        executor
            .calls()
            .iter()
            .filter(|call| matches!(call, run_fixture::ExecutorCall::Prepare))
            .count(),
        1
    );
    assert_eq!(
        executor
            .calls()
            .iter()
            .filter(|call| matches!(call, run_fixture::ExecutorCall::Submit))
            .count(),
        0
    );
}

#[test]
fn resources_are_connection_scoped_authorized_redacted_bounded_and_absent_when_unknown() {
    let (controller, _executor) = run_fixture::mcp_controller_fixture();
    let server = server(controller.clone());
    let run = futures::executor::block_on(invoke(
        &server,
        "run",
        json!({
            "request_id":"00000000-0000-0000-0000-000000000073", "root":run_fixture::ROOT, "prompt":"x"
        }),
    ));
    let run_id = run["run_id"].as_str().expect("run id");
    let summary =
        futures::executor::block_on(server.read_resource(&format!("ego://runs/{run_id}/summary")))
            .expect("authorized summary");
    let projection: Value = serde_json::from_str(&summary.text).expect("summary JSON");
    let fields = projection
        .as_object()
        .expect("snapshot projection object")
        .keys()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    assert!(fields.is_subset(&std::collections::BTreeSet::from([
        "run_id".to_owned(),
        "revision".to_owned(),
        "root".to_owned(),
        "status".to_owned(),
        "settlement".to_owned(),
    ])));
    assert!(projection["run_id"].is_string());
    assert!(projection["revision"].is_number());
    assert!(projection["root"].is_string());
    assert!(summary.text.contains("run_id"));
    assert!(!summary.text.contains("secret-token"));
    assert!(!summary.text.contains("hidden reasoning"));
    assert!(summary.text.len() <= 4096);
    let artifact = futures::executor::block_on(
        server.read_resource(&format!("ego://runs/{run_id}/artifacts/output")),
    )
    .expect("authorized artifact");
    assert_eq!(artifact.mime_type, "application/octet-stream");
    assert!(
        artifact
            .blob
            .as_ref()
            .is_some_and(|bytes| bytes.len() <= 4096)
    );
    assert!(!artifact.text.contains("secret-token"));
    let absent = futures::executor::block_on(server.read_resource(
        "ego://runs/00000000-0000-0000-0000-000000000000.00000000-0000-0000-0000-000000000000/summary",
    ));
    assert!(absent.is_err(), "guessing a RunId discloses no metadata");
    let other_connection = factory(controller.clone()).new_connection();
    assert!(
        futures::executor::block_on(
            other_connection.read_resource(&format!("ego://runs/{run_id}/summary"))
        )
        .is_err()
    );
}

#[test]
fn disconnect_only_detaches_and_never_cancels_the_run() {
    let (controller, executor) = run_fixture::mcp_controller_fixture();
    let persistent = server(controller.clone());
    let run = futures::executor::block_on(invoke(
        &persistent,
        "run",
        json!({
            "request_id":"00000000-0000-0000-0000-000000000074", "root":run_fixture::ROOT, "prompt":"x"
        }),
    ));
    drop(persistent);
    assert!(
        !executor
            .calls()
            .iter()
            .any(|call| matches!(call, run_fixture::ExecutorCall::Interrupt))
    );
    assert_eq!(run["status"], "running");
}

#[test]
fn cancellation_before_and_during_work_and_unresolved_approval_are_explicit() {
    let (controller, executor) = run_fixture::mcp_controller_fixture();
    let (factory, spawner) = factory_with_spawner(controller);
    executor.block_effects();
    let server = factory.new_connection();
    let before = futures::executor::block_on(invoke(
        &server,
        "run",
        json!({
            "request_id":"00000000-0000-0000-0000-000000000075", "root":run_fixture::ROOT, "prompt":"x"
        }),
    ));
    assert_eq!(before["status"], "running");
    let cancelling = futures::executor::block_on(invoke(
        &server,
        "cancel",
        json!({"request_id":"00000000-0000-0000-0000-000000000076", "run_id":before["run_id"], "expected_revision":1}),
    ));
    assert_eq!(cancelling["status"], "cancelling");
    executor.allow_effects();
    for future in spawner.take_pending() {
        futures::executor::block_on(future);
    }

    executor.block_effects();
    executor.permission_required();
    let approval = futures::executor::block_on(invoke(
        &server,
        "run",
        json!({
            "request_id":"00000000-0000-0000-0000-000000000077", "root":run_fixture::ROOT, "prompt":"write file"
        }),
    ));
    assert_eq!(approval["status"], "running");
    executor.allow_effects();
    for future in spawner.take_pending() {
        futures::executor::block_on(future);
    }
    let status = futures::executor::block_on(invoke(
        &server,
        "status",
        json!({"run_id": approval["run_id"]}),
    ));
    assert_eq!(status["status"], "settled");
    assert!(!status.to_string().contains("input_required"));
}

#[test]
fn restart_reconstructs_interrupted_or_uncertain_without_claiming_running_and_retention_is_bounded()
{
    let (controller, executor) = run_fixture::mcp_controller_fixture();
    let first_connection = server(controller.clone());
    let request = json!({
        "request_id":"00000000-0000-0000-0000-000000000078",
        "root":run_fixture::ROOT,
        "prompt":"x"
    });
    let run = futures::executor::block_on(invoke(&first_connection, "run", request.clone()));
    let prepare_calls = executor
        .calls()
        .iter()
        .filter(|call| matches!(call, run_fixture::ExecutorCall::Prepare))
        .count();
    let restarted = server(controller.clone());
    let run_id = run["run_id"].as_str().expect("run id");
    assert!(
        futures::executor::block_on(restarted.call_tool(call("status", json!({"run_id":run_id}))))
            .is_err()
    );
    assert!(
        futures::executor::block_on(
            restarted.read_resource(&format!("ego://runs/{run_id}/summary"))
        )
        .is_err()
    );

    let replay = futures::executor::block_on(invoke(&restarted, "run", request));
    assert_eq!(replay["run_id"], run["run_id"]);
    assert_eq!(replay["revision"], run["revision"]);
    assert_eq!(
        executor
            .calls()
            .iter()
            .filter(|call| matches!(call, run_fixture::ExecutorCall::Prepare))
            .count(),
        prepare_calls,
        "authorized replay does not prepare or spawn a second effect"
    );
    let status = futures::executor::block_on(invoke(
        &restarted,
        "status",
        json!({"run_id":run["run_id"]}),
    ));
    assert_eq!(status["run_id"], run["run_id"]);
}
