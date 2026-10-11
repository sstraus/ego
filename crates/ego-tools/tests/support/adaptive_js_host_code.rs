use super::*;

// Catches: opening the whole pnpm store and pnpm cache also opens the host's
// own pnpm and `pnpm dlx` packages. pnpm 12 installs itself and dlx tools into
// the global virtual store (`store/v11/links`) and `<cache>/pnpm/dlx`; the host
// shim `~/Library/pnpm/bin/pnpm` execs that code unconfined later.
#[test]
fn adaptive_keeps_pnpm_global_virtual_store_and_dlx_code_readonly() {
    let Some(root) =
        isolated("js_host_code::adaptive_keeps_pnpm_global_virtual_store_and_dlx_code_readonly")
    else {
        return;
    };
    let home = root.join("home");
    let pnpm = home.join("Library/pnpm/store/v11/links/@/pnpm/12.5.1/hash/node_modules/pnpm/bin");
    let dlx = home.join("Library/Caches/pnpm/dlx/hash/stamp/node_modules/knip/bin");
    let files = home.join("Library/pnpm/store/v11/files/00");
    for dir in [&pnpm, &dlx, &files] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let host_code = [pnpm.join("pnpm.cjs"), dlx.join("knip.js")];
    for path in &host_code {
        std::fs::write(path, "original").unwrap();
    }
    let policy = admit(&root.join("workspace")).unwrap();
    let write = |path: &Path| {
        execute(
            policy.clone(),
            &root,
            vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf compromised > \"$1\"".into(),
                "fixture".into(),
                path.as_os_str().to_owned(),
            ],
        )
    };
    let content = files.join("probe");
    assert!(write(&content).is_ok(), "pnpm content store write refused");
    assert_eq!(std::fs::read_to_string(&content).unwrap(), "compromised");
    for path in &host_code {
        let _ = write(path);
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "original",
            "host-executed pnpm code rewritten: {}",
            path.display()
        );
    }
}
