use super::*;
use ego_cli::auth::{AuthEntry, AuthStore};

// Catches: startup rejects stored-key metadata or the stable account numbers
// written by named login/status. Reaching the prompt does not require Gemini HTTP.
#[test]
fn stored_gemini_key_with_account_numbers_reaches_the_prompt() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub)
        .conversing()
        .without_fixture_thinking()
        .args(["--sandbox", "off", "--mode", "yolo"])
        .env("TERM", "xterm-256color");
    ego.file("AGENTS.md", "# named key startup fixture\n");
    ego.home_file(
        "config.toml",
        "model = 'gemini/gemini-3.8-flash'\nconnection = 'gemini_work'\nthinking = 'low'\n\
         [connections.gemini_work]\nname = 'Gemini Work'\nvendor = 'gemini'\naccess = 'key'\n\
         [account_numbers.gemini]\ngemini_work = 1\n\
         [ui.statusline]\ntemplate = '''READY {{ model }}\n{{ prompt }}'''\n",
    );
    AuthStore::new(ego.home_path())
        .put(
            "connection:gemini_work",
            AuthEntry::ApiKey {
                key: "synthetic-gemini-startup-key".into(),
            },
        )
        .unwrap();
    let (session, mut input, reader) = PtySession::start(&ego, 24, 100, Some((24, 100)));
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_post_trust_prompt(&reader, &mut output, start);
    until_normalized(&reader, &mut output, start, "READY gemini/gemini-3.8-flash");
    input.write_all(b"\x15/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert!(stub.generations().is_empty());
    assert!(!normalized(&output).contains("synthetic-gemini-startup-key"));
}
