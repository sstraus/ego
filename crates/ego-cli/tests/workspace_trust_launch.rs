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

// Catches: explicit trust silently overrides bare.
#[test]
fn explicit_workspace_trust_rejects_bare_without_effects() {
    let (places, stub) = fixture();
    let bare = places
        .ego(&stub)
        .conversing()
        .args(["--bare", "--trust-workspace", "task"])
        .run();
    assert_eq!(bare.code, Some(2));
    assert!(bare.stderr.contains("--bare") && bare.stderr.contains("--trust-workspace"));
    assert!(!places.ego(&stub).home_path().join("trust.json").exists());
    assert!(stub.generations().is_empty());
}

// Catches: headless launch rejects the operator grant or forgets standing trust.
#[test]
fn headless_workspace_grant_loads_project_skill_and_is_remembered() {
    let places = Places::new();
    places.file("AGENTS.md", "# REGRESSION-MARKER-INSTRUCTIONS\n");
    let stub = Stub::start(Plan {
        installed: vec!["qwen3".to_owned(), "project".to_owned()],
        completions: vec![Reply::Sse(text("REGRESSION-MARKER-OK")); 4],
        ..Plan::default()
    });
    places.file(
        ".ego/skills/headless/SKILL.md",
        "---\nname: headless\ndescription: REGRESSION-MARKER\n---\nProject skill body.\n",
    );
    places.file(".ego/config.toml", "model = \"ollama/project\"\n");
    places
        .ego(&stub)
        .home_file("config.toml", "model = \"ollama/qwen3\"\n");
    for (index, (flag, admitted)) in [
        (None, false),
        (Some("--trust-workspace"), true),
        (None, true),
        (Some("--bare"), false),
    ]
    .into_iter()
    .enumerate()
    {
        let id = format!("01900000-0000-7000-8000-{index:012}");
        let ego = places.ego(&stub).args([
            "--sandbox",
            "off",
            "--max-output-tokens",
            "100",
            "--session-id",
            &id,
            "task",
        ]);
        let ego = if let Some(flag) = flag {
            ego.args([flag])
        } else {
            ego
        };
        let result = ego.run();
        assert_eq!(result.code, Some(0), "{}", result.stderr);
        let requests = stub.generations();
        let generation = requests.last().unwrap();
        assert_eq!(
            generation["max_tokens"], 100,
            "output cap must reach the wire"
        );
        assert_eq!(
            generation["model"],
            if admitted { "project" } else { "qwen3" }
        );
        let request = generation["messages"].to_string();
        assert_eq!(
            request.contains("REGRESSION-MARKER-INSTRUCTIONS"),
            admitted,
            "{request}"
        );
        let events = std::fs::read_to_string(
            places
                .home_path()
                .join("sessions")
                .join(&id)
                .join("events.jsonl"),
        )
        .unwrap();
        assert_eq!(
            events.contains("REGRESSION-MARKER"),
            admitted,
            "{events}"
        );
    }
    assert!(places.home_path().join("trust.json").exists());
}

// Catches: an external import leaks before consent, or remembered consent
// survives replacement bytes. Uses the built CLI with isolated HOME/workspace.
#[test]
fn external_import_disclosure_requires_content_bound_workspace_consent() {
    let places = Places::new();
    let home = places.home_path();
    let maintainer = home.join("workspace/example-project/maintainer.md");
    std::fs::create_dir_all(maintainer.parent().unwrap()).unwrap();
    std::fs::write(&maintainer, "EXTERNAL-maintainer-477-ORIGINAL").unwrap();
    places.file("AGENTS.md", "@~/workspace/example-project/maintainer.md\n");
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("OK")); 5],
        ..Plan::default()
    });
    for (index, grant, expected) in [(0, false, false), (1, true, true), (2, false, true)] {
        let id = format!("01900000-0000-7000-8000-{index:012}");
        let ego = places.ego(&stub).args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--session-id",
            &id,
            "task",
        ]);
        let result = if grant {
            ego.args(["--trust-workspace"]).run()
        } else {
            ego.run()
        };
        assert_eq!(result.code, Some(0), "{} {}", result.stdout, result.stderr);
        assert_eq!(
            stub.generations()[index]["messages"]
                .to_string()
                .contains("EXTERNAL-maintainer-477-ORIGINAL"),
            expected
        );
    }
    std::fs::write(&maintainer, "EXTERNAL-maintainer-477-CHANGED").unwrap();
    let result = places
        .ego(&stub)
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--session-id",
            "01900000-0000-7000-8000-000000000003",
            "task",
        ])
        .run();
    assert_eq!(result.code, Some(0), "{} {}", result.stdout, result.stderr);
    assert!(
        !stub.generations()[3]["messages"]
            .to_string()
            .contains("EXTERNAL-maintainer-477")
    );
}

// Catches: --trust-workspace approves ambient files but silently drops imports
// from the instruction file the operator explicitly selected.
#[test]
fn explicit_instruction_file_external_import_survives_explicit_workspace_grant() {
    let places = Places::new();
    places.file("AGENTS.md", "Ambient workspace instructions.\n");
    let selected = places.file("selected.md", "@~/shared.md\n");
    std::fs::write(
        places.home_path().join("shared.md"),
        "REGRESSION-MARKER",
    )
    .unwrap();
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("OK"))],
        ..Plan::default()
    });
    let result = places
        .ego(&stub)
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--trust-workspace",
            "--instructions",
            selected.to_str().unwrap(),
            "task",
        ])
        .run();
    assert_eq!(result.code, Some(0), "{} {}", result.stdout, result.stderr);
    let requests = stub.generations();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0]["messages"]
            .to_string()
            .contains("REGRESSION-MARKER"),
        "explicitly granted import omitted from provider request; diagnostics: {}",
        result.stderr
    );
}

// Catches: portable-only ancestor skills fail to trigger the real trust question,
// or ordinary skill edits incorrectly revoke an executable-surface answer.
#[test]
fn portable_only_workspace_prompts_and_ancestor_edits_keep_trust() {
    let places = Places::new();
    places.file(".git/HEAD", "ref: refs/heads/fixture\n");
    let skill = places.file(
        ".agents/skills/foo/SKILL.md",
        "---\nname: foo\ndescription: portable fixture\n---\nFIRST-REGRESSION-MARKER\n",
    );
    let nested = places
        .file("nested/.keep", "")
        .parent()
        .unwrap()
        .to_path_buf();
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("OK")); 3],
        ..Plan::default()
    });
    for (index, expected_prompt) in [true, false, false].into_iter().enumerate() {
        if index == 2 {
            std::fs::write(
                &skill,
                "---\nname: foo\ndescription: changed portable fixture\n---\nSECOND-REGRESSION-MARKER\n",
            )
            .unwrap();
        }
        let result = support::Ego::at(places.home_path(), &nested, Some(&stub))
            .conversing()
            .args(["--model", "ollama/qwen3", "--sandbox", "off"])
            .run_pty_trusting("yes", "/skills");
        assert_eq!(result.code, Some(0), "{}", result.stdout);
        assert_eq!(
            result.stdout.contains("Trust project files"),
            expected_prompt,
            "{}",
            result.stdout
        );
        assert!(result.stdout.contains("/foo"), "{}", result.stdout);
        assert!(
            result.stdout.contains(&skill.display().to_string()),
            "{}",
            result.stdout
        );
    }
    assert!(stub.generations().is_empty());
}
