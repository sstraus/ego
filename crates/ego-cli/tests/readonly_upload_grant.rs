#![cfg(unix)]

mod support;

use serde_json::json;
use support::{AcpHost, Places, Plan, Reply, Stub, text, tool_call};

fn configure(places: &Places) {
    std::fs::write(
        places.home_path().join("config.toml"),
        "model='ollama/qwen3'\nthinking='off'\nroots=[]\nsandbox='workspace'\nmode='yolo'\n",
    )
    .unwrap();
}

fn call(id: &str, tool: &str, params: serde_json::Value) -> Reply {
    Reply::Sse(tool_call(id, tool, &params.to_string()))
}

// Catches: launch authority is dropped, or opens writes and executable uploads.
#[test]
fn grant_is_read_only() {
    let uploads = tempfile::tempdir().unwrap();
    let uploads = uploads.path().canonicalize().unwrap();
    let source = uploads.join("attachment.txt");
    let program = uploads.join("uploaded-program");
    let places = Places::new();
    let marker = places.workspace_path().join("executed");
    std::fs::write(&source, "UPLOAD-READ-CONTENT").unwrap();
    std::fs::write(
        &program,
        format!("#!/bin/sh\nprintf ran > '{}'\n", marker.display()),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    configure(&places);
    let stub = Stub::start(Plan {
        completions: vec![
            call("read_upload", "read", json!({"path": source})),
            call(
                "write_upload",
                "write",
                json!({"path": source, "content": "OVERWRITTEN"}),
            ),
            call(
                "exec_upload",
                "exec",
                json!({"argv": [program], "timeout_ms": 1000}),
            ),
            Reply::Sse(text("Finished.")),
        ],
        ..Plan::default()
    });
    let finished = places
        .ego(&stub)
        .args([
            "--read-only-dir",
            uploads.to_str().unwrap(),
            "Inspect upload",
        ])
        .run();
    assert_eq!(finished.code, Some(0), "{}", finished.stderr);
    let generations = stub.generations();
    assert!(generations[1].to_string().contains("UPLOAD-READ-CONTENT"));
    assert_eq!(
        std::fs::read_to_string(&source).unwrap(),
        "UPLOAD-READ-CONTENT"
    );
    assert!(!marker.exists(), "the uploaded executable ran");
    let exec_result = generations[3]["messages"]
        .as_array()
        .unwrap()
        .iter()
        // A refreshed runtime context message can follow the tool result.
        .find(|message| message["role"] == "tool" && message["tool_call_id"] == "exec_upload")
        .expect("the uploaded executable refusal reaches the next generation")
        .to_string();
    let refusal = if cfg!(target_os = "macos") {
        "sandbox denied exec"
    } else {
        "error"
    };
    assert!(
        exec_result.contains(refusal),
        "exec must return a refusal: {exec_result}"
    );
}

// Catches: an attachment subtree under writable authority aborts CLI startup.
#[test]
fn nested_launch_grant_starts_and_preserves_builtin_read_only_authority() {
    for (sandbox, external_root) in [("workspace", false), ("off", false), ("workspace", true)] {
        let places = Places::new();
        configure(&places);
        let external = tempfile::tempdir().unwrap();
        let external = external.path().canonicalize().unwrap();
        let writable = if external_root {
            external.as_path()
        } else {
            places.workspace_path()
        };
        if external_root || sandbox == "off" {
            std::fs::write(places.home_path().join("config.toml"), format!(
            "model='ollama/qwen3'\nthinking='off'\nsandbox='{sandbox}'\nmode='yolo'\nroots=[{{path='{}',access='read-write'}}]\n",
            writable.display()
        )).unwrap();
        }
        let uploads = writable.join("attachments");
        std::fs::create_dir(&uploads).unwrap();
        let uploads = uploads.canonicalize().unwrap();
        let source = uploads.join("attachment.txt");
        let sibling = writable.join("sibling.txt");
        std::fs::write(&source, "NESTED-UPLOAD").unwrap();
        let stub = Stub::start(Plan {
            completions: vec![
                call("read_upload", "read", json!({"path": source})),
                call(
                    "write_upload",
                    "write",
                    json!({"path": source, "content": "OVERWRITTEN"}),
                ),
                call(
                    "write_sibling",
                    "write",
                    json!({"path": sibling, "content": "SIBLING-WRITE"}),
                ),
                Reply::Sse(text("Finished.")),
            ],
            ..Plan::default()
        });
        let finished = places
            .ego(&stub)
            .args([
                "--read-only-dir",
                uploads.to_str().unwrap(),
                "Inspect nested upload",
            ])
            .run();
        assert_eq!(finished.code, Some(0), "{}", finished.stderr);
        assert!(stub.generations()[1].to_string().contains("NESTED-UPLOAD"));
        assert_eq!(std::fs::read(&source).unwrap(), b"NESTED-UPLOAD");
        assert_eq!(std::fs::read(&sibling).unwrap(), b"SIBLING-WRITE");
        #[cfg(target_os = "linux")]
        if sandbox != "off" {
            assert!(
                finished
                    .stderr
                    .contains("writable ancestor wins for exec children"),
                "{}",
                finished.stderr
            );
        }
    }
}

// Catches: ACP accepts additionalDirectories but fails to carry reads into a turn.
#[test]
fn acp_additional_directories_become_read_roots() {
    for method in [
        "session/new",
        "session/load",
        "session/resume",
        "session/fork",
    ] {
        let uploads = tempfile::tempdir().unwrap();
        let uploads = uploads.path().canonicalize().unwrap();
        let source = uploads.join("attachment.txt");
        std::fs::write(&source, "ACP-UPLOAD-CONTENT").unwrap();
        let places = Places::new();
        configure(&places);
        let stub = Stub::start(Plan {
            completions: vec![
                call("read_upload", "read", json!({"path": source})),
                Reply::Sse(text("Read.")),
            ],
            ..Plan::default()
        });
        let mut host = AcpHost::adopt(places.ego(&stub).subcommand("acp").spawn_conversing());
        host.initialize();
        let source_session = (method != "session/new").then(|| {
            let created = host.request("session/new", json!({"cwd": places.workspace_path(), "mcpServers": [], "additionalDirectories": [uploads]}));
            created["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("{created}")).to_owned()
        });
        let mut params = json!({"cwd": places.workspace_path(), "mcpServers": [], "additionalDirectories": [uploads]});
        if let Some(session) = &source_session {
            params["sessionId"] = json!(session);
        }
        let response = host.request(method, params);
        assert!(response["result"].is_object(), "{method}: {response}");
        if method == "session/new" {
            assert_eq!(
                response["result"]["_meta"]["ego"]["readOnlyDirectory"],
                json!(uploads),
                "{response}"
            );
        }
        let session = response["result"]["sessionId"]
            .as_str()
            .or(source_session.as_deref())
            .unwrap_or_else(|| panic!("{response}"));
        let response = host.request(
            "session/prompt",
            json!({"sessionId": session, "prompt": [{"type": "text", "text": "Read upload"}]}),
        );
        assert!(response["result"].is_object(), "{method}: {response}");
        host.finish();
        assert!(
            stub.generations()[1]
                .to_string()
                .contains("ACP-UPLOAD-CONTENT"),
            "{method}"
        );
    }
}

// Catches: .. at launch or a symlink under the grant exposes sibling contents.
#[test]
fn grant_cannot_widen_to_parent_or_symlink_escape() {
    let scratch = tempfile::tempdir().unwrap();
    let scratch = scratch.path().canonicalize().unwrap();
    let uploads = scratch.join("uploads");
    std::fs::create_dir(&uploads).unwrap();
    std::fs::write(uploads.join("allowed"), "ALLOWED-UPLOAD").unwrap();
    std::fs::write(scratch.join("sibling"), "SIBLING-SECRET").unwrap();
    std::fs::write(uploads.join(".env.synthetic"), "FLOOR-SECRET").unwrap();
    std::os::unix::fs::symlink(scratch.join("sibling"), uploads.join("escape")).unwrap();
    let places = Places::new();
    configure(&places);
    let stub = Stub::start(Plan {
        completions: vec![
            call("allowed", "read", json!({"path": uploads.join("allowed")})),
            call("escape", "read", json!({"path": uploads.join("escape")})),
            call(
                "parent",
                "read",
                json!({"path": uploads.join("../sibling")}),
            ),
            call(
                "floor",
                "read",
                json!({"path": uploads.join(".env.synthetic")}),
            ),
            Reply::Sse(text("Finished.")),
        ],
        ..Plan::default()
    });
    let finished = places
        .ego(&stub)
        .args([
            "--read-only-dir",
            uploads.to_str().unwrap(),
            "Inspect upload",
        ])
        .run();
    assert_eq!(finished.code, Some(3), "{}", finished.stderr);
    let generations = stub.generations();
    assert!(generations[1].to_string().contains("ALLOWED-UPLOAD"));
    assert!(
        !serde_json::to_string(&generations)
            .unwrap()
            .contains("SIBLING-SECRET")
    );
    assert!(
        !serde_json::to_string(&generations)
            .unwrap()
            .contains("FLOOR-SECRET")
    );
    let invalid = uploads.join("..");
    let refused = places
        .ego(&stub)
        .args(["--read-only-dir", invalid.to_str().unwrap(), "Read parent"])
        .run();
    assert_eq!(refused.code, Some(2), "{}", refused.stderr);
    assert!(refused.stderr.contains("read-only"), "{}", refused.stderr);
    let alias = scratch.join("upload-alias");
    std::os::unix::fs::symlink(&uploads, &alias).unwrap();
    let refused = places
        .ego(&stub)
        .args(["--read-only-dir", alias.to_str().unwrap(), "Read alias"])
        .run();
    assert_eq!(refused.code, Some(2), "{}", refused.stderr);
    assert!(refused.stderr.contains("canonical"), "{}", refused.stderr);
}

// Catches: measuring a clamped upload policy loses the process ceiling and refuses the prompt.
#[test]
fn upload_grant_with_narrower_host_sandbox_ceiling_reads_on_prompt() {
    let places = Places::new();
    let uploads = tempfile::tempdir().unwrap();
    let uploads = uploads.path().canonicalize().unwrap();
    let source = uploads.join("attachment.txt");
    std::fs::write(&source, "CEILING-UPLOAD-CONTENT").unwrap();
    std::fs::write(
        places.home_path().join("config.toml"),
        format!(
            "model='ollama/qwen3'\nthinking='off'\nroots=[]\nsandbox='off'\n[profile.host]\nsandbox='workspace'\nroots=[{{path='{}',access='read'}}]\n",
            uploads.display()
        ),
    )
    .unwrap();
    let stub = Stub::start(Plan {
        completions: vec![
            call("read_upload", "read", json!({"path": source})),
            Reply::Sse(text("Read.")),
        ],
        ..Plan::default()
    });
    let mut host = AcpHost::adopt(
        places
            .ego(&stub)
            .subcommand("acp")
            .args(["--sandbox", "off"])
            .spawn_conversing(),
    );
    host.initialize();
    let opened = host.request(
        "session/new",
        json!({
            "cwd": places.workspace_path(), "mcpServers": [], "additionalDirectories": [uploads],
            "_meta": {"ego": {"ceilingProfile": "host"}},
        }),
    );
    let session = opened["result"]["sessionId"].as_str().unwrap();
    let response = host.request(
        "session/prompt",
        json!({"sessionId": session, "prompt": [{"type": "text", "text": "Read upload"}]}),
    );
    assert!(response["result"].is_object(), "{response}");
    host.finish();
    assert!(
        stub.generations()[1]
            .to_string()
            .contains("CEILING-UPLOAD-CONTENT")
    );
}

// Catches: a launch grant bypasses system roots ceilings.
#[test]
fn grant_respects_system_roots_ceilings() {
    let places = Places::new();
    configure(&places);
    let stub = Stub::start(Plan::default());
    let uploads = tempfile::tempdir().unwrap();
    let uploads = uploads.path().canonicalize().unwrap();
    std::fs::write(
        places.home_path().join("config.toml"),
        "model='ollama/qwen3'\nthinking='off'\nroots=[]\n[profile.host]\nroots=[]\n",
    )
    .unwrap();
    let mut host = AcpHost::adopt(places.ego(&stub).subcommand("acp").spawn_conversing());
    host.initialize();
    let refused = host.request(
        "session/new",
        json!({
            "cwd": places.workspace_path(), "mcpServers": [], "additionalDirectories": [uploads],
            "_meta": {"ego": {"ceilingProfile": "host"}},
        }),
    );
    assert!(refused["error"].is_object(), "{refused}");
    let listed = host.request("session/list", json!({"cwd": places.workspace_path()}));
    assert_eq!(listed["result"]["sessions"], json!([]), "{listed}");
    host.finish();
}
