//! `ego acp --exclusive-read-only`: an unattended session that nothing can widen (story 950-32d4).
#![cfg(unix)]
use crate::support;

use std::path::Path;
use std::process::Command;

use support::{AcpHost, Ego, Places, Plan, Reply, Stub, text, tool_call};

/// `mode = yolo` and `sandbox = off` ask for the widest session a configuration can state, so a
/// refusal below can only come from the policy.
fn host(places: &Places, stub: &Stub, extra_env: &[(&str, &str)]) -> AcpHost {
    std::fs::write(
        places.home_path().join("config.toml"),
        "model = 'ollama/qwen3'\nthinking = 'off'\nsandbox = 'off'\nmode = 'yolo'\n",
    )
    .expect("isolated config");
    let mut ego = Ego::at(places.home_path(), places.workspace_path(), Some(stub))
        .subcommand("acp")
        .args([
            "--exclusive-read-only",
            "--env-allow",
            "OLLAMA_HOST",
            "--sandbox",
            "off",
        ]);
    for (key, value) in extra_env {
        ego = ego.env(key, value);
    }
    AcpHost::adopt(ego.spawn_conversing())
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("/usr/bin/git")
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git runs");
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn a_write_and_a_push_are_refused_and_no_tool_is_offered() {
    let places = Places::new();
    // The session works in `scratch`; a git repository at the session root would make the macOS
    // adaptive sandbox refuse the session itself, and the refusal must come from the policy.
    let root = places.workspace_path();
    let (scratch, repo, remote) = (
        root.join("scratch"),
        root.join("repo"),
        root.join("remote.git"),
    );
    std::fs::create_dir_all(&scratch).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    git(root, &["init", "--bare", "-q", "remote.git"]);
    git(&repo, &["init", "-q"]);
    std::fs::write(repo.join("seed.txt"), "seed").unwrap();
    git(&repo, &["add", "seed.txt"]);
    git(&repo, &["commit", "-q", "-m", "seed"]);
    git(
        &repo,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call(
                "w",
                "write",
                r#"{"files":[{"path":"pwned.txt","content":"x"}]}"#,
            )),
            Reply::Sse(tool_call(
                "x",
                "exec",
                &format!(
                    r#"{{"argv":["/usr/bin/git","-C","{}","push","origin","HEAD"]}}"#,
                    repo.display()
                ),
            )),
            Reply::Sse(text("done")),
        ],
        ..Plan::default()
    });
    let mut host = host(&places, &stub, &[]);
    host.initialize();
    let session = host.new_session(scratch.to_str().unwrap());
    let answer = host.request(
        "session/prompt",
        serde_json::json!({"sessionId": session, "prompt": [{"type": "text", "text": "go"}]}),
    );
    assert!(answer["result"].is_object(), "{answer}");

    // The write path is relative, so it lands in the session cwd (`scratch`), not the root.
    for landed in [
        scratch.join("pwned.txt"),
        places.workspace_path().join("pwned.txt"),
    ] {
        assert!(!landed.exists(), "a write ran: {}", landed.display());
    }
    assert_eq!(
        git(&remote, &["for-each-ref"]),
        "",
        "a push reached the remote"
    );
    let generations = stub.generations();
    assert_eq!(generations.len(), 3, "{generations:#?}");
    for generation in &generations {
        let offered: Vec<String> = generation["tools"]
            .as_array()
            .map(|tools| {
                tools
                    .iter()
                    .map(|tool| tool["function"]["name"].as_str().unwrap_or("?").to_owned())
                    .collect()
            })
            .unwrap_or_default();
        assert!(offered.is_empty(), "tools were offered: {offered:?}");
    }
    host.finish();
}

#[test]
fn a_host_that_supplies_an_mcp_server_is_refused() {
    let places = Places::new();
    let stub = Stub::start(Plan::default());
    let mut host = host(&places, &stub, &[]);
    host.initialize();
    let opened = host.request(
        "session/new",
        serde_json::json!({
            "cwd": places.workspace_path(),
            "mcpServers": [{"name": "w", "command": "/bin/true", "args": [], "env": []}]
        }),
    );
    assert!(opened["error"].is_object(), "{opened}");
    assert!(opened["result"]["sessionId"].is_null(), "{opened}");
    host.finish();
}

/// The cleared environment is read back from the running process itself.
#[cfg(target_os = "linux")]
#[test]
fn ambient_credentials_do_not_reach_the_process() {
    let places = Places::new();
    let stub = Stub::start(Plan::default());
    let mut host = host(
        &places,
        &stub,
        &[
            ("GH_TOKEN", "ghp_fake"),
            ("SSH_AUTH_SOCK", "/tmp/fake.sock"),
            ("GIT_ASKPASS", "/tmp/askpass"),
            ("GIT_CONFIG_GLOBAL", "/tmp/gitconfig"),
            ("TUIC_SESSION", "fake"),
            ("UNRELATED", "1"),
        ],
    );
    host.initialize();
    let pid = host.child().id();
    let raw = std::fs::read(format!("/proc/{pid}/environ")).expect("the process environment");
    let names: Vec<String> = raw
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            String::from_utf8_lossy(entry)
                .split('=')
                .next()
                .unwrap()
                .to_owned()
        })
        .collect();
    for forbidden in [
        "GH_TOKEN",
        "SSH_AUTH_SOCK",
        "GIT_ASKPASS",
        "GIT_CONFIG_GLOBAL",
        "TUIC_SESSION",
        "UNRELATED",
    ] {
        assert!(
            !names.iter().any(|name| name == forbidden),
            "{forbidden} reached ego: {names:?}"
        );
    }
    for kept in ["PATH", "HOME", "EGO_HOME", "OLLAMA_HOST"] {
        assert!(
            names.iter().any(|name| name == kept),
            "{kept} was dropped: {names:?}"
        );
    }
    host.finish();
}
