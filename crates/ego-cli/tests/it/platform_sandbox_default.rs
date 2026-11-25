//! Operator-visible platform defaults, without implicit backend fallback.
use std::process::Command;

// Catches: config/doctor conceal the resolved platform level or its reason,
// or an explicit unsupported Adaptive selection silently becomes Workspace.
#[test]
fn platform_sandbox_default_is_explained_and_explicit_selection_is_preserved() {
    let scratch = tempfile::Builder::new()
        .prefix(".ego-default-")
        .tempdir_in(std::env::current_dir().unwrap())
        .unwrap();
    for name in [
        "home",
        "ego",
        "workspace",
        "tmp",
        "cargo",
        "rustup",
        "github",
    ] {
        std::fs::create_dir(scratch.path().join(name)).unwrap();
    }
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_ego"))
            .args(args)
            .current_dir(scratch.path().join("workspace"))
            .env("HOME", scratch.path().join("home"))
            .env("EGO_HOME", scratch.path().join("ego"))
            .env("TMPDIR", scratch.path().join("tmp"))
            .env("CARGO_HOME", scratch.path().join("cargo"))
            .env("RUSTUP_HOME", scratch.path().join("rustup"))
            .env("GH_CONFIG_DIR", scratch.path().join("github"))
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("EGO_PROFILE")
            .env_remove("EGO_MODEL")
            .output()
            .unwrap()
    };
    let (level, reason) = if cfg!(target_os = "macos") {
        (
            "adaptive",
            "platform default: macOS supports adaptive confinement",
        )
    } else {
        (
            "workspace",
            "platform default: adaptive confinement is unsupported; using workspace",
        )
    };
    let config = run(&["config", "ls", "--effective", "--json"]);
    assert!(
        config.status.success(),
        "{}",
        String::from_utf8_lossy(&config.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&config.stdout).unwrap();
    assert_eq!(value["sandbox"], level);
    assert_eq!(value["sandbox_source"], "default");
    assert_eq!(value["sandbox_reason"], reason);
    let doctor = run(&["doctor"]);
    let report = String::from_utf8(doctor.stdout).unwrap();
    assert!(report.contains(reason), "{report}");
    assert!(
        report.contains(&format!("sandbox\t{level}\tdefault")),
        "{report}"
    );

    std::fs::write(
        scratch.path().join("ego/config.toml"),
        "sandbox='adaptive'\n",
    )
    .unwrap();
    let explicit = run(&["config", "ls", "--effective", "--json"]);
    if cfg!(target_os = "macos") {
        assert!(
            explicit.status.success(),
            "{}",
            String::from_utf8_lossy(&explicit.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&explicit.stdout).unwrap();
        assert_eq!(value["sandbox"], "adaptive");
        assert_eq!(value["sandbox_source"], "user");
        assert!(value.get("sandbox_reason").is_none());
    } else {
        assert!(!explicit.status.success());
        assert!(
            String::from_utf8_lossy(&explicit.stderr)
                .contains("adaptive confinement is currently demonstrated only on macOS")
        );
    }
}
