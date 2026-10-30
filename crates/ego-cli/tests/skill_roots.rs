//! Portable roots observed through the real interactive binary.
mod support;

#[cfg(unix)]
#[test]
fn boss_portable_layout_lists_once_and_invokes_admitted_body() {
    // Catches: omitted portable roots, duplicate alias entries, or disabled invocation.
    // Count whole entries: a wrapped source path can also end a line with /foo.
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
        "---\nname: foo\ndescription: Portable fixture\n---\nPORTABLE-XVL-BODY $ARGUMENTS\n",
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
        .run_pty("/skills invoke foo CHECK-XVL");
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
                .is_some_and(|text| text.contains("PORTABLE-XVL-BODY CHECK-XVL"))),
        "{}",
        requests[0]
    );
}
