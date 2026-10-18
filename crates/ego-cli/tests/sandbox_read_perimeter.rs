#![cfg(unix)]

mod support;

use serde_json::json;
use support::{Places, Plan, Reply, Stub, text, tool_call};

fn call(id: &str, tool: &str, params: serde_json::Value) -> Reply {
    Reply::Sse(tool_call(id, tool, &params.to_string()))
}

fn configure(places: &Places, sandbox: &str) {
    std::fs::write(
        places.home_path().join("config.toml"),
        format!(
            "model='ollama/qwen3'\nthinking='off'\nroots=[]\nsandbox='{sandbox}'\nmode='yolo'\n"
        ),
    )
    .unwrap();
}

// Catches: built-in tools ignore off/adaptive and retain the workspace read boundary.
#[test]
fn off_reads_lists_and_searches_a_sibling_directory() {
    broad_reads("off");
}

#[cfg(target_os = "macos")]
#[test]
fn adaptive_reads_lists_and_searches_where_the_shell_can_read() {
    broad_reads("adaptive");
}

fn broad_reads(sandbox: &str) {
    let sibling = tempfile::tempdir().unwrap();
    let sibling = sibling.path().canonicalize().unwrap();
    let file = sibling.join("public.txt");
    std::fs::write(&file, "SIBLING-PUBLIC-CONTENT").unwrap();
    let places = Places::new();
    configure(&places, sandbox);
    let stub = Stub::start(Plan {
        completions: vec![
            call("read", "read", json!({"path": file})),
            call("list", "glob", json!({"path": sibling, "pattern": "*.txt"})),
            call(
                "search",
                "grep",
                json!({"path": sibling, "query": "SIBLING-PUBLIC"}),
            ),
            Reply::Sse(text("Finished.")),
        ],
        ..Plan::default()
    });
    let finished = places.ego(&stub).args(["Inspect sibling"]).run();
    assert_eq!(finished.code, Some(0), "{}", finished.stderr);
    let generations = stub.generations();
    for (index, call_id, expected) in [
        (1, "read", "SIBLING-PUBLIC-CONTENT"),
        (2, "list", "public.txt"),
        (3, "search", "SIBLING-PUBLIC-CONTENT"),
    ] {
        let result = generations[index]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "tool" && message["tool_call_id"] == call_id)
            .expect("the requested tool result reaches the next generation")
            .to_string();
        assert!(
            result.contains(expected),
            "{sandbox} step {index}: {result}"
        );
    }
}

// Catches: broad read authority leaks into strict sandbox levels.
#[test]
fn confined_modes_refuse_sibling_reads_without_a_grant() {
    for sandbox in ["ro", "workspace"] {
        let sibling = tempfile::tempdir().unwrap();
        let file = sibling.path().join("public.txt");
        std::fs::write(&file, "CONFINED-SIBLING-CONTENT").unwrap();
        let places = Places::new();
        configure(&places, sandbox);
        let stub = Stub::start(Plan {
            completions: vec![
                call("read", "read", json!({"path": file})),
                Reply::Sse(text("Finished.")),
            ],
            ..Plan::default()
        });
        let finished = places.ego(&stub).args(["Inspect sibling"]).run();
        assert_eq!(finished.code, Some(0), "{}", finished.stderr);
        let generations = stub.generations();
        assert!(generations[1].to_string().contains("outside the workspace"));
        assert!(
            !serde_json::to_string(&generations)
                .unwrap()
                .contains("CONFINED-SIBLING-CONTENT")
        );
    }
}

// Catches: sandbox off/adaptive plus a user allow exposes protected credential bytes.
#[test]
fn broad_modes_refuse_credential_paths_even_with_user_allow() {
    let mut modes = vec!["off"];
    if cfg!(target_os = "macos") {
        modes.push("adaptive");
    }
    for sandbox in modes {
        let sibling = tempfile::tempdir().unwrap();
        let file = sibling.path().join(".env.synthetic");
        std::fs::write(&file, "SYNTHETIC_SECRET=DO-NOT-EXPOSE").unwrap();
        let places = Places::new();
        configure(&places, sandbox);
        let mut config = std::fs::read_to_string(places.home_path().join("config.toml")).unwrap();
        config.push_str("[permissions]\nallow=['read(/**)']\n");
        std::fs::write(places.home_path().join("config.toml"), config).unwrap();
        for (index, request) in [
            call("read", "read", json!({"path": file})),
            call(
                "search",
                "grep",
                json!({"path": sibling.path(), "query": "SYNTHETIC_SECRET"}),
            ),
            call(
                "list",
                "glob",
                json!({"path": sibling.path(), "pattern": "*"}),
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let stub = Stub::start(Plan {
                completions: vec![request, Reply::Sse(text("Finished."))],
                ..Plan::default()
            });
            let finished = places.ego(&stub).args(["Inspect credential"]).run();
            assert_eq!(
                finished.code,
                Some(if index == 0 { 3 } else { 0 }),
                "{}",
                finished.stderr
            );
            assert!(
                finished.stderr.contains(if index == 0 {
                    "protected credential path"
                } else {
                    "permit"
                }),
                "{}",
                finished.stderr
            );
            let generations = stub.generations();
            assert!(
                !serde_json::to_string(&generations)
                    .unwrap()
                    .contains("DO-NOT-EXPOSE")
            );
        }
    }
}

// Catches: canonical symlink targets bypass the broad-mode credential floor.
#[test]
fn off_refuses_a_public_symlink_to_a_credential_file() {
    let sibling = tempfile::tempdir().unwrap();
    let secret = sibling.path().join(".env.synthetic");
    let alias = sibling.path().join("public.txt");
    std::fs::write(&secret, "ALIAS_SECRET=DO-NOT-EXPOSE").unwrap();
    std::os::unix::fs::symlink(&secret, &alias).unwrap();
    let places = Places::new();
    configure(&places, "off");
    let stub = Stub::start(Plan {
        completions: vec![
            call("read", "read", json!({"path": alias})),
            Reply::Sse(text("Finished.")),
        ],
        ..Plan::default()
    });
    let finished = places.ego(&stub).args(["Inspect alias"]).run();
    assert_eq!(finished.code, Some(3), "{}", finished.stderr);
    assert!(finished.stderr.contains("protected credential path"));
    let generations = stub.generations();
    assert!(
        !serde_json::to_string(&generations)
            .unwrap()
            .contains("DO-NOT-EXPOSE")
    );
}
