//! Re-admission answers and tool-session consent have separate lifetimes.
use agent_client_protocol::{Agent, Lines};
use agent_client_protocol_schema::v1;
use ego_acp::permission::AcpPermissionAdapter;
use ego_core::event::id::ItemId;
use ego_core::permit::{CanonPath, Escalation, Op, OpName, PermissionDecision, PermissionRequest};
use futures::channel::mpsc;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};

fn readmission() -> PermissionRequest {
    PermissionRequest::execution_policy_readmission(
        "Current authority may be wider".into(),
        CanonPath::new("/workspace"),
    )
}

fn tool() -> PermissionRequest {
    PermissionRequest::new(
        "read",
        vec![Op::Read {
            path: CanonPath::new("/workspace/secrets.txt"),
        }],
        Escalation::NoRuleCovers {
            op: OpName::Read,
            path: CanonPath::new("/workspace/secrets.txt"),
        },
    )
}

async fn answers(cases: Vec<(PermissionRequest, &'static str, PermissionDecision)>) {
    let replies: Vec<_> = cases.iter().map(|(_, answer, _)| *answer).collect();
    let (mut to_agent, to_agent_rx) = mpsc::unbounded::<String>();
    let (from_agent, mut from_agent_rx) = mpsc::unbounded::<String>();
    let transport = Lines::new(
        from_agent.sink_map_err(std::io::Error::other),
        to_agent_rx.map(Ok::<_, std::io::Error>),
    );
    let agent = Agent
        .builder()
        .connect_with(transport, async move |connection| {
            let adapter = AcpPermissionAdapter::live(v1::SessionId::new("readmission"), connection);
            let approver = adapter.approver();
            for (request, _, expected) in cases {
                assert_eq!(approver.decide(ItemId::new(), &request).await, expected);
            }
            Ok(())
        });
    let host = async move {
        for answer in replies {
            let frame: Value =
                serde_json::from_str(&from_agent_rx.next().await.expect("permission request"))
                    .unwrap();
            assert_eq!(frame["method"], "session/request_permission");
            if frame["params"]["toolCall"]["title"]
                .as_str()
                .unwrap()
                .starts_with("Re-admit execution policy:")
            {
                let options: Vec<_> = frame["params"]["options"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|option| option["optionId"].as_str().unwrap())
                    .collect();
                assert_eq!(options, ["allow-once", "reject-once"]);
            }
            to_agent
                .send(
                    json!({"jsonrpc":"2.0", "id":frame["id"],
                    "result":{"outcome":{"outcome":"selected", "optionId":answer}}})
                    .to_string(),
                )
                .await
                .unwrap();
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let (result, ()) = tokio::join!(agent, host);
        result.unwrap();
    })
    .await
    .expect("permission decisions must settle");
}

// Catches: re-admission offers or accepts a tool-session grant, auto-allowing later tools.
#[tokio::test]
async fn readmission_session_answer_cannot_approve_later_tools() {
    answers(vec![
        (readmission(), "allow-session", PermissionDecision::Deny),
        (tool(), "reject-once", PermissionDecision::Deny),
        (readmission(), "allow-once", PermissionDecision::Allow),
        (tool(), "reject-once", PermissionDecision::Deny),
    ])
    .await;
}

// Catches: a prior tool-session grant bypasses the explicit re-admission answer.
#[tokio::test]
async fn tool_session_grant_still_requires_readmission_answer() {
    answers(vec![
        (tool(), "allow-session", PermissionDecision::Allow),
        (readmission(), "reject-once", PermissionDecision::Deny),
    ])
    .await;
}
