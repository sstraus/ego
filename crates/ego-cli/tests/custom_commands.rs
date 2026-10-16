//! Offline process evidence: the built binary sends captured shell output, not markers.
mod support;

use support::{MODEL, Places, Plan, Reply, Stub};

fn plan() -> Plan {
    Plan {
        installed: vec![MODEL.to_owned()],
        completions: vec![Reply::Sse(support::text("review complete"))],
        ..Plan::default()
    }
}

#[test]
fn custom_commands_capture_shell_and_inline_files_before_provider_send() {
    let stub = Stub::start(plan());
    let places = Places::new();
    let ego = places.ego(&stub);
    ego.home_file(
        "config.toml",
        "model = \"ollama/qwen3\"\nthinking = \"off\"\nsandbox = \"off\"\n",
    );
    ego.home_file("commands/review.md", "---\ndescription: Review changes\nargument-hint: '[file]'\n---\nReview $1\n@context.txt\n!`printf command-captured-output`");
    ego.file("context.txt", "FILE-CONTEXT !`this-is-data-not-a-command`");
    let result = places
        .ego(&stub)
        .args(["/review 'some file'", "--mode", "yolo", "--sandbox", "off"])
        .run();
    assert_eq!(result.code, Some(0), "{}", result.stderr);
    let requests = stub.generations();
    assert_eq!(requests.len(), 1);
    let body = requests[0].to_string();
    assert!(body.contains("Review some file"), "{body}");
    assert!(body.contains("command-captured-output"), "{body}");
    assert!(!body.contains("!`printf"), "{body}");
    assert!(
        body.contains("FILE-CONTEXT !`this-is-data-not-a-command`"),
        "{body}"
    );
}

#[test]
fn custom_commands_import_escape_and_overflow_refuse_before_shell_effects() {
    for (reference, content) in [
        ("../outside.txt", "small".to_owned()),
        ("large.txt", "x".repeat(65537)),
    ] {
        let stub = Stub::start(plan());
        let places = Places::new();
        let ego = places.ego(&stub);
        ego.home_file(
            "config.toml",
            "model = \"ollama/qwen3\"\nthinking = \"off\"\nsandbox = \"off\"\n",
        );
        ego.home_file(
            "commands/review.md",
            &format!("!`touch unexpected-effect`\n@{reference}"),
        );
        ego.file("large.txt", &content);
        let marker = ego.workspace_path().join("unexpected-effect");
        let result = places
            .ego(&stub)
            .args(["/review", "--mode", "yolo", "--sandbox", "off"])
            .run();
        assert_ne!(result.code, Some(0));
        assert!(!marker.exists());
        assert!(stub.generations().is_empty());
    }
}

#[test]
fn custom_commands_shell_failure_refuses_partial_provider_prompt() {
    let stub = Stub::start(plan());
    let places = Places::new();
    let ego = places.ego(&stub);
    ego.home_file(
        "config.toml",
        "model = \"ollama/qwen3\"\nthinking = \"off\"\nsandbox = \"off\"\n",
    );
    ego.home_file("commands/review.md", "Review !`printf partial; exit 7`");
    let result = places
        .ego(&stub)
        .args(["/review", "--mode", "yolo", "--sandbox", "off"])
        .run();
    assert_ne!(result.code, Some(0));
    let diagnostic = format!("{}{}", result.stdout, result.stderr);
    assert!(diagnostic.contains("command expansion failed"));
    assert!(diagnostic.contains("7"), "{diagnostic}");
    assert!(stub.generations().is_empty());
}

#[test]
fn custom_commands_shell_arguments_are_data_not_source() {
    let stub = Stub::start(plan());
    let places = Places::new();
    let ego = places.ego(&stub);
    ego.home_file(
        "config.toml",
        "model = \"ollama/qwen3\"\nthinking = \"off\"\nsandbox = \"off\"\n",
    );
    ego.home_file("commands/review.md", "!`printf '%s' \"$1\"`");
    let result = places
        .ego(&stub)
        .args([
            "/review '$(touch unexpected-effect)'",
            "--mode",
            "yolo",
            "--sandbox",
            "off",
        ])
        .run();
    assert_eq!(result.code, Some(0), "{}", result.stderr);
    assert!(!ego.workspace_path().join("unexpected-effect").exists());
    assert!(
        stub.generations()[0]
            .to_string()
            .contains("$(touch unexpected-effect)")
    );
}

#[test]
fn custom_commands_shell_policy_refusal_never_contacts_provider() {
    let stub = Stub::start(plan());
    let places = Places::new();
    let ego = places.ego(&stub);
    ego.home_file(
        "config.toml",
        "model = \"ollama/qwen3\"\nthinking = \"off\"\nsandbox = \"off\"\n",
    );
    ego.home_file("commands/review.md", "Review !`printf must-not-run`");
    let result = places
        .ego(&stub)
        .args(["/review", "--mode", "plan", "--sandbox", "off"])
        .run();
    assert_ne!(result.code, Some(0));
    assert!(stub.generations().is_empty());
}

#[test]
fn custom_commands_acp_catalogue_precedes_first_prompt_and_expands_same_body() {
    let stub = Stub::start(plan());
    let places = Places::new();
    let ego = places.ego(&stub);
    ego.home_file(
        "config.toml",
        "model = \"ollama/qwen3\"\nthinking = \"off\"\nsandbox = \"off\"\n",
    );
    ego.home_file(
        "commands/review.md",
        "---\ndescription: ACP review\nargument-hint: '[file]'\n---\nACP-EXPANDED ${1:-default} ${@:2:1}",
    );
    let mut host = support::AcpHost::adopt(places.ego(&stub).subcommand("acp").spawn_conversing());
    host.initialize();
    let session = host.new_session(ego.workspace_path().to_str().unwrap());
    let catalogue = loop {
        let frame = host.read_frame();
        if frame["params"]["update"]["sessionUpdate"] == "available_commands_update" {
            break frame;
        }
    };
    let commands = catalogue["params"]["update"]["availableCommands"]
        .as_array()
        .expect("catalogue before first prompt");
    assert!(
        commands
            .iter()
            .any(|command| command["name"] == "review" && command["input"]["hint"] == "[file]"),
        "{catalogue}"
    );
    let response = host.request(
        "session/prompt",
        serde_json::json!({"sessionId":session,"prompt":[{"type":"text","text":"/review target"}]}),
    );
    let stderr = host.finish();
    assert_eq!(
        response["result"]["stopReason"], "end_turn",
        "{response} {stderr}"
    );
    assert!(
        stub.generations()[0]
            .to_string()
            .contains("ACP-EXPANDED target")
    );
}

#[cfg(unix)]
#[test]
fn custom_commands_project_review_sends_real_git_diff_after_trust() {
    let stub = Stub::start(plan());
    let places = Places::new();
    let ego = places.ego(&stub);
    ego.home_file(
        "config.toml",
        "model = \"ollama/qwen3\"\nthinking = \"off\"\nsandbox = \"off\"\n",
    );
    ego.file(
        ".claude/commands/review.md",
        "Review $ARGUMENTS\n!`git diff -- tracked.txt`",
    );
    ego.file("tracked.txt", "before\n");
    for args in [vec!["init", "--quiet"], vec!["add", "tracked.txt"]] {
        let result = std::process::Command::new("git")
            .args(args)
            .current_dir(ego.workspace_path())
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    ego.file("tracked.txt", "REGRESSION-MARKER-REAL-DIFF\n");
    let result = places
        .ego(&stub)
        .conversing()
        .args(["--mode", "yolo", "--sandbox", "off"])
        .run_pty("/review foo");
    assert_eq!(result.code, Some(0), "{}", result.stderr);
    let request = stub.generations()[0].to_string();
    assert!(request.contains("Review foo"), "{request}");
    assert!(request.contains("+REGRESSION-MARKER-REAL-DIFF"), "{request}");
}

// Catches: core slash interception executes bracketed-paste data as a command.
#[test]
fn pasted_custom_command_stays_literal_and_never_runs_its_shell() {
    let stub = Stub::start(plan());
    let places = Places::new();
    let ego = places.ego(&stub);
    ego.home_file(
        "config.toml",
        "model = \"ollama/qwen3\"\nthinking = \"off\"\nsandbox = \"off\"\n",
    );
    ego.home_file(
        "commands/review.md",
        "EXPANDED !`touch pasted-command-effect`",
    );
    let marker = ego.workspace_path().join("pasted-command-effect");
    let result = places
        .ego(&stub)
        .conversing()
        .args(["--mode", "yolo", "--sandbox", "off"])
        .run_pty("\x1b[200~/review\x1b[201~");
    assert_eq!(result.code, Some(0), "{}", result.stdout);
    assert!(
        !marker.exists(),
        "bracketed paste executed the command shell"
    );
    assert!(stub.generations()[0].to_string().contains("/review"));
}

// Catches: preserving literal pasted prompts also disables explicitly typed commands.
#[test]
fn typed_custom_command_selects_its_admitted_operation() {
    let stub = Stub::start(plan());
    let places = Places::new();
    let ego = places.ego(&stub);
    ego.home_file(
        "config.toml",
        "model = \"ollama/qwen3\"\nthinking = \"off\"\nsandbox = \"off\"\n",
    );
    ego.home_file(
        "commands/review.md",
        "TYPED-EXPANDED !`touch typed-command-effect`",
    );
    let result = places
        .ego(&stub)
        .conversing()
        .args(["--mode", "yolo", "--sandbox", "off"])
        .run_pty("/review");
    assert_eq!(result.code, Some(0), "{}", result.stdout);
    assert!(ego.workspace_path().join("typed-command-effect").exists());
    let generations = stub.generations();
    assert_eq!(generations.len(), 1);
    assert!(generations[0].to_string().contains("TYPED-EXPANDED"));
}
