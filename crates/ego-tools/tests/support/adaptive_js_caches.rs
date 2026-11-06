use super::*;

const CASE: &str = "js_caches::adaptive_derives_pnpm_home_but_keeps_npx_and_dlx_readonly";

/// Each child write must match the approved pnpm-home/cache boundary.
fn assert_writes(root: &Path, expectations: &[(PathBuf, bool)]) {
    let policy = admit(&root.join("workspace")).unwrap();
    for (directory, writable) in expectations {
        std::fs::create_dir_all(directory).unwrap();
        let probe = directory.join("probe");
        let result = execute(
            policy.clone(),
            root,
            vec!["/usr/bin/touch".into(), probe.clone().into_os_string()],
        );
        assert_eq!(
            (result.is_ok(), probe.exists()),
            (*writable, *writable),
            "{}: {result:?}",
            probe.display()
        );
    }
}

fn rerun(layout: &str, env: &[(&str, PathBuf)]) {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            &format!("adaptive_admission::{CASE}"),
            "--nocapture",
        ])
        .env("EGO_JS_LAYOUT", layout);
    for (key, value) in env {
        command.env(key, value);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{layout}: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

// Catches: Adaptive omits the pnpm store and npm cache, so pnpm fails with
// ERR_PNPM_STORE_DIR_OPEN_OPERATION_LOCK; or the derivation opens
// npx packages that the host later executes; or ignores
// the operator's relocated PNPM_HOME/XDG/npm cache; or an empty XDG variable
// refuses admission instead of meaning unset as it does for pnpm.
// The approved whole-pnpm-home contract includes engine/global/bin writes.
#[test]
fn adaptive_derives_pnpm_home_but_keeps_npx_and_dlx_readonly() {
    let Some(root) = isolated(CASE) else {
        return;
    };
    let home = root.join("home");
    let defaults = [
        (home.join("Library/pnpm/store/v11/files/00"), true),
        (home.join("Library/pnpm/store/v11/links"), true),
        (home.join("Library/Caches/pnpm/metadata"), true),
        (home.join("Library/Caches/pnpm/dlx"), false),
        (home.join(".npm/_cacache"), true),
        (home.join(".npm/_logs"), true),
        (home.join("Library/pnpm/bin"), true),
        (home.join("Library/pnpm/package-manager-store"), true),
        (home.join(".npm/_npx"), false),
    ];
    match std::env::var("EGO_JS_LAYOUT").as_deref() {
        Ok("relocated") => {
            return assert_writes(
                &root,
                &[
                    (root.join("pnpm-home/store/v11/files/00"), true),
                    (root.join("pnpm-home/store/v11/links"), true),
                    (root.join("xdg-cache/pnpm/metadata"), true),
                    (root.join("xdg-cache/pnpm/dlx"), false),
                    (root.join("npm-cache/_cacache"), true),
                    (root.join("pnpm-home/bin"), true),
                    (root.join("npm-cache/_npx"), false),
                    (home.join("Library/pnpm/store"), false),
                ],
            );
        }
        Ok("empty") => return assert_writes(&root, &defaults),
        _ => {}
    }
    assert_writes(&root, &defaults);
    rerun(
        "relocated",
        &[
            ("PNPM_HOME", root.join("pnpm-home")),
            ("XDG_DATA_HOME", root.join("xdg-data")),
            ("XDG_CACHE_HOME", root.join("xdg-cache")),
            ("npm_config_cache", root.join("npm-cache")),
        ],
    );
    rerun(
        "empty",
        &[
            ("XDG_DATA_HOME", PathBuf::new()),
            ("XDG_CACHE_HOME", PathBuf::new()),
            ("npm_config_cache", PathBuf::new()),
        ],
    );
}
