//! ACP-hosted MCP through the packaged binary and one real ACP connection.

mod support;

use support::{AcpHost, Ego, Places, Plan, Reply, Stub, text, tool_call};

#[derive(Default)]
struct McpHost {
    connects: usize,
    disconnects: usize,
    calls: usize,
    methods: Vec<String>,
    updates: Vec<String>,
    resources: bool,
    subscribed: Vec<String>,
    workspace_text: Option<String>,
    inbox_text: Option<String>,
    fail_discovery: bool,
}

impl McpHost {
    fn until(&mut self, host: &mut AcpHost, wanted: u64) -> serde_json::Value {
        loop {
            let frame = host.read_frame();
            if frame["id"].as_u64() == Some(wanted) {
                return frame;
            }
            match frame["method"].as_str() {
                Some("mcp/connect") => {
                    assert_eq!(frame["params"]["serverId"], "tuicommander");
                    self.connects += 1;
                    host.write_frame(serde_json::json!({
                        "jsonrpc": "2.0", "id": frame["id"],
                        "result": {"connectionId": "host-connection"}
                    }));
                }
                Some("mcp/disconnect") => {
                    assert_eq!(frame["params"]["connectionId"], "host-connection");
                    self.disconnects += 1;
                    host.write_frame(serde_json::json!({
                        "jsonrpc": "2.0", "id": frame["id"], "result": {}
                    }));
                }
                Some("mcp/message") if frame.get("id").is_some() => {
                    assert_eq!(frame["params"]["connectionId"], "host-connection");
                    self.methods.push(
                        frame["params"]["method"]
                            .as_str()
                            .unwrap_or_default()
                            .to_owned(),
                    );
                    if self.fail_discovery && frame["params"]["method"] == "server/discover" {
                        host.write_frame(serde_json::json!({
                            "jsonrpc":"2.0","id":frame["id"],
                            "error":{"code":-32603,"message":"host discovery failed"}
                        }));
                        continue;
                    }
                    let result = match frame["params"]["method"].as_str() {
                        Some("server/discover") => serde_json::json!({
                            "resultType": "complete", "supportedVersions": ["2026-07-28"],
                            "capabilities": if self.resources { serde_json::json!({"tools":{"listChanged":false},"resources":{"subscribe":true}}) } else { serde_json::json!({"tools":{"listChanged":false}}) },
                            "ttlMs": 0, "cacheScope": "private"
                        }),
                        Some("resources/list") => serde_json::json!({
                            "resultType":"complete", "resources":[
                                {"uri":"tuic://workspace","name":"workspace"},
                                {"uri":"tuic://inbox","name":"inbox"}
                            ], "nextCursor":null,"ttlMs":0,"cacheScope":"private"
                        }),
                        Some("resources/subscribe") => {
                            self.subscribed.push(
                                frame["params"]["params"]["uri"]
                                    .as_str()
                                    .unwrap_or_default()
                                    .to_owned(),
                            );
                            serde_json::json!({})
                        }
                        Some("resources/read") => {
                            let uri = frame["params"]["params"]["uri"]
                                .as_str()
                                .unwrap_or_default();
                            let body = if uri == "tuic://workspace" {
                                self.workspace_text.as_deref().unwrap_or(r#"{"repos":["/workspace/repo"],"viewedRepo":"/workspace/repo"}"#)
                            } else {
                                self.inbox_text
                                    .as_deref()
                                    .unwrap_or(r#"{"messages":[],"nextSince":0}"#)
                            };
                            serde_json::json!({"resultType":"complete","contents":[{"uri":uri,"mimeType":"application/json","text":body}]})
                        }
                        Some("tools/list") => serde_json::json!({
                            "resultType": "complete", "tools": [{
                                "name":"search_tools", "description":"fixture search",
                                "inputSchema":{"type":"object","additionalProperties":false}
                            }], "nextCursor": null, "ttlMs": 0, "cacheScope": "private"
                        }),
                        Some("tools/call") => {
                            self.calls += 1;
                            assert_eq!(frame["params"]["params"]["name"], "search_tools");
                            serde_json::json!({
                                "resultType":"complete", "content":[{"type":"text","text":"lookup-complete"}],
                                "isError":false
                            })
                        }
                        _ => serde_json::json!({
                            "resultType":"complete", "resources":[], "prompts":[],
                            "nextCursor":null, "ttlMs":0, "cacheScope":"private"
                        }),
                    };
                    host.write_frame(serde_json::json!({
                        "jsonrpc":"2.0", "id":frame["id"], "result":result
                    }));
                }
                Some("session/request_permission") => {
                    panic!("a configured tool permission unexpectedly asked: {frame}")
                }
                other => self.updates.push(format!("{other:?}: {frame}")),
            }
        }
    }

    fn updated_workspace(&mut self, host: &mut AcpHost, text: &str) {
        self.workspace_text = Some(text.to_owned());
        host.write_frame(serde_json::json!({
            "jsonrpc":"2.0","method":"mcp/message","params":{
                "connectionId":"host-connection","method":"notifications/resources/updated",
                "params":{"uri":"tuic://workspace"}
            }
        }));
        loop {
            let frame = host.read_frame();
            if frame["method"] == "mcp/message"
                && frame["params"]["method"] == "resources/read"
                && frame["params"]["params"]["uri"] == "tuic://workspace"
            {
                host.write_frame(serde_json::json!({
                    "jsonrpc":"2.0","id":frame["id"],"result":{
                        "resultType":"complete","contents":[{
                            "uri":"tuic://workspace","mimeType":"application/json","text":text
                        }]
                    }
                }));
                break;
            }
        }
    }

    fn updated_inbox(&mut self, host: &mut AcpHost, text: &str) {
        self.inbox_text = Some(text.to_owned());
        host.write_frame(serde_json::json!({
            "jsonrpc":"2.0","method":"mcp/message","params":{
                "connectionId":"host-connection","method":"notifications/resources/updated",
                "params":{"uri":"tuic://inbox"}
            }
        }));
        loop {
            let frame = host.read_frame();
            if frame["method"] == "mcp/message"
                && frame["params"]["method"] == "resources/read"
                && frame["params"]["params"]["uri"] == "tuic://inbox"
            {
                host.write_frame(serde_json::json!({
                    "jsonrpc":"2.0","id":frame["id"],"result":{
                        "resultType":"complete","contents":[{
                            "uri":"tuic://inbox","mimeType":"application/json","text":text
                        }]
                    }
                }));
                break;
            }
        }
    }
}

#[test]
fn failed_acp_mcp_admission_disconnects_its_open_host_connection() {
    let places = Places::new();
    std::fs::write(
        places.home_path().join("config.toml"),
        "[profile.acp]\nmodel = \"ollama/qwen3\"\nthinking = \"off\"\nsandbox = \"off\"\nmode = \"default\"\n",
    ).expect("isolated profile configuration");
    let stub = Stub::start(Plan::default());
    let mut host = AcpHost::adopt(
        Ego::at(places.home_path(), places.workspace_path(), Some(&stub))
            .subcommand("acp")
            .args(["--profile", "acp"])
            .spawn_conversing(),
    );
    host.initialize();
    let mut mcp = McpHost {
        fail_discovery: true,
        ..McpHost::default()
    };
    let opened = host.send_request(
        "session/new",
        serde_json::json!({
            "cwd":places.workspace_path(),
            "mcpServers":[{"type":"acp","name":"tuicommander","serverId":"tuicommander"}]
        }),
    );
    let failure = mcp.until(&mut host, opened);
    assert!(failure["error"].is_object(), "{failure}");
    host.finish();
    assert_eq!(mcp.connects, 1);
    assert_eq!(
        mcp.disconnects, 1,
        "failed admission leaked the ACP host connection"
    );
}

#[test]
fn host_mail_arriving_during_a_tool_call_reaches_the_next_generation() {
    let places = Places::new();
    std::fs::write(
        places.home_path().join("config.toml"),
        "[permissions]\nallow = [\"tool(tuicommander/search_tools)\"]\n\n[profile.acp]\nmodel = \"ollama/qwen3\"\nthinking = \"off\"\nsandbox = \"off\"\nmode = \"default\"\n",
    ).expect("isolated profile configuration");
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call("held", "search_tools", "{}")),
            Reply::Sse(text("done")),
        ],
        ..Plan::default()
    });
    let mut host = AcpHost::adopt(
        Ego::at(places.home_path(), places.workspace_path(), Some(&stub))
            .subcommand("acp")
            .args(["--profile", "acp"])
            .spawn_conversing(),
    );
    host.initialize();
    let mut mcp = McpHost {
        resources: true,
        ..McpHost::default()
    };
    let opened = host.send_request(
        "session/new",
        serde_json::json!({
            "cwd":places.workspace_path(),
            "mcpServers":[{"type":"acp","name":"tuicommander","serverId":"tuicommander"}]
        }),
    );
    let opened = mcp.until(&mut host, opened);
    let session = opened["result"]["sessionId"].as_str().expect("ACP session");
    let prompt = host.send_request(
        "session/prompt",
        serde_json::json!({
            "sessionId":session,"prompt":[{"type":"text","text":"Search while checking mail"}]
        }),
    );
    let held = loop {
        let frame = host.read_frame();
        if frame["method"] == "mcp/message" && frame["params"]["method"] == "tools/call" {
            break frame;
        }
        assert_ne!(
            frame["id"].as_u64(),
            Some(prompt),
            "the prompt ended before the tool call: {frame}"
        );
    };
    let mail = r#"{"messages":[{"id":"mail-17","from":"maintainer","body":"review the new branch","timestamp":17}],"nextSince":17}"#;
    mcp.updated_inbox(&mut host, mail);
    host.write_frame(serde_json::json!({
        "jsonrpc":"2.0","id":held["id"],"result":{
            "resultType":"complete","content":[{"type":"text","text":"lookup-complete"}],"isError":false
        }
    }));
    let finished = mcp.until(&mut host, prompt);
    assert!(finished["result"].is_object(), "{finished}");
    host.finish();
    let generations = stub.generations();
    assert!(
        generations.get(1).is_some_and(|generation| {
            generation["messages"].as_array().is_some_and(|messages| {
                messages.iter().any(|message| {
                    message["role"] == "system"
                        && message["content"].as_str().is_some_and(|content| {
                            content.contains("mail-17") && content.contains("review the new branch")
                        })
                })
            })
        }),
        "new host mail was absent at the next tool boundary: {generations:?}"
    );
}

#[test]
fn acp_host_resources_are_subscribed_and_workspace_reaches_the_model() {
    let places = Places::new();
    std::fs::write(
        places.home_path().join("config.toml"),
        "[profile.acp]\nmodel = \"ollama/qwen3\"\nthinking = \"off\"\nsandbox = \"off\"\nmode = \"default\"\n",
    ).expect("isolated profile configuration");
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("ready")), Reply::Sse(text("updated"))],
        ..Plan::default()
    });
    let mut host = AcpHost::adopt(
        Ego::at(places.home_path(), places.workspace_path(), Some(&stub))
            .subcommand("acp")
            .args(["--profile", "acp"])
            .spawn_conversing(),
    );
    host.initialize();
    let mut mcp = McpHost {
        resources: true,
        ..McpHost::default()
    };
    let opened = host.send_request(
        "session/new",
        serde_json::json!({
            "cwd":places.workspace_path(),
            "mcpServers":[{"type":"acp","name":"tuicommander","serverId":"tuicommander"}]
        }),
    );
    let opened = mcp.until(&mut host, opened);
    let session = opened["result"]["sessionId"].as_str().expect("ACP session");
    let prompt = host.send_request(
        "session/prompt",
        serde_json::json!({
            "sessionId":session,"prompt":[{"type":"text","text":"What workspace is visible?"}]
        }),
    );
    let finished = mcp.until(&mut host, prompt);
    assert!(finished["result"].is_object(), "{finished}");
    mcp.updated_workspace(
        &mut host,
        r#"{"repos":["/workspace/other"],"viewedRepo":"/workspace/other"}"#,
    );
    let next = host.send_request(
        "session/prompt",
        serde_json::json!({
            "sessionId":session,"prompt":[{"type":"text","text":"What workspace is visible now?"}]
        }),
    );
    let finished = mcp.until(&mut host, next);
    assert!(finished["result"].is_object(), "{finished}");
    host.finish();
    mcp.subscribed.sort();
    assert_eq!(mcp.subscribed, ["tuic://inbox", "tuic://workspace"]);
    assert!(
        stub.generations().iter().any(|generation| {
            generation["messages"].as_array().is_some_and(|messages| {
                messages.iter().any(|message| {
                    message["role"] == "system"
                        && message["content"]
                            .as_str()
                            .is_some_and(|content| content.contains("/workspace/repo"))
                })
            })
        }),
        "the host workspace resource did not reach model context"
    );
    let generations = stub.generations();
    assert!(
        generations.last().is_some_and(|generation| {
            generation["messages"].as_array().is_some_and(|messages| {
                messages.iter().any(|message| {
                    message["role"] == "system"
                        && message["content"]
                            .as_str()
                            .is_some_and(|content| content.contains("/workspace/other"))
                })
            })
        }),
        "the updated workspace resource was absent from the next model context"
    );
}

#[test]
fn two_acp_mcp_tool_calls_share_one_host_connection() {
    let places = Places::new();
    std::fs::write(
        places.home_path().join("config.toml"),
        "[permissions]\nallow = [\"tool(tuicommander/search_tools)\"]\n\n[profile.acp]\nmodel = \"ollama/qwen3\"\nthinking = \"off\"\nsandbox = \"off\"\nmode = \"default\"\n",
    )
    .expect("isolated profile configuration");
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call("first", "search_tools", "{}")),
            Reply::Sse(text("first done")),
            Reply::Sse(tool_call("second", "search_tools", "{}")),
            Reply::Sse(text("second done")),
            Reply::Sse(tool_call("third", "search_tools", "{}")),
            Reply::Sse(text("third done")),
        ],
        ..Plan::default()
    });
    let mut host = AcpHost::adopt(
        Ego::at(places.home_path(), places.workspace_path(), Some(&stub))
            .subcommand("acp")
            .args(["--profile", "acp"])
            .spawn_conversing(),
    );
    let initialized = host.initialize();
    assert_eq!(
        initialized["result"]["agentCapabilities"]["mcpCapabilities"]["acp"],
        true
    );
    let mut mcp = McpHost::default();
    let opened = host.send_request(
        "session/new",
        serde_json::json!({
            "cwd": places.workspace_path(),
            "mcpServers": [{"type":"acp","name":"tuicommander","serverId":"tuicommander"}]
        }),
    );
    let opened = mcp.until(&mut host, opened);
    let session = opened["result"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("ACP server was refused: {opened}"));
    let mut results = Vec::new();
    for _ in 0..2 {
        let prompt = host.send_request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session,
                "prompt": [{"type":"text","text":"Search"}]
            }),
        );
        let finished = mcp.until(&mut host, prompt);
        assert!(finished["result"].is_object(), "{finished}");
        results.push(finished);
    }
    let closed = host.send_request("session/close", serde_json::json!({"sessionId": session}));
    let closed = mcp.until(&mut host, closed);
    assert!(closed["result"].is_object(), "{closed}");
    assert_eq!(
        mcp.disconnects, 1,
        "closing the session must disconnect the ACP MCP channel"
    );
    let reopened = host.send_request(
        "session/new",
        serde_json::json!({
            "cwd": places.workspace_path(),
            "mcpServers": [{"type":"acp","name":"tuicommander","serverId":"tuicommander"}]
        }),
    );
    let reopened = mcp.until(&mut host, reopened);
    let next_session = reopened["result"]["sessionId"]
        .as_str()
        .expect("reopened session");
    let prompt = host.send_request(
        "session/prompt",
        serde_json::json!({
            "sessionId": next_session, "prompt": [{"type":"text","text":"Search again"}]
        }),
    );
    let finished = mcp.until(&mut host, prompt);
    assert!(finished["result"].is_object(), "{finished}");
    let stderr = host.finish();
    assert_eq!(mcp.connects, 2, "one ACP MCP connection per session");
    assert_eq!(
        mcp.calls, 3,
        "all authorized calls reached the host; methods={:?}; results={results:?}; updates={:?}; stderr={stderr}",
        mcp.methods, mcp.updates
    );
}

#[test]
fn rejected_acp_mcp_permission_never_sends_the_tool_call() {
    let places = Places::new();
    std::fs::write(
        places.home_path().join("config.toml"),
        "[profile.acp]\nmodel = \"ollama/qwen3\"\nthinking = \"off\"\nsandbox = \"off\"\nmode = \"default\"\n",
    ).expect("isolated profile configuration");
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call("denied", "search_tools", "{}")),
            Reply::Sse(text("done")),
        ],
        ..Plan::default()
    });
    let mut host = AcpHost::adopt(
        Ego::at(places.home_path(), places.workspace_path(), Some(&stub))
            .subcommand("acp")
            .args(["--profile", "acp"])
            .spawn_conversing(),
    );
    host.initialize();
    let mut mcp = McpHost::default();
    let opened = host.send_request(
        "session/new",
        serde_json::json!({
            "cwd":places.workspace_path(),
            "mcpServers":[{"type":"acp","name":"tuicommander","serverId":"tuicommander"}]
        }),
    );
    let opened = mcp.until(&mut host, opened);
    let session = opened["result"]["sessionId"].as_str().expect("ACP session");
    let prompt = host.send_request(
        "session/prompt",
        serde_json::json!({
            "sessionId":session,"prompt":[{"type":"text","text":"Search"}]
        }),
    );
    let permission = loop {
        let frame = host.read_frame();
        if frame["method"] == "session/request_permission" {
            break frame;
        }
        assert_ne!(
            frame["params"]["method"], "tools/call",
            "tool call escaped before permission: {frame}"
        );
    };
    assert!(
        permission.to_string().contains("tuicommander/search_tools"),
        "{permission}"
    );
    host.write_frame(serde_json::json!({
        "jsonrpc":"2.0","id":permission["id"],
        "result":{"outcome":{"outcome":"selected","optionId":"reject-once"}}
    }));
    let finished = mcp.until(&mut host, prompt);
    assert!(
        finished["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("permission was refused")),
        "{finished}"
    );
    host.finish();
    assert_eq!(mcp.calls, 0, "a rejected ACP MCP call reached the host");
}

#[test]
fn cancelling_an_acp_mcp_call_cancels_the_outer_message_request() {
    let places = Places::new();
    std::fs::write(
        places.home_path().join("config.toml"),
        "[permissions]\nallow = [\"tool(tuicommander/search_tools)\"]\n\n[profile.acp]\nmodel = \"ollama/qwen3\"\nthinking = \"off\"\nsandbox = \"off\"\nmode = \"default\"\n",
    )
    .expect("isolated profile configuration");
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(tool_call("held", "search_tools", "{}"))],
        ..Plan::default()
    });
    let mut host = AcpHost::adopt(
        Ego::at(places.home_path(), places.workspace_path(), Some(&stub))
            .subcommand("acp")
            .args(["--profile", "acp"])
            .spawn_conversing(),
    );
    host.initialize();
    let mut mcp = McpHost::default();
    let opened = host.send_request(
        "session/new",
        serde_json::json!({
            "cwd": places.workspace_path(),
            "mcpServers": [{"type":"acp","name":"tuicommander","serverId":"tuicommander"}]
        }),
    );
    let opened = mcp.until(&mut host, opened);
    let session = opened["result"]["sessionId"].as_str().expect("ACP session");
    eprintln!("cancel fixture: session opened");
    let prompt = host.send_request(
        "session/prompt",
        serde_json::json!({
            "sessionId": session, "prompt": [{"type":"text","text":"Search"}]
        }),
    );
    let mut held = None;
    let mut cancelled = false;
    loop {
        let frame = host.read_frame();
        if frame["id"].as_u64() == Some(prompt) {
            assert!(frame["result"].is_object(), "{frame}");
            break;
        }
        match frame["method"].as_str() {
            Some("mcp/message") if frame["params"]["method"] == "tools/call" => {
                eprintln!("cancel fixture: tool call held");
                held = Some(frame["id"].clone());
                host.write_frame(serde_json::json!({
                    "jsonrpc":"2.0", "method":"session/cancel", "params":{"sessionId":session}
                }));
            }
            Some("$/cancel_request") => {
                eprintln!("cancel fixture: outer cancellation received");
                assert_eq!(
                    frame["params"]["requestId"],
                    held.clone().expect("held MCP request")
                );
                cancelled = true;
                host.write_frame(serde_json::json!({
                    "jsonrpc":"2.0", "id":held, "error":{"code":-32800,"message":"cancelled"}
                }));
            }
            Some("mcp/message") if frame["params"]["method"] == "notifications/cancelled" => {
                panic!(
                    "cancellation was sent as an inner MCP notification instead of cancelling the outer ACP request: {frame}"
                );
            }
            _ => {}
        }
    }
    host.finish();
    assert!(
        cancelled,
        "the ACP host never received cancellation of its mcp/message request"
    );
}
