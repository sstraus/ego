//! Portable roots observed through the real interactive binary.
mod support;

#[cfg(unix)]
#[test]
fn boss_portable_layout_lists_once_and_invokes_admitted_body() {
    // Catches: the CLI omits portable roots or alias collisions disable invocation.
    let stub = support::Stub::start(support::Plan {
        installed: vec![support::MODEL.to_owned()],
        completions: vec![support::Reply::Sse(support::text("portable complete"))],
        ..support::Plan::default()
    });
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
    assert_eq!(
        listing.stdout.matches("/foo\r\n").count() + listing.stdout.matches("/foo\n").count(),
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
        .run_pty("/skills invoke foo REGRESSION-MARKER");
    assert_eq!(invoked.code, Some(0), "{}", invoked.stderr);
    let requests = stub.generations();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["content"]
                .as_str()
                .is_some_and(|text| text.contains("REGRESSION-MARKER-BODY REGRESSION-MARKER"))),
        "{}",
        requests[0]
    );
}
