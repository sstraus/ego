use super::*;

/// Every regular file under `root`, relative path and bytes, skipping the
/// named top-level directories. Any read failure panics rather than hiding it.
fn tree(root: &std::path::Path, skip: &[&str]) -> std::collections::BTreeMap<String, Vec<u8>> {
    fn walk(
        base: &std::path::Path,
        dir: &std::path::Path,
        skip: &[&str],
        out: &mut std::collections::BTreeMap<String, Vec<u8>>,
    ) {
        for entry in std::fs::read_dir(dir).expect("read fixture directory") {
            let entry = entry.expect("fixture directory entry");
            let path = entry.path();
            let relative = path
                .strip_prefix(base)
                .expect("entry under root")
                .to_string_lossy()
                .into_owned();
            if skip.contains(&relative.as_str()) {
                continue;
            }
            let kind = entry.file_type().expect("fixture entry type");
            if kind.is_dir() {
                walk(base, &path, skip, out);
            } else {
                out.insert(relative, std::fs::read(&path).expect("read fixture file"));
            }
        }
    }
    let mut out = std::collections::BTreeMap::new();
    walk(root, root, skip, &mut out);
    out
}

// Catches: bare `/login` waits behind the held model reply (busy refusal or
// queue receipt), reaches the model, writes config/credentials/workspace/journal,
// or lets the actual provider form start a sign-in while a turn is running.
#[test]
fn bare_login_inventory_renders_before_the_parent_is_released() {
    let (reply, gate) = gated_paced_line("LOGIN-INVENTORY-1169-HELD");
    let stub = Stub::start(Plan {
        completions: vec![reply],
        ..Plan::default()
    });
    struct ReleaseOnDrop(Arc<ReleaseGate>);
    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            self.0.release();
        }
    }
    let _release = ReleaseOnDrop(Arc::clone(&gate));
    let places = support::Places::new();
    places.file("AGENTS.md", "# login inventory fixture\n");
    let ego = places.ego(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "off",
        "--mode",
        "yolo",
        "--trust-workspace",
        "--session-id",
        support::FIXTURE_SESSION_ID,
    ]);
    ego.home_file(
        "config.toml",
        "# login inventory fixture: retain these exact bytes.\n",
    );
    let journal = places
        .home_path()
        .join("sessions")
        .join(support::FIXTURE_SESSION_ID)
        .join("events.jsonl");
    let (session, mut input, reader) = PtySession::start(&ego, 40, 120, Some((40, 120)));
    let mut output = Vec::new();
    until_prompt_from(&reader, &mut output, 0);
    let start = output.len();
    input
        .write_all(b"hold the reply during login inventory\r")
        .unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "LOGIN-INVENTORY-1169-HELD");
    assert_eq!(stub.generations().len(), 1);
    let home_before = tree(places.home_path(), &["sessions"]);
    let workspace_before = tree(places.workspace_path(), &[]);
    let journal_before = std::fs::read(&journal).expect("held journal exists");

    let start = output.len();
    input.write_all(b"/login\r").unwrap();
    input.flush().unwrap();
    until_normalized_without_new_generation(
        &reader,
        &mut output,
        start,
        "/login PROVIDER — authorize a provider",
        &stub,
        1,
    );
    let inventory = normalized(&output[start..]);
    assert!(
        inventory.contains("Built-in OAuth providers"),
        "{inventory}"
    );
    assert!(inventory.contains("openai-codex"), "{inventory}");
    assert!(!inventory.contains("anthropic"), "{inventory}");
    assert!(!inventory.contains("google-cloud"), "{inventory}");
    for refusal in [
        "login is unavailable",
        "pending until the current turn ends",
    ] {
        assert!(!inventory.contains(refusal), "{refusal}: {inventory}");
    }

    // The provider form keeps its busy refusal and starts no flow.
    let start = output.len();
    input.write_all(b"/login openai-codex\r").unwrap();
    input.flush().unwrap();
    until_normalized_without_new_generation(
        &reader,
        &mut output,
        start,
        "login is unavailable while a turn is running",
        &stub,
        1,
    );
    assert!(!normalized(&output[start..]).contains("Waiting for the loopback"));
    // A refused busy command stays in the composer; discard it before quitting.
    input.write_all(b"\x15").unwrap();
    input.flush().unwrap();

    assert_eq!(
        tree(places.home_path(), &["sessions"]),
        home_before,
        "home written"
    );
    assert_eq!(
        tree(places.workspace_path(), &[]),
        workspace_before,
        "workspace written"
    );
    assert_eq!(
        std::fs::read(&journal).expect("held journal exists"),
        journal_before,
        "journal written by login"
    );
    assert!(!places.home_path().join("auth.json").exists());
    assert_eq!(stub.generations().len(), 1, "login reached the model");

    let start = output.len();
    gate.release();
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert_eq!(stub.generations().len(), 1);
}
