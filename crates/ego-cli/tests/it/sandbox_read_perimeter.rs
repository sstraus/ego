#![cfg(unix)]

use crate::support;

use serde_json::json;
use support::{Finished, Places, Plan, Reply, Stub, text, tool_call};

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

fn run(places: &Places, stub: &Stub, prompt: &str) -> Finished {
    // Keep HOME, EGO_HOME, workspace and probe scratch separate: capability
    // measurement needs a credential-shaped fixture outside the workspace.
    let home = tempfile::tempdir().unwrap();
    let temporary = home.path().join("temporary");
    std::fs::create_dir(&temporary).unwrap();
    let mut finished = places
        .ego(stub)
        .env("HOME", home.path().to_str().unwrap())
        .env("TMPDIR", temporary.to_str().unwrap())
        .args([prompt])
        .run();
    if finished.code != Some(0) {
        // Preserve the actual native probe result before fixture cleanup.
        if let Some((_, suffix)) = finished.stderr.split_once("stored as run evidence ") {
            let hash = suffix.chars().take(64).collect::<String>();
            for entry in std::fs::read_dir(places.home_path().join("sessions"))
                .into_iter()
                .flatten()
                .flatten()
            {
                let blob = entry.path().join("evidence").join(format!("{hash}.blob"));
                if let Ok(bytes) = std::fs::read(blob) {
                    finished.stderr.push_str(&String::from_utf8_lossy(&bytes));
                }
            }
        }
    }
    finished
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
            call("read", "read", json!({"files":[{"path": file}]})),
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
    let finished = run(&places, &stub, "Inspect sibling");
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
                call("read", "read", json!({"files":[{"path": file}]})),
                Reply::Sse(text("Finished.")),
            ],
            ..Plan::default()
        });
        let finished = run(&places, &stub, "Inspect sibling");
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

// Catches: a broad user allow exposes credentials in the default Adaptive posture.
#[cfg(target_os = "macos")]
#[test]
fn adaptive_refuses_credential_paths_with_broad_user_allow() {
    for sandbox in ["adaptive"] {
        let sibling = tempfile::tempdir().unwrap();
        let file = sibling.path().join(".env.synthetic");
        std::fs::write(&file, "SYNTHETIC_SECRET=DO-NOT-EXPOSE").unwrap();
        // This fixture must exercise the credential floor, not an enclosing
        // checkout's ignore rules (public checkouts can ignore `.env*`).
        // Explicitly include the synthetic file so grep/glob really visit it.
        std::fs::write(
            sibling.path().join(".gitignore"),
            ".env*\n!.env.synthetic\n",
        )
        .unwrap();
        let places = Places::new();
        configure(&places, sandbox);
        let mut config = std::fs::read_to_string(places.home_path().join("config.toml")).unwrap();
        config.push_str("[permissions]\nallow=['read(/**)']\n");
        std::fs::write(places.home_path().join("config.toml"), config).unwrap();
        for (index, request) in [
            call("read", "read", json!({"files":[{"path": file}]})),
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
            let finished = run(&places, &stub, "Inspect credential");
            assert_eq!(finished.code, Some(0), "{}", finished.stderr);
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
            assert_eq!(generations.len(), 2);
            if index == 0 {
                assert!(
                    generations[1]
                        .to_string()
                        .contains("protected credential path")
                );
            }
            assert!(
                !serde_json::to_string(&generations)
                    .unwrap()
                    .contains("DO-NOT-EXPOSE")
            );
        }
    }
}

// Catches: sandbox off still refuses canonical credential targets, or exposes values.
#[test]
fn off_reads_key_names_through_a_public_symlink_to_a_credential_file() {
    let sibling = tempfile::tempdir().unwrap();
    let secret = sibling.path().join(".env.synthetic");
    let alias = sibling.path().join("public.txt");
    std::fs::write(&secret, "ALIAS_SECRET=DO-NOT-EXPOSE").unwrap();
    std::os::unix::fs::symlink(&secret, &alias).unwrap();
    std::fs::write(
        sibling.path().join(".gitignore"),
        ".env*\n!.env.synthetic\n",
    )
    .unwrap();
    let places = Places::new();
    configure(&places, "off");
    let stub = Stub::start(Plan {
        completions: vec![
            call("read", "read", json!({"files":[{"path": alias}]})),
            call(
                "search",
                "grep",
                json!({"path": sibling.path(), "query": "ALIAS_SECRET"}),
            ),
            call(
                "list",
                "glob",
                json!({"path": sibling.path(), "pattern": "*"}),
            ),
            Reply::Sse(text("Finished.")),
        ],
        ..Plan::default()
    });
    let finished = run(&places, &stub, "Inspect alias");
    assert_eq!(finished.code, Some(0), "{}", finished.stderr);
    assert!(!finished.stderr.contains("protected credential path"));
    let generations = stub.generations();
    assert_eq!(generations.len(), 4);
    assert!(generations[1].to_string().contains("ALIAS_SECRET"));
    assert!(generations[2].to_string().contains("ALIAS_SECRET"));
    assert!(generations[3].to_string().contains(".env.synthetic"));
    assert!(!finished.stderr.contains("outside the permit"));
    assert!(
        !serde_json::to_string(&generations)
            .unwrap()
            .contains("DO-NOT-EXPOSE")
    );
}

// Catches: an exact owner credential grant admits the read but the model still gets key names.
#[cfg(target_os = "macos")]
#[test]
fn adaptive_exact_credential_read_grant_discloses_values() {
    let sibling = tempfile::tempdir().unwrap();
    let file = sibling.path().join(".env.synthetic");
    std::fs::write(&file, "OWNER_KEY=DO-NOT-EXPOSE\n").unwrap();
    let file = file.canonicalize().unwrap();
    let places = Places::new();
    configure(&places, "adaptive");
    let config_path = places.home_path().join("config.toml");
    let mut config = std::fs::read_to_string(&config_path).unwrap();
    config.push_str(&format!(
        "[permissions]\nallow=['read({})']\n",
        file.display()
    ));
    std::fs::write(config_path, config).unwrap();
    let stub = Stub::start(Plan {
        completions: vec![
            call("read", "read", json!({"files":[{"path": file}]})),
            Reply::Sse(text("Finished.")),
        ],
        ..Plan::default()
    });
    let finished = run(&places, &stub, "Inspect credential values");
    assert_eq!(finished.code, Some(0), "{}", finished.stderr);
    assert!(!finished.stderr.contains("protected credential path"));
    let generations = stub.generations();
    assert!(generations[1].to_string().contains("OWNER_KEY"));
    assert!(generations[1].to_string().contains("DO-NOT-EXPOSE"));
}
