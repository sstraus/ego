use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    stamp();

    if std::env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }

    match std::env::var("CARGO_CFG_TARGET_ENV").as_deref() {
        Ok("msvc") => println!("cargo::rustc-link-arg-bin=ego=/STACK:8388608"),
        Ok("gnu") => println!("cargo::rustc-link-arg-bin=ego=-Wl,--stack,8388608"),
        _ => {}
    }
}

/// Name the build in the binary: the commit it was cut from and its profile.
///
/// `EGO_BUILD_COMMIT` overrides the lookup for a build outside a git checkout
/// (a source archive, a packaging sandbox). Without git and without the override
/// the answer is `unknown`; the build never fails over it.
fn stamp() {
    println!("cargo::rerun-if-env-changed=EGO_BUILD_COMMIT");
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    let commit = std::env::var("EGO_BUILD_COMMIT")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| git(&manifest, &["rev-parse", "--short=9", "HEAD"]))
        .unwrap_or_else(|| "unknown".to_owned());
    println!("cargo::rustc-env=EGO_BUILD_COMMIT={commit}");
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "unknown".to_owned());
    println!("cargo::rustc-env=EGO_BUILD_PROFILE={profile}");

    // A new commit moves HEAD or the branch it names. The index is left out on
    // purpose: staging a file would otherwise rebuild this crate.
    let mut watched = vec![git(&manifest, &["rev-parse", "--git-path", "HEAD"])];
    if let Some(reference) = git(&manifest, &["symbolic-ref", "-q", "HEAD"]) {
        watched.push(git(&manifest, &["rev-parse", "--git-path", &reference]));
    }
    watched.push(git(&manifest, &["rev-parse", "--git-path", "packed-refs"]));
    for path in watched.into_iter().flatten() {
        let path = manifest.join(path);
        if path.exists() {
            println!("cargo::rerun-if-changed={}", path.display());
        }
    }
}

fn git(directory: &Path, arguments: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(directory)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (!text.is_empty()).then_some(text)
}
