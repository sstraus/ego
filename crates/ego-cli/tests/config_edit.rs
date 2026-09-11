//! contract for the process/editor half of `ego config edit`.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

struct Fixture {
    home: tempfile::TempDir,
    workspace: tempfile::TempDir,
}

fn reported_candidate(stderr: &[u8]) -> PathBuf {
    let text = String::from_utf8_lossy(stderr);
    let line = text
        .lines()
        .find(|line| line.contains("candidate"))
        .expect("editor candidate path is reported");
    PathBuf::from(
        line.split_whitespace()
            .last()
            .expect("candidate path field"),
    )
}

impl Fixture {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().unwrap(),
            workspace: tempfile::tempdir().unwrap(),
        }
    }

    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ego"));
        command
            .env_clear()
            .env("HOME", self.home.path())
            .env("EGO_HOME", self.home.path())
            .env("PATH", "/usr/bin:/bin")
            .current_dir(self.workspace.path())
            .args(args);
        for (key, value) in env {
            command.env(key, value);
        }
        command.output().unwrap()
    }

    fn spawn(&self, args: &[&str], env: &[(&str, &str)]) -> Child {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ego"));
        command
            .env_clear()
            .env("HOME", self.home.path())
            .env("EGO_HOME", self.home.path())
            .env("PATH", "/usr/bin:/bin")
            .current_dir(self.workspace.path())
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in env {
            command.env(key, value);
        }
        command.spawn().unwrap()
    }

    fn executable(&self, name: &str, body: &str) -> String {
        let path = self.workspace.path().join(name);
        fs::write(&path, body).unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&path, permissions).unwrap();
        path.to_str().unwrap().to_owned()
    }
}

#[test]
fn missing_editor_is_usage_error_and_edit_rejects_other_arguments() {
    let fixture = Fixture::new();
    let missing = fixture.run(&["config", "edit"], &[]);
    assert_eq!(missing.status.code(), Some(2));
    assert!(missing.stdout.is_empty());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("EDITOR"));
    for args in [
        &["config", "edit", "--profile", "ci"] as &[&str],
        &["config", "edit", "--json"],
        &["config", "edit", "extra"],
    ] {
        let output = fixture.run(args, &[("EDITOR", "/bin/true")]);
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn visual_wins_and_lexed_argv_has_candidate_last_without_shell_expansion() {
    let fixture = Fixture::new();
    fs::write(
        fixture.home.path().join("config.toml"),
        "model = \"ollama/qwen3\"\n",
    )
    .unwrap();
    let visual = fixture.executable(
        "visual",
        "#!/bin/sh\nprintf 'visual-start\\n' >&2\nprintf '%s\\n' \"$@\" > argv.txt\nfor candidate do :; done\nprintf '%s\\n' 'model = \"ollama/llama3\"' > \"$candidate\"\n",
    );
    let editor = fixture.executable("editor", "#!/bin/sh\nprintf 'EDITOR-RAN\\n' >&2\nexit 9\n");
    let visual_env = format!("{visual} \"quoted arg\" 'literal $HOME' back\\slash");
    let output = fixture.run(
        &["config", "edit"],
        &[("VISUAL", &visual_env), ("EDITOR", &editor)],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("EDITOR-RAN"));
    assert!(stderr.find("config").unwrap() < stderr.find("visual-start").unwrap());
    let argv = fs::read_to_string(fixture.workspace.path().join("argv.txt")).unwrap();
    assert_eq!(argv.lines().next(), Some("quoted arg"));
    assert_eq!(argv.lines().nth(1), Some("literal $HOME"));
    assert_eq!(argv.lines().nth(2), Some("backslash"));
    assert!(
        argv.lines()
            .last()
            .is_some_and(|line| line.contains("config"))
    );
    assert_eq!(
        fs::read_to_string(fixture.home.path().join("config.toml")).unwrap(),
        "model = \"ollama/llama3\"\n"
    );
}

#[test]
fn unchanged_launch_failure_and_nonzero_editor_clean_up_reported_candidates() {
    let fixture = Fixture::new();
    fs::write(
        fixture.home.path().join("config.toml"),
        "model = \"ollama/qwen3\"\n",
    )
    .unwrap();
    let unchanged = fixture.executable("unchanged", "#!/bin/sh\nexit 0\n");
    let output = fixture.run(&["config", "edit"], &[("EDITOR", &unchanged)]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"unchanged\tuser\tnot_published\n");
    let path = reported_candidate(&output.stderr);
    assert!(!path.exists(), "candidate survived: {}", path.display());

    let nonzero = fixture.executable("nonzero", "#!/bin/sh\nexit 9\n");
    let output = fixture.run(&["config", "edit"], &[("EDITOR", &nonzero)]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let path = reported_candidate(&output.stderr);
    assert!(!path.exists(), "candidate survived: {}", path.display());
    let launch = fixture.run(&["config", "edit"], &[("EDITOR", "/no/such/editor")]);
    assert_eq!(launch.status.code(), Some(1));
    assert!(launch.stdout.is_empty());
    let path = reported_candidate(&launch.stderr);
    assert!(!path.exists(), "candidate survived: {}", path.display());
}

#[test]
fn concurrent_base_change_conflicts_without_overwriting_either_candidate_or_authority() {
    let fixture = Fixture::new();
    let authority = fixture.home.path().join("config.toml");
    fs::write(&authority, "model = \"ollama/qwen3\"\n").unwrap();
    let editor = fixture.executable(
        "conflict-editor",
        "#!/bin/sh\nauthority=\"$1\"\nfor candidate do :; done\nprintf '%s\\n' 'model = \"ollama/gemma3\"' > \"$authority\"\nprintf '%s\\n' 'model = \"ollama/llama3\"' > \"$candidate\"\n",
    );
    let visual = format!("{editor} \"{}\"", authority.display());
    let output = fixture.run(&["config", "edit"], &[("VISUAL", &visual)]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        fs::read_to_string(&authority).unwrap(),
        "model = \"ollama/gemma3\"\n"
    );
    let candidate = reported_candidate(&output.stderr);
    assert!(candidate.exists());
    assert_eq!(
        fs::metadata(candidate).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn invalid_candidate_preserves_recovery_0600() {
    let fixture = Fixture::new();
    fs::write(
        fixture.home.path().join("config.toml"),
        "model = \"ollama/qwen3\"\n",
    )
    .unwrap();
    let invalid = fixture.executable(
        "invalid-editor",
        "#!/bin/sh\nprintf '%s\\n' 'model = [not valid TOML' > \"$1\"\n",
    );
    let output = fixture.run(&["config", "edit"], &[("EDITOR", &invalid)]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let path = reported_candidate(&output.stderr);
    let metadata = fs::metadata(path).unwrap();
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
}

#[cfg(unix)]
#[test]
fn sigint_cancels_editor_and_removes_the_complete_process_tree_and_candidate() {
    let fixture = Fixture::new();
    fs::write(
        fixture.home.path().join("config.toml"),
        "model = \"ollama/qwen3\"\n",
    )
    .unwrap();
    let editor = fixture.executable(
        "long-editor",
        "#!/bin/sh\nset +m\n(sleep 300) </dev/null >/dev/null 2>&1 &\ndescendant=$!\ntmp=\"$PWD/descendant.pid.tmp\"\nprintf '%s\\n' \"$descendant\" > \"$tmp\"\nmv \"$tmp\" \"$PWD/descendant.pid\"\nsleep 300\n",
    );
    let child = fixture.spawn(&["config", "edit"], &[("EDITOR", &editor)]);
    let pid_path = fixture.workspace.path().join("descendant.pid");
    let deadline = Instant::now() + Duration::from_secs(10);
    let descendant = loop {
        if let Ok(pid) = fs::read_to_string(&pid_path) {
            break pid.trim().parse::<u32>().expect("descendant pid");
        }
        assert!(
            Instant::now() < deadline,
            "editor did not publish descendant pid"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let status = Command::new("/bin/kill")
        .args(["-INT", &child.id().to_string()])
        .stderr(Stdio::null())
        .status()
        .expect("send SIGINT");
    assert!(status.success());
    let output = child.wait_with_output().expect("wait for cancelled ego");
    assert_eq!(output.status.code(), Some(130));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("cancel"), "stderr: {stderr}");
    let candidate = reported_candidate(&output.stderr);
    assert!(!candidate.exists());
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let alive = Command::new("/bin/kill")
            .args(["-0", &descendant.to_string()])
            .stderr(Stdio::null())
            .status()
            .expect("probe descendant");
        if !alive.success() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "editor descendant survived cancellation"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
