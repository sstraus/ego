use super::*;

// Catches: allowing operator pnpm home also opens dlx code outside that home.
#[test]
fn adaptive_keeps_dlx_code_outside_pnpm_home_readonly() {
    let name = "js_host_code::adaptive_keeps_dlx_code_outside_pnpm_home_readonly";
    let Some(root) = isolated(name) else {
        return;
    };
    let dlx = root.join("home/Library/Caches/pnpm/dlx/hash/node_modules/knip/bin/knip.js");
    std::fs::create_dir_all(dlx.parent().unwrap()).unwrap();
    std::fs::write(&dlx, "original").unwrap();
    let policy = admit(&root.join("workspace")).unwrap();
    let _ = execute(
        policy,
        &root,
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf compromised > \"$1\"".into(),
            "fixture".into(),
            dlx.clone().into_os_string(),
        ],
    );
    assert_eq!(std::fs::read_to_string(dlx).unwrap(), "original");
}
