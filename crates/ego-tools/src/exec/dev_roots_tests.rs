use super::{DevRoots, Toolchain, derive};

fn toolchain(home: &std::path::Path) -> Toolchain {
    Toolchain {
        cargo: home.join(".cargo"),
        rustup: home.join(".rustup"),
    }
}

fn tree() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().canonicalize().unwrap();
    (dir, home)
}

// Catches: the shim, the cache named by the mbx config or the public keys are
// not derived, so cargo through the shim and SSH signing stay blocked.
#[test]
fn derives_the_shim_the_configured_cache_and_public_keys() {
    let (_dir, home) = tree();
    let mbx = home.join("Library/Application Support/mbx");
    std::fs::create_dir_all(mbx.join("bin")).unwrap();
    std::fs::create_dir_all(home.join("cache")).unwrap();
    std::fs::write(
        mbx.join("config.toml"),
        format!("cache_dir = \"{}\"\n", home.join("cache").display()),
    )
    .unwrap();
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    for name in ["id_ed25519", "id_ed25519.pub", "allowed_signers", "config"] {
        std::fs::write(home.join(".ssh").join(name), "x").unwrap();
    }
    let roots = derive(&home, &toolchain(&home), None);
    assert_eq!(roots.exec, vec![mbx.join("bin")]);
    assert_eq!(roots.write, vec![home.join("cache")]);
    assert!(roots.read.contains(&home.join(".ssh/id_ed25519.pub")));
    assert!(roots.read.contains(&home.join(".ssh/allowed_signers")));
    // Catches: a private key or ssh config derived as readable.
    assert!(!roots.read.contains(&home.join(".ssh/id_ed25519")));
    assert!(!roots.read.contains(&home.join(".ssh/config")));
}

// Catches: an absent tool deriving a root, or a cache that holds the home directory.
#[test]
fn derives_nothing_without_the_tools_and_never_the_home_directory() {
    let (_dir, home) = tree();
    assert_eq!(derive(&home, &toolchain(&home), None), DevRoots::default());
    let mbx = home.join("Library/Application Support/mbx");
    std::fs::create_dir_all(mbx.join("bin")).unwrap();
    std::fs::write(
        mbx.join("config.toml"),
        format!("cache_dir = \"{}\"\n", home.display()),
    )
    .unwrap();
    assert!(derive(&home, &toolchain(&home), None).write.is_empty());
}

// Catches: the tuic binary derived for a path that is not a file.
#[test]
fn derives_the_tuic_binary_only_when_it_is_a_file() {
    let (_dir, home) = tree();
    let binary = home.join("tuic");
    std::fs::write(&binary, "x").unwrap();
    assert_eq!(
        derive(&home, &toolchain(&home), Some(&binary)).exec,
        vec![binary.clone()]
    );
    assert!(
        derive(&home, &toolchain(&home), Some(&home))
            .exec
            .is_empty()
    );
}

// Catches: cargo under Workspace cannot run or lock its caches, or the toolchain
// grant reaches the cargo credentials that sit beside the registry.
#[test]
fn derives_the_toolchain_without_cargo_credentials() {
    let (_dir, home) = tree();
    let cargo = home.join(".cargo");
    for dir in ["bin", "registry", "git"] {
        std::fs::create_dir_all(cargo.join(dir)).unwrap();
    }
    std::fs::create_dir_all(home.join(".rustup/toolchains")).unwrap();
    for name in ["config.toml", "credentials.toml", ".package-cache"] {
        std::fs::write(cargo.join(name), "x").unwrap();
    }
    let roots = derive(&home, &toolchain(&home), None);
    assert!(roots.exec.contains(&cargo.join("bin")));
    assert!(roots.exec.contains(&home.join(".rustup/toolchains")));
    assert!(roots.write.contains(&cargo.join("registry")));
    assert!(roots.write.contains(&cargo.join(".package-cache")));
    let all: Vec<_> = roots
        .exec
        .iter()
        .chain(&roots.read)
        .chain(&roots.write)
        .collect();
    assert!(
        all.iter()
            .all(|path| !path.ends_with("credentials.toml") && **path != cargo)
    );
}
