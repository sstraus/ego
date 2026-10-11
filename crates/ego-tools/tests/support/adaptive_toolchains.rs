use super::*;

// Catches: whole toolchain homes admit host-executed controls as ordinary caches.
#[test]
fn adaptive_toolchain_controls_are_read_only_while_caches_remain_writable() {
    let Some(root) = isolated(
        "toolchains::adaptive_toolchain_controls_are_read_only_while_caches_remain_writable",
    ) else {
        return;
    };
    for broad in [false, true] {
        if broad {
            // Exercise a canonical alias under an independently writable TMPDIR.
            for name in ["cargo", "rustup"] {
                std::fs::rename(root.join(name), root.join("tmp").join(name)).unwrap();
                std::os::unix::fs::symlink(root.join("tmp").join(name), root.join(name)).unwrap();
            }
        }
        let controls = [
            "cargo/env",
            "cargo/bin/cargo",
            "cargo/config",
            "cargo/config.toml",
            "rustup/settings.toml",
            "rustup/toolchains/stable/bin/rustc",
        ];
        for relative in controls {
            let path = root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"original").unwrap();
        }
        for relative in [
            "cargo/registry/cache",
            "cargo/git/db",
            "rustup/downloads",
            "rustup/tmp",
        ] {
            std::fs::create_dir_all(root.join(relative)).unwrap();
        }
        let policy = admit(&root.join("workspace")).unwrap();
        for relative in controls {
            let path = root.join(relative);
            let result = execute(
                policy.clone(),
                &root,
                vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "printf planted > \"$1\"".into(),
                    "fixture".into(),
                    path.clone().into_os_string(),
                ],
            );
            assert!(
                matches!(result, Err(ego_core::permit::ProcessError::Exit { .. })),
                "host control writable (broad={broad}): {relative}: {result:?}"
            );
            assert_eq!(std::fs::read(path).unwrap(), b"original");
        }
        for relative in [
            "cargo/registry/cache/package",
            "cargo/git/db/object",
            "rustup/downloads/archive",
            "rustup/tmp/partial",
        ] {
            let path = root.join(relative);
            execute(
                policy.clone(),
                &root,
                vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "printf cached > \"$1\"".into(),
                    "fixture".into(),
                    path.clone().into_os_string(),
                ],
            )
            .unwrap();
            assert_eq!(std::fs::read(path).unwrap(), b"cached");
        }
        let missing = root.join("cargo/bin/new-proxy");
        assert!(
            execute(
                policy.clone(),
                &root,
                vec!["/usr/bin/touch".into(), missing.clone().into_os_string()]
            )
            .is_err()
        );
        assert!(!missing.exists());
        let relocated = root.join("tmp/relocated-toolchains");
        assert!(
            execute(
                policy,
                &root,
                vec![
                    "/bin/mv".into(),
                    root.join("rustup/toolchains").into_os_string(),
                    relocated.clone().into_os_string()
                ]
            )
            .is_err()
        );
        assert!(!relocated.exists());
    }
}
