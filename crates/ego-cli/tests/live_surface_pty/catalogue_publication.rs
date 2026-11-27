//! The real interactive picker and setup project the same configured instance.

use super::{
    Ego, PtySession, catalogue_instance_fixture::save_instance, drain_until_quiet,
    until_normalized, until_post_trust_prompt, until_prompt_from,
};
use std::io::Write;

// Catches: the live picker omits a configured alias or setup ignores the
// instance's declared key source while an interactive seat is active.
#[test]
fn picker_and_setup_show_the_azure_instance() {
    let places = crate::support::Places::new();
    let base = "https://catalogue-fixture.openai.azure.com/openai/v1/";
    save_instance(
        places.home_path(),
        "azure-openai",
        base,
        &format!(
            "vendor = 'azure-openai'\naccess = 'key'\nkey_env = 'OFFICE_AZURE_KEY'\nazure_resource = '{base}'"
        ),
        "deployment-a",
        3072,
        "alternate",
    );
    let ego = Ego::at(places.home_path(), places.workspace_path(), None)
        .conversing()
        .args(["--profile", "chat"])
        .env("OFFICE_AZURE_KEY", "fixture-only")
        .env("TERM", "xterm-256color");
    ego.file("AGENTS.md", "# catalogue fixture\n");
    let (session, mut input, reader) = PtySession::start(&ego, 45, 100, None);
    let mut output = Vec::new();
    until_normalized(&reader, &mut output, 0, "Trust project files");
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_post_trust_prompt(&reader, &mut output, 0);

    let start = output.len();
    input.write_all(b"/model\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "Select model");
    until_normalized(&reader, &mut output, start, "office/alternate");
    let start = output.len();
    input.write_all(b"\x1b").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, start);

    let start = output.len();
    input.write_all(b"/setup\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "Choose where to save");
    input.write_all(b"\r").unwrap();
    input.flush().unwrap();
    until_normalized(
        &reader,
        &mut output,
        start,
        "Setup · Settings · User defaults",
    );
    input.write_all(b"\x1b[F\x1b[A\x1b[A\r").unwrap();
    input.flush().unwrap();
    until_normalized(
        &reader,
        &mut output,
        start,
        "office: credential source present",
    );
    until_normalized(&reader, &mut output, start, "Back");
    let start = output.len();
    input.write_all(b"\r").unwrap();
    input.flush().unwrap();
    until_normalized(
        &reader,
        &mut output,
        start,
        "Setup · Settings · User defaults",
    );
    let start = output.len();
    input.write_all(b"\x1b").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, start);
    drain_until_quiet(&reader, &mut output);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
}
