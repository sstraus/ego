#![cfg(unix)]

//! contract for supervised credential preflight.
//!
//! These tests deliberately run the real `ego` process. A configured
//! credential command is a secret-bearing child protocol, not an ordinary
//! tool invocation: the run deadline and signals must own its complete
//! process tree, while an independent credential failure must not become a
//! run timeout. The fixtures use only local shell processes and bounded
//! marker observation; no provider or external network is contacted.

use crate::support;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ego_cli::auth::{AuthEntry, AuthStore};
use support::{Ego, Finished};
use tempfile::TempDir;

const OPENAI_CODEX_FACTS: &str = r#"
[model."openai-codex/gpt-5.1".capabilities]
schema_version = 6
max_context_tokens = 400000
max_output_tokens = 128000
max_tools = 128
supports_parallel_tool_calls = true
reasoning_carryover = "same_provider_only"
reasoning_efforts = "any"
default_reasoning_effort = "off"
cancel = "cooperative"
cache = "long"
transport = "sse"
"#;

const SECRET: &str = "PREflight-SECRET-MUST-NOT-ESCAPE";
const PREFLIGHT_TIMEOUT: &str = "5s";
/// How long to wait for something that must happen.
///
/// It bounds how quickly a genuine failure is reported and nothing else: both
/// helpers that use it return the moment the condition holds, so a shorter
/// window makes no passing test stricter — it only decides how often a
/// full-workspace run, where thousands of tests compete for the same cores,
/// reports a shell that was merely slow as a shell that never ran.
const OBSERVATION_WINDOW: Duration = Duration::from_secs(15);
const DESCENDANT_LIFETIME_SECS: u64 = 15;
const FOREGROUND_DELAY_SECS: u64 = 10;

struct Fixture {
    _root: TempDir,
    home: PathBuf,
    workspace: PathBuf,
    script: PathBuf,
    direct_pid: PathBuf,
    descendant_pid: PathBuf,
    count: PathBuf,
    store_before: Vec<u8>,
}

impl Fixture {
    fn new(script_body: &str) -> Self {
        let root = tempfile::tempdir().expect("fixture root");
        let home = root.path().join("home");
        let workspace = root.path().join("workspace");
        fs::create_dir_all(&home).expect("fixture home");
        fs::create_dir_all(&workspace).expect("fixture workspace");
        let script = home.join("credential-command.sh");
        let direct_pid = home.join("direct.pid");
        let descendant_pid = home.join("descendant.pid");
        let count = home.join("invocations");
        let script_text = format!(
            "#!/bin/sh\nset -eu\nDIRECT={direct}\nDESCENDANT={descendant}\nCOUNT={count}\n{script_body}",
            direct = shell_quote(&direct_pid),
            descendant = shell_quote(&descendant_pid),
            count = shell_quote(&count),
        );
        fs::write(&script, script_text).expect("credential fixture");
        make_executable(&script);
        fs::write(home.join("models.toml"), OPENAI_CODEX_FACTS).expect("model facts");
        let config = format!(
            "[auth.openai-codex]\ncredential_command = \"{}\"\n",
            script.display()
        );
        fs::write(home.join("config.toml"), config).expect("credential config");
        AuthStore::new(&home)
            .put(
                "openai-codex",
                AuthEntry::OAuth {
                    access: "old-access".into(),
                    refresh: Some("old-refresh".into()),
                    expires: Some(0),
                },
            )
            .expect("seed expired credential");
        let store_before = fs::read(AuthStore::new(&home).path()).expect("seeded store");
        Self {
            _root: root,
            home,
            workspace,
            script,
            direct_pid,
            descendant_pid,
            count,
            store_before,
        }
    }

    fn ego(&self) -> Ego {
        Ego::at(&self.home, &self.workspace, None).args(["--sandbox", "off"])
    }

    fn store_bytes(&self) -> Vec<u8> {
        fs::read(AuthStore::new(&self.home).path()).expect("credential store")
    }

    fn assert_store_unchanged(&self) {
        assert_eq!(
            self.store_bytes(),
            self.store_before,
            "a credential result after the run deadline mutated auth.json"
        );
    }

    fn assert_tree_absent(&self) {
        let direct = fs::read_to_string(&self.direct_pid).expect("direct child marker");
        let descendant = fs::read_to_string(&self.descendant_pid).expect("descendant marker");
        assert_pid_gone(direct.trim());
        assert_pid_gone(descendant.trim());
    }
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("executable fixture");
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}

fn wait_for_marker(path: &Path) {
    let deadline = Instant::now() + OBSERVATION_WINDOW;
    while !path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(path.exists(), "credential fixture did not reach {:?}", path);
}

#[cfg(unix)]
fn assert_pid_gone(pid: &str) {
    let deadline = Instant::now() + OBSERVATION_WINDOW;
    while Instant::now() < deadline {
        let status = std::process::Command::new("/bin/kill")
            .args(["-0", pid])
            .status()
            .expect("kill utility");
        if !status.success() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("credential process {pid} survived bounded cleanup observation");
}

#[cfg(not(unix))]
fn assert_pid_gone(_pid: &str) {}

fn long_refresh_body() -> String {
    let body = r#"
printf '%s\n' "$$" > "$DIRECT"
sleep __DESCENDANT_LIFETIME__ &
printf '%s\n' "$!" > "$DESCENDANT"
printf '%s' 'PREflight-SECRET-MUST-NOT-ESCAPE'
sleep __FOREGROUND_DELAY__
printf '%s\n' '{"access":"new-access","refresh":"new-refresh","expires":0}'
"#;
    body.replace(
        "sleep __DESCENDANT_LIFETIME__",
        &format!("sleep {DESCENDANT_LIFETIME_SECS}"),
    )
    .replace(
        "sleep __FOREGROUND_DELAY__",
        &format!("sleep {FOREGROUND_DELAY_SECS}"),
    )
}

#[cfg(unix)]
#[test]
fn an_expired_run_before_preflight_spawns_no_credential_child() {
    let body = long_refresh_body();
    let fixture = Fixture::new(&body);
    let finished = fixture
        .ego()
        .args([
            "--model",
            "openai-codex/gpt-5.1",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--timeout",
            "0s",
            "Ask",
        ])
        .run();

    assert_eq!(finished.code, Some(124), "stderr: {}", finished.stderr);
    assert!(!fixture.direct_pid.exists(), "credential child was spawned");
    assert!(
        !fixture.descendant_pid.exists(),
        "credential descendant was spawned"
    );
    fixture.assert_store_unchanged();
}

#[cfg(unix)]
#[test]
fn run_deadline_during_main_preflight_cleans_the_credential_process_tree() {
    let body = long_refresh_body();
    let fixture = Fixture::new(&body);
    let running = fixture
        .ego()
        .args([
            "--model",
            "openai-codex/gpt-5.1",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--timeout",
            PREFLIGHT_TIMEOUT,
            "Ask",
        ])
        .spawn();
    wait_for_marker(&fixture.descendant_pid);
    let finished = running.finish();

    assert_eq!(finished.code, Some(124), "stderr: {}", finished.stderr);
    fixture.assert_tree_absent();
    fixture.assert_store_unchanged();
}

#[cfg(unix)]
// Catches: judge preflight escaping the run deadline or retaining credential descendants.
#[test]
fn judge_preflight_uses_the_same_deadline_and_cleans_its_credential_tree() {
    let body = r#"
# Read the refresh request before replying, so exit cannot race ego writing stdin.
cat >/dev/null
count=0
if [ -f "$COUNT" ]; then count=$(cat "$COUNT"); fi
count=$((count + 1))
printf '%s\n' "$count" > "$COUNT"
if [ "$count" -eq 1 ]; then
  printf '%s\n' '{"access":"head.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdF8xIn19.sig","refresh":"judge-refresh","expires":0}'
  exit 0
fi
printf '%s\n' "$$" > "$DIRECT"
sleep __DESCENDANT_LIFETIME__ &
printf '%s\n' "$!" > "$DESCENDANT"
printf '%s' 'PREflight-SECRET-MUST-NOT-ESCAPE'
sleep __FOREGROUND_DELAY__
printf '%s\n' '{"access":"judge-refresh","refresh":"judge-refresh","expires":0}'
"#;
    let body = body
        .replace(
            "sleep __DESCENDANT_LIFETIME__",
            &format!("sleep {DESCENDANT_LIFETIME_SECS}"),
        )
        .replace(
            "sleep __FOREGROUND_DELAY__",
            &format!("sleep {FOREGROUND_DELAY_SECS}"),
        );
    let fixture = Fixture::new(&body);
    let config = "mode = \"auto\"\n[permissions]\njudge = \"openai-codex/gpt-5.1\"\n";
    fs::write(
        fixture.home.join("config.toml"),
        format!(
            "{config}[auth.openai-codex]\ncredential_command = \"{}\"\n",
            fixture.script.display()
        ),
    )
    .expect("judge config");
    // This process has only two sequential credential preflights. Bound its
    // fixture workers instead of spending the five-second deadline starting
    // one worker per host core in both Tokio runtimes under parallel load.
    let mut running = fixture
        .ego()
        .env("TOKIO_WORKER_THREADS", "2")
        .args([
            "--model",
            "openai-codex/gpt-5.1",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--timeout",
            PREFLIGHT_TIMEOUT,
            "Ask",
        ])
        .spawn();
    let observation_deadline = Instant::now() + OBSERVATION_WINDOW;
    while !fixture.descendant_pid.exists() {
        if running
            .child
            .try_wait()
            .expect("observe ego process")
            .is_some()
        {
            let finished = running.finish();
            panic!(
                "ego exited before judge credential startup: {:?}; stderr: {}; invocations: {:?}",
                finished.code,
                finished.stderr,
                fs::read_to_string(&fixture.count)
            );
        }
        assert!(
            Instant::now() < observation_deadline,
            "judge credential startup exceeded bounded observation"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let finished = running.finish();

    assert_eq!(finished.code, Some(124), "stderr: {}", finished.stderr);
    assert_eq!(fs::read_to_string(&fixture.count).unwrap().trim(), "2");
    fixture.assert_tree_absent();
}

#[cfg(unix)]
#[test]
fn sigint_and_sigterm_during_credential_preflight_cleanup_before_return() {
    for (signal, expected_code) in [("INT", 130), ("TERM", 143)] {
        let body = long_refresh_body();
        let fixture = Fixture::new(&body);
        let running = fixture
            .ego()
            .args([
                "--model",
                "openai-codex/gpt-5.1",
                "--session-id",
                support::FIXTURE_SESSION_ID,
                "Ask",
            ])
            .spawn();
        wait_for_marker(&fixture.descendant_pid);
        let pid = running.child.id().to_string();
        let status = std::process::Command::new("/bin/kill")
            .args([&format!("-{signal}"), &pid])
            .status()
            .expect("kill utility");
        assert!(status.success(), "could not send SIG{signal}");
        let finished = running.finish();

        assert_eq!(finished.code, Some(expected_code), "SIG{signal}");
        fixture.assert_tree_absent();
        fixture.assert_store_unchanged();
    }
}

#[cfg(unix)]
#[test]
fn partial_credential_output_is_discarded_and_never_reaches_cli_streams() {
    let body = long_refresh_body();
    let fixture = Fixture::new(&body);
    let Finished {
        code,
        stdout,
        stderr,
        ..
    } = fixture
        .ego()
        .args([
            "--model",
            "openai-codex/gpt-5.1",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--timeout",
            PREFLIGHT_TIMEOUT,
            "Ask",
        ])
        .run();

    assert_eq!(code, Some(124), "stderr: {stderr}");
    assert!(!stdout.contains(SECRET), "secret reached stdout: {stdout}");
    assert!(!stderr.contains(SECRET), "secret reached stderr: {stderr}");
    fixture.assert_store_unchanged();
}

#[cfg(unix)]
#[test]
fn a_credential_nonzero_exit_is_not_relabelled_as_run_timeout() {
    let fixture = Fixture::new(
        r#"
printf '%s\n' "$$" > "$DIRECT"
sleep 5 &
printf '%s\n' "$!" > "$DESCENDANT"
sleep 1
# The fixture reports its own timeout-like nonzero result. This is deliberately
# a mapping test, not evidence for ego's independent PROGRAM_TIMEOUT policy.
exit 124
"#,
    );
    let finished = fixture
        .ego()
        .args([
            "--model",
            "openai-codex/gpt-5.1",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--timeout",
            "10s",
            "Ask",
        ])
        .run();

    assert_ne!(
        finished.code,
        Some(124),
        "command failure became run timeout"
    );
    assert!(
        finished.code.is_some(),
        "credential failure had no exit status"
    );
    fixture.assert_tree_absent();
}

/// Catches: a failing credential helper that shows only its exit code, a
/// helper reason that echoes a secret it printed, and a fixed login timeout
/// that the owner cannot change for a slow browser login.
#[test]
fn helper_failure_shows_masked_reason_and_owner_timeout_allows_slow_login() {
    let login = |script_body: &str, timeout_secs: u64| {
        let root = tempfile::tempdir().expect("fixture root");
        let home = root.path().join("home");
        let workspace = root.path().join("workspace");
        fs::create_dir_all(&home).expect("fixture home");
        fs::create_dir_all(&workspace).expect("fixture workspace");
        let script = home.join("login-helper.sh");
        fs::write(&script, format!("#!/bin/sh\n{script_body}\n")).expect("helper");
        make_executable(&script);
        fs::write(
            home.join("config.toml"),
            format!(
                "[auth.openai-codex]\ncredential_command = \"{}\"\ncredential_timeout_secs = {timeout_secs}\n",
                script.display()
            ),
        )
        .expect("config");
        let started = Instant::now();
        let finished = Ego::at(&home, &workspace, None)
            .subcommand("auth")
            .args(["login", "openai-codex"])
            .run();
        (finished, started.elapsed())
    };

    let (failed, _) = login(
        "printf '{\"access\":\"tok-ABCDEF-123\"}'; echo 'portal said no for tok-ABCDEF-123' >&2; exit 5",
        30,
    );
    assert_ne!(failed.code, Some(0), "{}", failed.stderr);
    assert!(failed.stderr.contains("code 5"), "{}", failed.stderr);
    assert!(
        failed.stderr.contains("portal said no"),
        "{}",
        failed.stderr
    );
    assert!(
        !failed.stderr.contains("tok-ABCDEF-123"),
        "{}",
        failed.stderr
    );

    let (slow, elapsed) = login("sleep 20", 1);
    assert_ne!(slow.code, Some(0), "{}", slow.stderr);
    assert!(slow.stderr.contains("timed out"), "{}", slow.stderr);
    assert!(elapsed < Duration::from_secs(10), "{elapsed:?}");

    let (rejected, _) = login("exit 0", 0);
    assert_ne!(rejected.code, Some(0));
    assert!(
        rejected.stderr.contains("credential_timeout_secs"),
        "{}",
        rejected.stderr
    );
}
