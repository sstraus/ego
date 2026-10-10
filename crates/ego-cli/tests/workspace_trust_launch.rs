mod support;

use support::{Places, Plan, Reply, Stub, text};

fn fixture() -> (Places, Stub) {
    let places = Places::new();
    places.file("AGENTS.md", "# REGRESSION-MARKER-INSTRUCTIONS\n");
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("REGRESSION-MARKER-OK")); 4],
        ..Plan::default()
    });
    (places, stub)
}

// Catches: yolo accidentally authorizes project files or discards a refusal.
#[test]
fn yolo_preserves_the_trust_question_and_remembered_refusal() {
    let (places, stub) = fixture();
    for first in [true, false] {
        let result = places
            .ego(&stub)
            .conversing()
            .args(["--model", "ollama/qwen3", "--sandbox", "off", "--yolo"])
            .run_pty_trusting("no", "task");
        assert_eq!(result.code, Some(0), "{}", result.stdout);
        assert_eq!(result.stdout.contains("Trust project files"), first);
    }
    assert_eq!(stub.generations().len(), 2);
    for generation in stub.generations() {
        assert!(
            !generation["messages"]
                .to_string()
                .contains("REGRESSION-MARKER-INSTRUCTIONS")
        );
    }
}

// Catches: an old refusal defeats a fresh explicit grant, or the grant is not
// remembered by the ordinary launch path.
#[test]
fn explicit_workspace_trust_replaces_refusal_and_is_remembered() {
    let (places, stub) = fixture();
    let refused = places
        .ego(&stub)
        .conversing()
        .args(["--model", "ollama/qwen3", "--sandbox", "off"])
        .run_pty_trusting("no", "first");
    assert_eq!(refused.code, Some(0));
    for explicit in [true, false] {
        let ego = places.ego(&stub).conversing().args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "next",
        ]);
        let ego = if explicit {
            ego.args(["--trust-workspace"])
        } else {
            ego
        };
        let result = ego.run_pty_seeded();
        assert_eq!(result.code, Some(0), "{}", result.stdout);
        assert!(!result.stdout.contains("Trust project files"));
    }
    let generations = stub.generations();
    assert_eq!(generations.len(), 3);
    for generation in &generations[1..] {
        assert!(
            generation["messages"]
                .to_string()
                .contains("REGRESSION-MARKER-INSTRUCTIONS")
        );
    }
}

// Catches: explicit trust silently overrides bare or admits a headless run.
#[test]
fn explicit_workspace_trust_rejects_bare_and_headless_without_effects() {
    let (places, stub) = fixture();
    let bare = places
        .ego(&stub)
        .conversing()
        .args(["--bare", "--trust-workspace", "task"])
        .run();
    assert_eq!(bare.code, Some(2));
    assert!(bare.stderr.contains("--bare") && bare.stderr.contains("--trust-workspace"));
    let headless = places.ego(&stub).args(["--trust-workspace", "task"]).run();
    assert_eq!(headless.code, Some(2), "{}", headless.stderr);
    assert!(
        headless
            .stderr
            .contains("requires an interactive terminal launch")
    );
    assert!(!places.ego(&stub).home_path().join("trust.json").exists());
    assert!(stub.generations().is_empty());
}
