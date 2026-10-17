use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock, ForkSessionRequest, InitializeRequest, LoadSessionRequest,
    NewSessionRequest, PromptRequest, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, ResumeSessionRequest, SessionNotification, TextContent,
};
use agent_client_protocol::{
    AcpAgent, AcpAgentConfig, Agent, Client, ConnectionTo, UntypedMessage,
};
use serde_json::json;
use std::time::Duration;
use uuid::Uuid;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cwd = std::env::var("ACP_WORKSPACE").expect("Run through isolated.sh");
    assert!(
        std::env::var_os("EGO_HOME").is_some(),
        "Run through isolated.sh"
    );
    let agent = AcpAgent::new(AcpAgentConfig::new(std::env::var("EGO_BIN")?).args([
        "acp",
        "--bare",
        "--no-tools",
        "--profile",
        "example",
        "-C",
        &cwd,
    ]));
    // AcpAgent's connection guard owns and tears down its child process.
    tokio::time::timeout(
        Duration::from_secs(20),
        Client
            .builder()
            .on_receive_notification(
                async |notification: SessionNotification, _cx| {
                    println!("update {:?}", notification.update);
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                async |_request: RequestPermissionRequest, responder, _cx| {
                    // Fail closed. A UI may select only an offered option_id.
                    responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Cancelled,
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(agent, |cx: ConnectionTo<Agent>| async move {
                let init = cx
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                assert_eq!(init.protocol_version, ProtocolVersion::V1);
                println!("initialize 1");
                let caps = serde_json::to_value(init.agent_capabilities)?;
                let mut request = NewSessionRequest::new(&cwd);
                request.meta = Some(serde_json::from_value(json!({"ego": {"bare": true}}))?);
                let session = cx.send_request(request).block_task().await?;
                let sid = session.session_id;
                println!("session/new ok");
                if caps["_meta"]["ego"]["hold"]["version"] == 1 {
                    let result = cx
                        .send_request(UntypedMessage::new(
                            "_ego/pause",
                            json!({"v": 1, "sessionId": sid, "requestId": Uuid::new_v4()}),
                        )?)
                        .block_task()
                        .await?;
                    println!("_ego/pause {}", result["state"]);
                    cx.send_request(UntypedMessage::new(
                        "_ego/resume",
                        json!({"v": 1, "sessionId": sid, "requestId": Uuid::new_v4()}),
                    )?)
                    .block_task()
                    .await?;
                }
                let result = cx
                    .send_request(PromptRequest::new(
                        sid.clone(),
                        vec![ContentBlock::Text(TextContent::new("Hello"))],
                    ))
                    .block_task()
                    .await;
                assert!(
                    result.is_err(),
                    "Unexpected successful generation in isolated mode"
                );
                println!("session/prompt refused: {}", result.unwrap_err());
                cx.send_notification(CancelNotification::new(sid.clone()))?;
                println!("session/cancel sent (idle)");
                if caps["loadSession"] == true {
                    cx.send_request(LoadSessionRequest::new(sid.clone(), &cwd))
                        .block_task()
                        .await?;
                    println!("session/load ok");
                }
                if caps["sessionCapabilities"]["resume"].is_object() {
                    cx.send_request(ResumeSessionRequest::new(sid.clone(), &cwd))
                        .block_task()
                        .await?;
                    println!("session/resume ok");
                }
                if caps["sessionCapabilities"]["fork"].is_object() {
                    cx.send_request(ForkSessionRequest::new(sid, &cwd))
                        .block_task()
                        .await?;
                    println!("session/fork ok");
                }
                Ok(())
            }),
    )
    .await??;
    Ok(())
}
