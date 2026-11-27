//! Portable roots observed through the real interactive binary.
#[cfg(unix)]
use crate::support;

#[cfg(unix)]
#[test]
fn boss_portable_layout_lists_once_and_invokes_admitted_body() {
    // Catches: omitted portable roots, duplicate alias entries, or disabled invocation.
    // Count whole entries: a wrapped source path can also end a line with /foo.
    let stub = support::Stub::start(
        support::Plan {
            installed: vec![support::MODEL.to_owned()],
            completions: vec![support::Reply::Sse(support::text("portable complete"))],
            ..support::Plan::default()
        }
        .clear_skill_reviews(),
    );
    let places = support::Places::new();
    let ego = places.ego(&stub);
    ego.home_file(
        "config.toml",
        "model = \"ollama/qwen3\"\nthinking = \"off\"\nsandbox = \"off\"\n",
    );
    let source = ego.file(
        ".agents/skills/foo/SKILL.md",
        "---\nname: foo\ndescription: Portable fixture\n---\nPORTABLE-474-BODY $ARGUMENTS\n",
    );
    std::fs::create_dir_all(ego.workspace_path().join(".claude/skills")).unwrap();
    std::os::unix::fs::symlink(
        "../../.agents/skills/foo",
        ego.workspace_path().join(".claude/skills/foo"),
    )
    .unwrap();
    let listing = places.ego(&stub).conversing().run_pty("/skills details");
    assert_eq!(listing.code, Some(0), "{}", listing.stderr);
    let retained = support::terminal_transcript(listing.stdout.as_bytes(), 24, 80);
    assert_eq!(
        retained
            .lines()
            .filter(|line| line.trim() == "/foo")
            .count(),
        1,
        "{}",
        listing.stdout
    );
    assert!(
        listing.stdout.contains(&source.display().to_string()),
        "{}",
        listing.stdout
    );
    let invoked = places
        .ego(&stub)
        .conversing()
        .run_pty("/skills invoke foo CHECK-474");
    assert_eq!(invoked.code, Some(0), "{}", invoked.stderr);
    let requests = stub.conversation_generations();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["content"]
                .as_str()
                .is_some_and(|text| text.contains("PORTABLE-474-BODY CHECK-474"))),
        "{}",
        requests[0]
    );
}

#[cfg(unix)]
#[test]
fn skill_body_receives_session_and_project_variables_from_the_real_turn() {
    // Catches: the session or project variable reaches the model literally
    // because the turn never passes its context to the renderer.
    let stub = support::Stub::start(
        support::Plan {
            installed: vec![support::MODEL.to_owned()],
            completions: vec![support::Reply::Sse(support::text("vars complete"))],
            ..support::Plan::default()
        }
        .clear_skill_reviews(),
    );
    let places = support::Places::new();
    let ego = places.ego(&stub);
    ego.home_file(
        "config.toml",
        "model = \"ollama/qwen3\"\nthinking = \"off\"\nsandbox = \"off\"\n",
    );
    ego.file(
        ".agents/skills/vars/SKILL.md",
        "---\nname: vars\ndescription: Variable fixture\n---\nVARS session=${CLAUDE_SESSION_ID} project=${CLAUDE_PROJECT_DIR} end\n",
    );
    let workspace = ego.workspace_path().file_name().unwrap().to_owned();
    let invoked = places
        .ego(&stub)
        .conversing()
        .run_pty("/skills invoke vars");
    assert_eq!(invoked.code, Some(0), "{}", invoked.stderr);
    let requests = stub.conversation_generations();
    let text = requests[0]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| message["content"].as_str())
        .find(|text| text.contains("VARS session="))
        .unwrap_or_else(|| panic!("{}", requests[0]))
        .to_owned();
    assert!(!text.contains("${CLAUDE_"), "{text}");
    let project = text
        .split("project=")
        .nth(1)
        .unwrap()
        .split(" end")
        .next()
        .unwrap();
    assert!(project.ends_with(workspace.to_str().unwrap()), "{text}");
}

#[cfg(unix)]
#[test]
fn effort_variable_carries_the_effort_of_the_running_generation() {
    // Catches: `${CLAUDE_EFFORT}` stays literal in a skill the model invokes after a
    // generation, because the turn never hands its effort to the renderer.
    use support::{Ego, Plan, Reply, Stub, text, tool_call};
    let stub = Stub::start(
        Plan {
            completions: vec![
                Reply::Sse(tool_call(
                    "effort-call",
                    "skill",
                    r#"{"kind":"invoke","name":"effort-check","arguments":""}"#,
                )),
                Reply::Sse(text("done")),
            ],
            ..Plan::default()
        }
        .clear_skill_reviews(),
    );
    let ego = Ego::against(&stub);
    ego.home_file("config.toml", "thinking = \"off\"\n");
    ego.home_file(
        "skills/effort-check/SKILL.md",
        "---\nname: effort-check\ndescription: Effort fixture\n---\nEFFORT=${CLAUDE_EFFORT} END\n",
    );
    let finished = ego
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--mode",
            "yolo",
        ])
        .arg("Run the effort skill")
        .run();
    assert_eq!(finished.code, Some(0), "{}", finished.stderr);
    let requests = stub.conversation_generations();
    let admitted = requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| message["content"].as_str())
        .find(|content| content.contains("Admitted skill instruction"))
        .unwrap_or_else(|| panic!("{}", requests[1]["messages"]));
    assert!(admitted.contains("EFFORT=off END"), "{admitted}");
}
