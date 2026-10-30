mod support;

#[cfg(unix)]
use support::{Ego, Plan, Stub};

#[cfg(unix)]
#[test]
fn configured_invalid_shared_root_refuses_instructions_instead_of_using_raw_paths() {
    let stub = Stub::start(Plan::default());
    let tree = tempfile::tempdir().expect("fixture root");
    let missing_home = tree.path().join("missing-home");

    let ego = Ego::against(&stub);
    ego.file("AGENTS.md", "project rules\n");
    std::fs::write(
        ego.home_path().join("config.toml"),
        format!("[instructions]\nshared_root={:?}\n", missing_home),
    )
    .unwrap();
    let missing_home_result = ego
        .env("HOME", missing_home.to_str().expect("UTF-8 fixture path"))
        .args([
            "--model",
            "ollama/qwen3",
            "--instructions",
            "AGENTS.md",
            "Ask",
        ])
        .run();
    assert_eq!(missing_home_result.code, Some(2));
    assert!(
        missing_home_result
            .stderr
            .contains("instruction discovery root could not be resolved"),
        "{}",
        missing_home_result.stderr
    );

    let ego = Ego::against(&stub);
    ego.file("AGENTS.md", "project rules\n");
    let missing_shared = tree.path().join("missing-shared");
    std::os::unix::fs::symlink(&missing_shared, ego.home_path().join("Gits"))
        .expect("dangling shared root");
    std::fs::write(
        ego.home_path().join("config.toml"),
        format!(
            "[instructions]\nshared_root={:?}\n",
            ego.home_path().join("Gits")
        ),
    )
    .unwrap();
    let missing_shared_result = ego
        .args([
            "--model",
            "ollama/qwen3",
            "--instructions",
            "AGENTS.md",
            "Ask",
        ])
        .run();
    assert_eq!(missing_shared_result.code, Some(2));
    assert!(
        missing_shared_result
            .stderr
            .contains("instruction discovery root could not be resolved"),
        "{}",
        missing_shared_result.stderr
    );
    assert!(
        stub.generations().is_empty(),
        "invalid roots reached provider"
    );
}
