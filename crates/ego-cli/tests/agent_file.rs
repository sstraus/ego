//! RED contract for versioned YAML agent files and `--attach` acquisition.

mod support;

use std::path::Path;
use support::{Ego, Finished, Plan, Reply, Stub, text};

const MAX_FILE_BYTES: usize = 1024 * 1024;
const MAX_FRONTMATTER_BYTES: usize = 64 * 1024;
const MAX_YAML_DEPTH: usize = 16;
const MAX_COLLECTION_ENTRIES: usize = 256;
const MAX_SCALAR_BYTES: usize = 16 * 1024;

fn png_bytes() -> Vec<u8> {
    vec![
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 156, 99, 248, 207, 192, 240,
        31, 0, 5, 0, 1, 255, 137, 153, 61, 29, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ]
}

fn supported_anthropic_facts() -> &'static str {
    r#"
[model."anthropic/fixture-vision".capabilities]
schema_version = 5
max_context_tokens = 128000
max_output_tokens = 4096
max_tools = 16
supports_parallel_tool_calls = false
reasoning_carryover = "unsupported"
reasoning_efforts = "any"
default_reasoning_effort = "off"
cancel = "cooperative"
cache = "none"
transport = "sse"
image_input = { kind = "supported", accepted_media_types = ["png"], max_images = 4, max_bytes_per_image = 65536, max_total_bytes = 65536, max_width = 16384, max_height = 16384, max_pixels = 100000000 }
"#
}

fn run_agent_file(contents: &str) -> (Finished, Stub) {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub);
    let file = ego.file("agent.md", contents);
    let finished = ego
        .args(["--sandbox", "off"])
        .args([
            "--file",
            file.to_str().unwrap(),
            "--model",
            "ollama/qwen3",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--output",
            "json",
        ])
        .run();
    (finished, stub)
}

fn run_agent_file_with_vars(contents: &str, values: &[String]) -> (Finished, Stub) {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub);
    let file = ego.file("agent.md", contents);
    let mut invocation = ego.args(["--sandbox", "off"]).args([
        "--file",
        file.to_str().unwrap(),
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--output",
        "json",
    ]);
    for value in values {
        invocation = invocation.arg("--var").arg(value);
    }
    (invocation.run(), stub)
}

fn valid_agent(body: &str) -> String {
    format!("---\nversion: 1\n---\n{body}")
}

#[test]
fn file_var_and_attach_are_accepted_by_the_run_grammar() {
    let ego = Ego::nowhere();
    let agent = ego.file(
        "agent.md",
        "---\nversion: 1\nvars: [name]\n---\nHello {{name}}.\n",
    );
    let image = ego.file("image.png", "fixture");
    let parsed = ego_cli::cli::parse(vec![
        "ego".into(),
        "run".into(),
        "--file".into(),
        agent.to_str().unwrap().into(),
        "--var".into(),
        "name=maintainer".into(),
        "--attach".into(),
        image.to_str().unwrap().into(),
    ])
    .expect("the final flags are part of the run grammar");
    let debug = format!("{parsed:?}");
    for required in ["agent.md", "image.png"] {
        assert!(
            debug.contains(required),
            "parsed invocation dropped {required}: {debug}"
        );
    }
    assert!(
        !debug.contains("name=maintainer") && !debug.contains("maintainer"),
        "parsed invocation Debug leaked supplied variable value: {debug}"
    );
}

#[test]
fn agent_file_substitutes_only_declared_variables_and_keeps_body_as_prompt() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("done"))],
        ..Plan::default()
    });
    let ego = Ego::against(&stub);
    let file = ego.file("agent.md", "#! /usr/bin/env -S ego run -f\n---\nversion: 1\nmodel: ollama/qwen3\nvars:\n  - name\n---\nHello {{name}}.\n");
    let finished = ego
        .args(["--sandbox", "off"])
        .args([
            "--file",
            file.to_str().unwrap(),
            "--var",
            "name=maintainer",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--output",
            "json",
        ])
        .run();
    assert_eq!(finished.code, Some(0), "stderr: {}", finished.stderr);
    assert_eq!(stub.generations().len(), 1);
    let body = stub.generations().remove(0);
    assert!(
        body.to_string().contains("Hello maintainer."),
        "substituted body missing: {body}"
    );
    assert!(
        !body.to_string().contains("{{name}}"),
        "placeholder survived substitution: {body}"
    );
}

#[test]
fn agent_file_and_argv_prompt_refuse_before_stdin_or_provider() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub);
    let file = ego.file("agent.md", "---\nversion: 1\n---\nfile prompt\n");
    let finished = ego
        .args(["--sandbox", "off"])
        .args([
            "--file",
            file.to_str().unwrap(),
            "--model",
            "ollama/qwen3",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--output",
            "json",
            "argv prompt",
        ])
        .stdin("piped prompt")
        .run();
    assert_eq!(finished.code, Some(2));
    assert!(
        finished.stderr.contains("agent file") || finished.stderr.contains("competing"),
        "missing input conflict: {}",
        finished.stderr
    );
    assert!(
        stub.generations().is_empty(),
        "provider was reached after input conflict"
    );
}

#[test]
fn piped_stdin_is_appended_as_fenced_context_after_an_agent_file_body() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub);
    let file = ego.file("agent.md", "---\nversion: 1\n---\ninstruction\n");
    let finished = ego
        .args(["--sandbox", "off"])
        .args([
            "--file",
            file.to_str().unwrap(),
            "--model",
            "ollama/qwen3",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--output",
            "json",
        ])
        .stdin("upstream context")
        .run();
    assert_eq!(finished.code, Some(0), "stderr: {}", finished.stderr);
    let body = stub
        .generations()
        .into_iter()
        .next()
        .expect("one generation");
    let prompt = body.to_string();
    assert!(prompt.contains("instruction"));
    assert!(prompt.contains("upstream context"));
    assert!(
        prompt.contains("piped input"),
        "stdin was not isolated as context: {body}"
    );
}

#[test]
fn malformed_unknown_duplicate_and_bounded_frontmatter_refuse_without_fallback() {
    for header in [
        "model: ollama/qwen3".to_string(),
        "version: 1\nunknown: true".to_string(),
        "version: 1\nversion: 1".to_string(),
        "version: true".to_string(),
        "version: yes".to_string(),
        "version: 2".to_string(),
        "version: 1\nvars: [name, name]".to_string(),
        "version: 1\nvars: [name]".to_string(),
        "version: 1\nmodel: &anchor value".to_string(),
        "version: 1\nmodel: *anchor".to_string(),
        "version: 1\nmodel: !custom value".to_string(),
        format!("version: 1\nmodel: {}", "x".repeat(16 * 1024 + 1)),
    ] {
        let stub = Stub::start(Plan::default());
        let ego = Ego::against(&stub);
        let file = ego.file("invalid.md", &format!("---\n{header}\n---\nbody\n"));
        let finished = ego
            .args(["--sandbox", "off"])
            .args([
                "--file",
                file.to_str().unwrap(),
                "--model",
                "ollama/qwen3",
                "--session-id",
                support::FIXTURE_SESSION_ID,
                "--output",
                "json",
            ])
            .run();
        assert_eq!(
            finished.code,
            Some(2),
            "invalid agent file was accepted: {header}"
        );
        assert!(
            stub.generations().is_empty(),
            "provider reached for invalid agent file: {header}"
        );
    }
}

#[test]
fn agent_file_parser_enforces_all_admitted_size_and_yaml_structure_bounds() {
    let mut deep = String::from("version: 1\nvars: ");
    for _ in 0..MAX_YAML_DEPTH + 1 {
        deep.push('[');
    }
    deep.push_str("name");
    for _ in 0..MAX_YAML_DEPTH + 1 {
        deep.push(']');
    }

    let collection = (0..MAX_COLLECTION_ENTRIES + 1)
        .map(|index| format!("v{index}"))
        .collect::<Vec<_>>()
        .join(", ");
    let frontmatter = format!("version: 1\n#{}", "x".repeat(MAX_FRONTMATTER_BYTES));
    let cases = [
        (
            "whole-file-over-1-mib",
            format!("{}{}", valid_agent("x"), "x".repeat(MAX_FILE_BYTES)),
        ),
        (
            "frontmatter-over-64-kib",
            format!("---\n{frontmatter}\n---\nbody\n"),
        ),
        ("yaml-depth-over-16", format!("---\n{deep}\n---\nbody\n")),
        (
            "collection-over-256",
            format!("---\nversion: 1\nvars: [{collection}]\n---\nbody\n"),
        ),
        (
            "scalar-over-16-kib",
            format!(
                "---\nversion: 1\nmodel: {}\n---\nbody\n",
                "x".repeat(MAX_SCALAR_BYTES + 1)
            ),
        ),
    ];
    for (label, contents) in cases {
        let (finished, stub) = run_agent_file(&contents);
        assert_eq!(finished.code, Some(2), "accepted parser bound {label}");
        assert!(
            stub.generations().is_empty(),
            "provider reached after {label}"
        );
    }

    let exact_whole_prefix = valid_agent("x");
    let exact_whole = format!(
        "{exact_whole_prefix}{}",
        "x".repeat(MAX_FILE_BYTES - exact_whole_prefix.len())
    );
    let (finished, stub) = run_agent_file(&exact_whole);
    assert_ne!(
        finished.code,
        Some(2),
        "exact whole-file limit was rejected"
    );
    assert_eq!(stub.generations().len(), 1);

    let exact_frontmatter_prefix = "version: 1\n#";
    let exact_frontmatter = format!(
        "---\nversion: 1\n#{}\n---\nbody\n",
        "x".repeat(MAX_FRONTMATTER_BYTES - exact_frontmatter_prefix.len() - 1)
    );
    let (finished, stub) = run_agent_file(&exact_frontmatter);
    assert_ne!(
        finished.code,
        Some(2),
        "exact frontmatter limit was rejected"
    );
    assert_eq!(stub.generations().len(), 1);

    let names = (0..MAX_COLLECTION_ENTRIES)
        .map(|index| format!("v{index}"))
        .collect::<Vec<_>>();
    let exact_collection = format!(
        "---\nversion: 1\nvars: [{}]\n---\n{}\n",
        names.join(", "),
        names
            .iter()
            .map(|name| format!("{{{{{name}}}}}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let values = names
        .iter()
        .map(|name| format!("{name}=ok"))
        .collect::<Vec<_>>();
    let (finished, stub) = run_agent_file_with_vars(&exact_collection, &values);
    assert_ne!(
        finished.code,
        Some(2),
        "exact collection limit was rejected"
    );
    assert_eq!(stub.generations().len(), 1);

    let exact_scalar = format!(
        "---\nversion: 1\nmodel: {}\n---\nbody\n",
        "x".repeat(MAX_SCALAR_BYTES)
    );
    let (finished, stub) = run_agent_file(&exact_scalar);
    assert_ne!(finished.code, Some(2), "exact scalar limit was rejected");
    assert_eq!(stub.generations().len(), 1);
}

#[test]
fn only_one_initial_shebang_is_accepted_and_the_body_remains_literal() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("done"))],
        ..Plan::default()
    });
    let ego = Ego::against(&stub);
    let file = ego.file(
        "shebang.md",
        "#! /usr/bin/env -S ego run -f\n---\nversion: 1\n---\nbody\n",
    );
    let finished = ego
        .args(["--sandbox", "off"])
        .args([
            "--file",
            file.to_str().unwrap(),
            "--model",
            "ollama/qwen3",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--output",
            "json",
        ])
        .run();
    assert_eq!(
        finished.code,
        Some(0),
        "initial shebang was rejected: {}",
        finished.stderr
    );
    let prompt = stub
        .generations()
        .into_iter()
        .next()
        .expect("initial shebang produced one generation")
        .to_string();
    assert!(prompt.contains("body"));
    assert!(
        !prompt.contains("/usr/bin/env"),
        "shebang leaked into the prompt: {prompt}"
    );

    for contents in [
        "#! first\n#! second\n---\nversion: 1\n---\nbody\n",
        "\n#! not-first\n---\nversion: 1\n---\nbody\n",
    ] {
        let (finished, stub) = run_agent_file(contents);
        assert_eq!(
            finished.code,
            Some(2),
            "invalid shebang placement was accepted"
        );
        assert!(stub.generations().is_empty());
    }
}

#[test]
fn variable_declarations_placeholders_and_supplied_values_must_be_equal() {
    for (header, body, supplied, expected_code, expected_generations) in [
        (
            "version: 1\nvars: [name]",
            "Hello {{other}}",
            "name=maintainer",
            2,
            0,
        ),
        (
            "version: 1\nvars: [name]",
            "Hello {{name}}",
            "other=maintainer",
            2,
            0,
        ),
        (
            "version: 1\nvars: [name]",
            "Hello {{name}}",
            "name=maintainer\nname=Again",
            2,
            0,
        ),
        ("version: 1\nvars: [name]", "Hello {{name}}", "name=", 2, 0),
        (
            "version: 1\nvars: [name]",
            "literal body",
            "name=maintainer",
            2,
            0,
        ),
    ] {
        let stub = Stub::start(Plan::default());
        let ego = Ego::against(&stub);
        let file = ego.file("invalid-vars.md", &format!("---\n{header}\n---\n{body}\n"));
        let mut invocation = ego.args(["--sandbox", "off"]).args([
            "--file",
            file.to_str().unwrap(),
            "--model",
            "ollama/qwen3",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--output",
            "json",
        ]);
        for value in supplied.split('\n') {
            invocation = invocation.arg("--var").arg(value);
        }
        let finished = invocation.run();
        if body == "literal body" {
            assert_eq!(
                finished.code,
                Some(2),
                "declared-but-unused variable was accepted"
            );
            assert!(stub.generations().is_empty());
        } else {
            assert_eq!(
                finished.code,
                Some(expected_code),
                "variable disposition was wrong: {header} / {body} / {supplied}"
            );
            assert_eq!(
                stub.generations().len(),
                expected_generations,
                "generation count was wrong: {header} / {body} / {supplied}"
            );
        }
    }
}

#[test]
fn placeholder_set_is_exact_and_substitution_is_literal_single_pass() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("done"))],
        ..Plan::default()
    });
    let ego = Ego::against(&stub);
    let file = ego.file(
        "literal.md",
        "---\nversion: 1\nmodel: ollama/qwen3\nvars: [name]\n---\nHello {{name}} and {{name}}.\n",
    );
    let finished = ego
        .args(["--sandbox", "off"])
        .args([
            "--file",
            file.to_str().unwrap(),
            "--var",
            "name={{other}}",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--output",
            "json",
        ])
        .run();
    assert_eq!(
        finished.code,
        Some(0),
        "literal substitution was rejected: {}",
        finished.stderr
    );
    let body = stub
        .generations()
        .into_iter()
        .next()
        .expect("one generation");
    assert!(body.to_string().contains("Hello {{other}} and {{other}}."));
}

#[test]
fn unknown_image_capability_refuses_before_remote_attachment_fetch() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub).env("ANTHROPIC_API_KEY", "test-key");
    let finished = ego
        .args(["--sandbox", "off"])
        .args([
            "--model",
            "anthropic/no-facts",
            "--attach",
            "https://images.example.invalid/a.png",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--output",
            "json",
            "describe",
        ])
        .run();
    assert_eq!(finished.code, Some(2));
    assert!(
        finished.stderr.contains("image") || finished.stderr.contains("capability"),
        "missing image capability refusal: {}",
        finished.stderr
    );
    assert!(
        stub.requests().is_empty(),
        "attachment URL or provider was contacted before capability refusal"
    );
}

#[test]
fn local_attach_is_read_once_and_projects_to_an_inline_image_without_origin() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub);
    ego.home_file("models.toml", supported_anthropic_facts());
    let image = ego.file("image.png", "");
    std::fs::write(&image, png_bytes()).expect("image fixture is writable");
    let finished = ego
        .env("ANTHROPIC_API_KEY", "test-key")
        .args(["--sandbox", "off"])
        .args([
            "--model",
            "anthropic/fixture-vision",
            "--attach",
            image.to_str().unwrap(),
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--output",
            "json",
            "describe",
        ])
        .run();
    assert_eq!(finished.code, Some(0), "stderr: {}", finished.stderr);
    let body = stub
        .generations()
        .into_iter()
        .next()
        .expect("one generation request");
    let encoded = body.to_string();
    // The Messages spelling of an inlined image: a `base64` source carrying
    // the bytes, rather than the data-URL the chat family uses. The claim is
    // the same one — the bytes travelled, and the path did not.
    assert!(
        encoded.contains(r#""type":"base64""#) && encoded.contains(r#""media_type":"image/png""#),
        "image was not inlined: {body}"
    );
    assert!(
        !encoded.contains(image.to_str().unwrap()),
        "local origin leaked to provider: {body}"
    );
}

#[test]
fn attachment_bounds_and_rejected_schemes_are_usage_errors_before_generation() {
    for attach in [
        "http://example.invalid/a.png",
        "file:///tmp/a.png",
        "data:image/png;base64,AAAA",
        "https://user:pass@example.invalid/a.png#fragment",
    ] {
        let stub = Stub::start(Plan::default());
        let finished = Ego::against(&stub)
            .args(["--sandbox", "off"])
            .args([
                "--model",
                "anthropic/no-facts",
                "--attach",
                attach,
                "--session-id",
                support::FIXTURE_SESSION_ID,
                "--output",
                "json",
                "describe",
            ])
            .run();
        assert_eq!(
            finished.code,
            Some(2),
            "rejected attachment scheme was accepted: {attach}"
        );
        assert!(
            stub.requests().is_empty(),
            "provider reached for rejected attachment: {attach}"
        );
    }
}

#[test]
fn frontmatter_relative_schema_is_relative_to_the_agent_file_not_the_process_cwd() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("{}"))],
        ..Plan::default()
    });
    let ego = Ego::against(&stub);
    let schema_dir = ego.file("agents/schema.json", "{}");
    let file = ego.file(
        "agents/agent.md",
        &format!(
            "---\nversion: 1\nschema: \"@{}\"\n---\nbody\n",
            Path::new(&schema_dir)
                .file_name()
                .unwrap()
                .to_string_lossy()
        ),
    );
    let finished = ego
        .args(["--sandbox", "off"])
        .args([
            "--file",
            file.to_str().unwrap(),
            "--model",
            "ollama/qwen3",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--output",
            "json",
        ])
        .run();
    assert_eq!(
        finished.code,
        Some(0),
        "relative schema should resolve beside agent file: {}",
        finished.stderr
    );
    assert_eq!(stub.generations().len(), 1);
}
