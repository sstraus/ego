//! Direct owner input is distinct from ambient runtime credentials.
use std::io::Write;
use std::process::{Command, Stdio};

fn command(home: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ego"));
    command
        .env_clear()
        .env("HOME", home)
        .env("EGO_HOME", home)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .current_dir(home);
    command
}

fn invoke(home: &std::path::Path, args: &[&str], input: &str) -> std::process::Output {
    let mut child = command(home)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[cfg(unix)]
fn hidden(home: &std::path::Path, args: &[&str], input: &str, signal: bool) -> (bool, String) {
    hidden_fragment(home, args, input, signal, None)
}

#[cfg(unix)]
fn hidden_fragment(
    home: &std::path::Path,
    args: &[&str],
    input: &str,
    signal: bool,
    prefix: Option<&str>,
) -> (bool, String) {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    use std::io::Read;
    use std::time::Duration;
    let pair = native_pty_system().openpty(PtySize::default()).unwrap();
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_ego"));
    cmd.env_clear();
    cmd.env("HOME", home);
    cmd.env("EGO_HOME", home);
    cmd.env("PATH", std::env::var_os("PATH").unwrap_or_default());
    cmd.env("GEMINI_API_KEY", "ambient-key-not-owner-input");
    cmd.env("OPENROUTER_API_KEY", "ambient-key-not-owner-input");
    cmd.cwd(home);
    cmd.args(args);
    let mut child = pair.slave.spawn_command(cmd).unwrap();
    let pid = child.process_id().unwrap();
    drop(pair.slave);
    let mut killer = child.clone_killer();
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut writer = pair.master.take_writer().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let reading = std::thread::spawn(move || {
        let mut output = Vec::new();
        let mut buffer = [0; 512];
        let mut prompted = false;
        let mut choice_stage = 0;
        while let Ok(n) = reader.read(&mut buffer) {
            if n == 0 {
                break;
            }
            output.extend_from_slice(&buffer[..n]);
            if !prompted
                && String::from_utf8_lossy(&output).contains("(choose 1 or 2, then Enter): ")
            {
                prompted = true;
                let _ = tx.send(2);
            }
            // Catches: the method selector hides digits or does not redraw Backspace.
            let transcript = String::from_utf8_lossy(&output);
            for (stage, suffix) in [
                (0, "(choose 1 or 2, then Enter): 2"),
                (1, "(choose 1 or 2, then Enter): 2\x08 \x08"),
                (2, "(choose 1 or 2, then Enter): 2\x08 \x081"),
                (3, "(choose 1 or 2, then Enter): 2\x08 \x081\x08 \x08"),
                (4, "(choose 1 or 2, then Enter): 2\x08 \x081\x08 \x082"),
                (
                    5,
                    "(choose 1 or 2, then Enter): 2\x08 \x081\x08 \x082\x08 \x081",
                ),
            ] {
                if choice_stage == stage && transcript.contains(suffix) {
                    choice_stage += 1;
                    let _ = tx.send(3 + stage);
                }
            }
            if String::from_utf8_lossy(&output).contains("(input hidden): ") {
                prompted = true;
                let _ = tx.send(1);
            }
        }
        let _ = tx.send(0);
        String::from_utf8_lossy(&output).into_owned()
    });
    let mut prompted = match rx.recv_timeout(Duration::from_secs(30)) {
        Ok(prompted) => prompted,
        Err(_) => {
            let _ = killer.kill();
            let _ = child.wait();
            panic!("hidden prompt did not settle");
        }
    };
    if prompted == 2 {
        // Assert each visible effect before Enter; eventual login success alone
        // would not catch a selector incorrectly using secret input.
        for (input, expected) in [
            (b"2".as_slice(), 3),
            (b"\x7f", 4),
            (b"1", 5),
            (b"\x1b[200~unintended-method-paste\x1b[201~", 6),
            (b"\x1b[200~2\x1b[201~", 7),
            (b"1", 8),
        ] {
            writer.write_all(input).unwrap();
            writer.flush().unwrap();
            let observed = rx.recv_timeout(Duration::from_secs(5));
            if !matches!(observed, Ok(value) if value == expected) {
                let _ = killer.kill();
                let _ = child.wait();
                panic!("method selection feedback missing before Enter: {observed:?}");
            }
        }
        writer.write_all(b"\r").unwrap();
        writer.flush().unwrap();
        prompted = match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(prompted) => prompted,
            Err(_) => {
                let _ = killer.kill();
                let _ = child.wait();
                panic!("API-key prompt did not arrive after method choice");
            }
        };
    }
    if prompted == 1 {
        if let Some(prefix) = prefix {
            writer.write_all(prefix.as_bytes()).unwrap();
            writer.flush().unwrap();
            if rx.recv_timeout(Duration::from_millis(200)).is_ok() {
                let _ = killer.kill();
                let _ = child.wait();
                panic!("invalid input returned before Enter and could expose the unread tail");
            }
        }
        writer.write_all(input.as_bytes()).unwrap();
        writer.flush().unwrap();
        if signal {
            assert!(
                Command::new("kill")
                    .args(["-TERM", &pid.to_string()])
                    .status()
                    .unwrap()
                    .success()
            );
        }
    }
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let waiting = std::thread::spawn(move || {
        let _ = done_tx.send(child.wait());
    });
    let status = match done_rx.recv_timeout(Duration::from_secs(30)) {
        Ok(status) => status.unwrap(),
        Err(_) => {
            let _ = killer.kill();
            waiting.join().unwrap();
            panic!("key input did not settle");
        }
    };
    waiting.join().unwrap();
    drop(writer);
    drop(pair.master);
    (status.success(), reading.join().unwrap())
}

// Catches: explicit login imports an ambient key, alters defaults, or only supports one vendor.
#[cfg(unix)]
#[test]
fn explicit_key_login_persists_owner_input_not_environment() {
    for provider in ["gemini", "openrouter"] {
        let home = tempfile::tempdir().unwrap();
        let key = "synthetic-owner-key-123456";
        let (success, transcript) = hidden(
            home.path(),
            &["auth", "login", provider, "--method", "api-key"],
            &format!("\x1b[200~{key}\x1b[201~\r"),
            false,
        );
        assert!(success, "{transcript}");
        assert!(!transcript.contains(key), "key echoed");
        let config = std::fs::read_to_string(home.path().join("config.toml")).unwrap();
        assert!(!config.contains(key));
        assert!(!config.contains("key_env"));
        assert!(!config.contains("default_connection"));
        let layer =
            ego_core::config::ConfigLayer::parse(ego_core::config::Source::User, &config).unwrap();
        assert_eq!(layer.value.connections.len(), 1);
        let (id, connection) = layer.value.connections.iter().next().unwrap();
        assert_eq!(connection.vendor, provider);
        assert_eq!(connection.access, ego_core::config::ConnectionAccess::Key);
        let entries = ego_cli::auth::AuthStore::new(home.path())
            .read_observed()
            .unwrap();
        match entries.get(&format!("connection:{id}")).unwrap() {
            ego_cli::auth::AuthEntry::ApiKey { key: saved } => {
                assert!(saved.expose_secret_bytes() == key.as_bytes())
            }
            _ => panic!("wrong credential kind"),
        }
        let status = invoke(home.path(), &["auth", "--connection", id, "status"], "");
        assert!(status.status.success());
        assert!(!String::from_utf8_lossy(&status.stdout).contains(key));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(home.path().join("auth.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        if provider == "gemini" {
            use crate::support::{Ego, Plan, Reply, Stub};
            // Catches: a fresh generation uses an ambient key or never resolves
            // the stored credential despite successful status/catalogue output.
            // This transport deliberately refuses the request; no provider
            // response shape or successful remote acceptance is simulated.
            let stub = Stub::start(Plan {
                completions: vec![Reply::Status(400)],
                ..Plan::default()
            });
            let workspace = tempfile::tempdir().unwrap();
            let auth_before = std::fs::read(home.path().join("auth.json")).unwrap();
            for ambient in [None, Some("synthetic-unrelated-ambient-key")] {
                let mut ego = Ego::at(home.path(), workspace.path(), Some(&stub))
                    .env("GEMINI_BASE_URL", &format!("http://{}", stub.address))
                    .args([
                        "--bare",
                        "--no-hooks",
                        "--sandbox",
                        "off",
                        "--timeout",
                        "15s",
                        "--model",
                        "gemini/gemini-3.8-flash",
                        "--output",
                        "json",
                        "Exercise the saved credential.",
                    ]);
                if let Some(ambient) = ambient {
                    ego = ego.env("GEMINI_API_KEY", ambient);
                }
                let before = stub.requests().len();
                let finished = ego.run();
                assert_eq!(finished.code, Some(2), "{}", finished.stderr);
                assert!(
                    finished.stderr.contains("the provider returned HTTP 400"),
                    "unexpected failure before the intended transport rejection: stderr={} stdout={} requests={:?}",
                    finished.stderr,
                    finished.stdout,
                    stub.requests()
                );
                assert!(!finished.stdout.contains(key) && !finished.stderr.contains(key));
                let requests = stub.requests();
                assert_eq!(requests.len(), before + 1, "unexpected extra HTTP request");
                let request = requests.last().unwrap();
                assert_eq!(
                    request.headers.get("x-goog-api-key").map(String::as_str),
                    Some(key),
                    "fresh process must use the exact stored owner key"
                );
                assert!(!request.body.contains(key));
            }
            assert_eq!(
                std::fs::read(home.path().join("auth.json")).unwrap(),
                auth_before
            );
        }
        let logout = invoke(home.path(), &["auth", "--connection", id, "logout"], "");
        assert!(logout.status.success());
        assert!(
            ego_cli::auth::AuthStore::new(home.path())
                .read_observed()
                .unwrap()
                .is_empty()
        );
    }
}

// Catches: invalid input or a mismatched named source publishes partial state.
#[cfg(unix)]
#[test]
fn refused_key_input_leaves_credentials_and_metadata_unchanged() {
    for input in [
        "\r",
        "short\r",
        "\x1b[200~synthetic-key with-whitespace\x1b[201~\r",
        "\x1b[200~synthetic-key\nsecond-line\x1b[201~\r",
    ] {
        let home = tempfile::tempdir().unwrap();
        let (success, _) = hidden(
            home.path(),
            &["auth", "login", "gemini", "--method", "api-key"],
            input,
            false,
        );
        assert!(!success);
        assert!(!home.path().join("auth.json").exists());
        assert!(!home.path().join("config.toml").exists());
    }
    let home = tempfile::tempdir().unwrap();
    let original = "[connections.env]\nname = 'Environment'\nvendor = 'gemini'\naccess = 'key'\nkey_env = 'GEMINI_API_KEY'\n";
    std::fs::write(home.path().join("config.toml"), original).unwrap();
    let result = invoke(
        home.path(),
        &[
            "auth",
            "--connection",
            "env",
            "login",
            "gemini",
            "--method",
            "api-key",
        ],
        "",
    );
    assert!(!result.status.success());
    assert_eq!(
        std::fs::read_to_string(home.path().join("config.toml")).unwrap(),
        original
    );
    assert!(!home.path().join("auth.json").exists());
}

// Catches: key material is accepted from a shell pipeline rather than safe owner entry.
#[test]
fn nonterminal_key_registration_refuses_without_reading_input() {
    let home = tempfile::tempdir().unwrap();
    let output = invoke(
        home.path(),
        &["auth", "login", "gemini", "--method", "api-key"],
        "",
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("interactive terminal"));
    assert!(!home.path().join("auth.json").exists());
}

// Catches: Ctrl-C or a real SIGTERM leaves raw mode active, saves a partial key,
// or hangs because synchronous polling starves the async signal handler.
#[cfg(unix)]
#[test]
fn native_key_prompt_cancels_and_restores_without_echo() {
    for signal in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let key = "synthetic-terminal-key-123456";
        let input = format!(
            "\x1b[200~{key}\x1b[201~{}",
            if signal { "" } else { "\x03" }
        );
        let (success, transcript) = hidden(
            home.path(),
            &["auth", "login", "gemini", "--method", "api-key"],
            &input,
            signal,
        );
        assert!(!success);
        assert!(!transcript.contains(key), "key echoed");
        assert!(
            transcript.contains("\x1b[?2004l"),
            "paste mode not restored"
        );
        assert!(!home.path().join("auth.json").exists());
    }
}

// Catches: a provider offering both methods silently uses OAuth or echoes an
// accidental secret while waiting for the method choice.
#[cfg(unix)]
#[test]
fn dual_method_terminal_choice_echoes_and_edits_before_key_entry() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        "[auth.anthropic]\noauth_login_enabled=true\n",
    )
    .unwrap();
    let key = "synthetic-dual-method-key";
    let (success, transcript) = hidden(
        home.path(),
        &["auth", "login", "anthropic"],
        &format!("\x1b[200~{key}\x1b[201~\r"),
        false,
    );
    assert!(success, "{transcript}");
    assert!(transcript.contains("Login method:"));
    assert!(!transcript.contains("unintended-method-paste"));
    assert!(!transcript.contains(key));
    assert!(
        ego_cli::auth::AuthStore::new(home.path())
            .read_observed()
            .unwrap()
            .values()
            .any(|entry| matches!(entry, ego_cli::auth::AuthEntry::ApiKey { .. }))
    );
}

// Catches: an invalid typed byte exits before the end of a fragmented paste,
// leaving its secret tail to be echoed and read by the parent shell.
#[cfg(unix)]
#[test]
fn invalid_input_stays_hidden_until_the_fragmented_line_is_consumed() {
    let home = tempfile::tempdir().unwrap();
    let tail = "synthetic-tail-must-stay-hidden";
    let (success, transcript) = hidden_fragment(
        home.path(),
        &["auth", "login", "gemini", "--method", "api-key"],
        &format!("{tail}\r"),
        false,
        Some("synthetic "),
    );
    assert!(!success);
    assert!(!transcript.contains(tail));
    assert!(!home.path().join("auth.json").exists());
}

// Catches: the bounded owner-input reader accepts or echoes an overlong key.
#[cfg(unix)]
#[test]
fn oversized_key_is_refused_without_echo_or_publication() {
    let home = tempfile::tempdir().unwrap();
    let key = "s".repeat(4097);
    let (success, transcript) = hidden(
        home.path(),
        &["auth", "login", "gemini", "--method", "api-key"],
        &format!("{key}\r"),
        false,
    );
    assert!(!success);
    assert!(!transcript.contains(&key));
    assert!(!home.path().join("auth.json").exists());
}

#[cfg(unix)]
#[test]
fn provider_account_numbers_do_not_follow_row_order_or_reuse_deleted_slots() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path();
    let cli = |args: &[&str], success: bool| {
        let result = command(path).args(args).output().unwrap();
        let out = format!(
            "{}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(result.status.success(), success, "{args:?}: {out}");
        assert!(!out.contains("conn_"), "internal identity leaked: {out}");
        out
    };
    let login = |args: &[&str], secret, label: &str| {
        let (ok, output) = hidden(path, args, &format!("\x1b[200~{secret}\x1b[201~\r"), false);
        assert!(
            ok && output.contains(label) && !output.contains("conn_"),
            "{output}"
        );
    };
    login(
        &["auth", "login", "gemini"],
        "local-key-one",
        "gemini account 1",
    );
    let discovery = &["models", "--provider", "gemini"];
    assert!(cli(discovery, true).contains("gemini/"));
    login(
        &["auth", "login", "gemini"],
        "local-key-two",
        "gemini account 2",
    );
    assert!(cli(discovery, false).contains("--account N"));
    let selected = &["models", "--provider", "gemini", "--account", "2"];
    assert!(cli(selected, true).contains("gemini/"));
    assert!(
        cli(
            &["models", "--provider", "anthropic", "--account", "2"],
            false
        )
        .contains("no anthropic account 2")
    );
    // Catches: --renew reports success but keeps the old bytes, creates a new
    // identity, or rotates another account instead of the selected stable one.
    let stored_keys = || {
        ego_cli::auth::AuthStore::new(path)
            .read_observed()
            .unwrap()
            .into_iter()
            .map(|(id, entry)| match entry {
                ego_cli::auth::AuthEntry::ApiKey { key } => {
                    (id, key.expose_secret_bytes().to_vec())
                }
                _ => panic!("key fixture unexpectedly contains an OAuth entry"),
            })
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    let mut expected_keys = stored_keys();
    let renewed_id = expected_keys
        .iter()
        .find(|(_, key)| key.as_slice() == b"local-key-two")
        .map(|(id, _)| id.clone())
        .expect("account 2 was registered");
    login(
        &["auth", "login", "gemini", "--renew", "--account", "2"],
        "local-explicitly-rotated-two",
        "gemini account 2",
    );
    expected_keys.insert(renewed_id.clone(), b"local-explicitly-rotated-two".to_vec());
    assert_eq!(
        stored_keys(),
        expected_keys,
        "targeted renewal changed another account"
    );
    assert!(cli(&["auth", "remove", "gemini"], false).contains("--account N"));
    cli(&["auth", "remove", "gemini", "--account", "1"], true);
    assert!(cli(discovery, true).contains("gemini/"));
    let status = cli(&["auth", "status", "gemini"], true);
    assert!(
        status.contains("gemini account 2") && !status.contains("account 1"),
        "{status}"
    );
    expected_keys = stored_keys();
    login(
        &["auth", "login", "gemini", "--renew"],
        "local-rotated-two",
        "gemini account 2",
    );
    expected_keys.insert(renewed_id, b"local-rotated-two".to_vec());
    assert_eq!(
        stored_keys(),
        expected_keys,
        "sole-account renewal changed identity"
    );
    login(
        &["auth", "login", "gemini"],
        "local-key-three",
        "gemini account 3",
    );
    assert!(
        cli(&["models", "--provider", "gemini", "--account", "1"], false)
            .contains("no gemini account 1")
    );
}

#[test]
fn provider_account_numbers_migrate_legacy_names_once_and_ignore_renames() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("config.toml");
    std::fs::write(&config, "[connections.alpha]\nname='Work'\nvendor='gemini'\naccess='key'\n[connections.omega]\nname='Home'\nvendor='gemini'\naccess='key'\n").unwrap();
    let status = || {
        let output = command(home.path())
            .args(["auth", "status", "gemini"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    let before = status();
    assert!(before.contains("gemini account 2: Home"), "{before}");
    let text = std::fs::read_to_string(&config).unwrap();
    assert!(text.contains("[account_numbers.gemini]"), "{text}");
    std::fs::write(&config, text.replace("'Home'", "'Renamed'")).unwrap();
    let after = status();
    assert!(after.contains("gemini account 2: Renamed"), "{after}");
}
