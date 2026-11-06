use super::*;

fn revision(fixture: &Fixture, profile: Option<&str>) -> String {
    let mut args = vec!["config", "snapshot"];
    if let Some(profile) = profile {
        args.extend(["--profile", profile]);
    }
    let output = fixture.run(&args);
    assert!(output.status.success(), "{}", error(&output));
    let snapshot: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    snapshot["revision"].as_str().unwrap().to_owned()
}

fn conflict(output: &Output) {
    assert_eq!(output.status.code(), Some(1));
    let reply: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(reply["code"], "conflict");
    assert_eq!(reply["persisted"], false);
    assert_eq!(reply["next_action"], "snapshot");
}

// Catches: an intervening second CLI process update silently overwritten.
#[test]
fn managed_conditional_write_rejects_external_writer() {
    let fixture = Fixture::new();
    fixture.write_config("model='ollama/original'\n");
    let token = revision(&fixture, None);
    assert_eq!(token, revision(&fixture, None));
    assert!(
        fixture
            .run(&["config", "set", "mode='plan'"])
            .status
            .success()
    );
    let before = std::fs::read(fixture.config_path()).unwrap();
    conflict(&fixture.run(&[
        "config",
        "batch",
        "--if-revision",
        &token,
        "--set",
        "model='ollama/new'",
    ]));
    assert_eq!(std::fs::read(fixture.config_path()).unwrap(), before);
    let fresh = revision(&fixture, None);
    assert_ne!(token, fresh);
    assert!(
        fixture
            .run(&[
                "config",
                "batch",
                "--if-revision",
                &fresh,
                "--set",
                "model='ollama/new'"
            ])
            .status
            .success()
    );
    assert_eq!(text(&fixture.run(&["config", "get", "mode"])), "\"plan\"\n");
}

// Catches: a base revision accepted for another profile.
#[test]
fn managed_conditional_write_rejects_other_target_revision() {
    let fixture = Fixture::new();
    fixture.write_config("model='ollama/original'\n");
    let base = revision(&fixture, None);
    let profile = revision(&fixture, Some("ci"));
    assert_ne!(base, profile);
    let before = std::fs::read(fixture.config_path()).unwrap();
    conflict(&fixture.run(&[
        "config",
        "batch",
        "--profile",
        "ci",
        "--if-revision",
        &base,
        "--set",
        "mode='plan'",
    ]));
    assert_eq!(std::fs::read(fixture.config_path()).unwrap(), before);
    assert!(
        fixture
            .run(&[
                "config",
                "batch",
                "--profile",
                "ci",
                "--if-revision",
                &profile,
                "--set",
                "mode='plan'"
            ])
            .status
            .success()
    );
}

// Catches: two simultaneous conditional writers both pass the revision check.
#[test]
fn managed_conditional_writers_have_exactly_one_winner() {
    let fixture = Fixture::new();
    fixture.write_config("# keep\nmodel='ollama/original'\n");
    let token = revision(&fixture, None);
    let barrier = std::sync::Barrier::new(2);
    let outputs = std::thread::scope(|scope| {
        let handles: Vec<_> = ["mode='plan'", "mode='auto'"]
            .into_iter()
            .map(|assignment| {
                let fixture = &fixture;
                let token = &token;
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    fixture.run(&[
                        "config",
                        "batch",
                        "--if-revision",
                        token,
                        "--set",
                        assignment,
                    ])
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(
        outputs
            .iter()
            .filter(|output| output.status.success())
            .count(),
        1
    );
    for output in outputs.iter().filter(|output| !output.status.success()) {
        conflict(output);
    }
    assert_eq!(
        text(&fixture.run(&["config", "get", "model"])),
        "\"ollama/original\"\n"
    );
}

// Catches: revisions reveal an unkeyed digest of low-entropy secrets or ignore private changes.
#[test]
fn managed_revisions_are_owner_private_and_secret_sensitive() {
    let a = Fixture::new();
    let b = Fixture::new();
    for fixture in [&a, &b] {
        fixture.write_config("[auth.acme]\ncredential_command='SECRET'\n");
    }
    let token = revision(&a, None);
    assert_ne!(token, revision(&b, None));
    assert_eq!(token.len(), 64);
    let snapshot = a.run(&["config", "snapshot"]);
    assert!(!text(&snapshot).contains("SECRET"));
    a.write_config("[auth.acme]\ncredential_command='OTHER'\n");
    assert_ne!(token, revision(&a, None));
}
