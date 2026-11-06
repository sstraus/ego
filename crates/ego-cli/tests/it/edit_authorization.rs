//! Regression Write rules cover same-file edits.

use crate::support;

use support::{Ego, Places, Plan, Reply, Stub, text, tool_call};

fn editing(places: &Places, allow: &str, deny: Option<&str>) -> support::Finished {
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call(
                "edit-price",
                "edit",
                r#"{"files":[{"path":"src/pricing.py","chunks":[{"old_string":"apple = 1","new_string":"apple = 2"}]}]}"#,
            )),
            Reply::Sse(text("done")),
        ],
        ..Plan::default()
    });
    let mut command = places.ego(&stub).args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "off",
        "--allow",
        allow,
    ]);
    if let Some(deny) = deny {
        command = command.args(["--deny", deny]);
    }
    command.arg("Change the apple price").run()
}

// Catches: edit requires an unstated read rule even when write covers its file.
#[test]
fn write_allow_edits_without_unstated_read_rule() {
    for allow in ["write(**)", "write(src/**)"] {
        let places = Places::new();
        let path = places.file("src/pricing.py", "apple = 1\npear = 3\n");
        let run = editing(&places, allow, None);
        assert_eq!(run.code, Some(0), "{allow}: {}", run.stderr);
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "apple = 2\npear = 3\n"
        );
    }
}

// Catches: write coverage escapes its glob or bypasses an explicit deny.
#[test]
fn edit_write_allow_preserves_glob_and_explicit_denies() {
    for (allow, deny) in [
        ("write(other/**)", None),
        ("write(**)", Some("read(src/**)")),
        ("write(**)", Some("write(src/**)")),
    ] {
        let places = Places::new();
        let path = places.file("src/pricing.py", "apple = 1\n");
        let run = editing(&places, allow, deny);
        assert_ne!(run.code, Some(0), "{allow} {deny:?}: {}", run.stderr);
        assert_eq!(std::fs::read_to_string(path).unwrap(), "apple = 1\n");
    }
}

// Catches: the parser recommends write for edit while enforcement rejects it.
#[test]
fn edit_unknown_head_guidance_matches_real_run() {
    let rejected = Ego::nowhere()
        .args(["--allow", "edit(src/**)", "Change the price"])
        .run();
    assert_ne!(rejected.code, Some(0));
    assert!(
        rejected.stderr.contains("including edit"),
        "{}",
        rejected.stderr
    );
    let places = Places::new();
    let path = places.file("src/pricing.py", "apple = 1\n");
    let accepted = editing(&places, "write(src/**)", None);
    assert_eq!(accepted.code, Some(0), "{}", accepted.stderr);
    assert_eq!(std::fs::read_to_string(path).unwrap(), "apple = 2\n");
}
