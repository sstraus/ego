//! Compiler contract for the private fields that make permits unforgeable.

use serde_json::Value;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

#[test]
fn permit_authority_fields_are_private() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/ui/permit_fields_are_private.rs");
    cases.compile_fail("tests/ui/permit_construction_is_private.rs");
    cases.compile_fail("tests/ui/authorized_construction_is_private.rs");
    cases.compile_fail("tests/ui/scope_construction_is_private.rs");
}

/// Catches: an external consumer can name or exercise a private authority
/// boundary, or the fixture fails for a reason other than the five rejections.
#[test]
fn permit_boundaries_are_private() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture = root.join("tests/ui/permit_boundaries_are_private.rs");
    let source = std::fs::read(&fixture).expect("the privacy fixture is readable");
    let library = cargo_metadata_artifact(root);
    let mut dependencies = OsString::from("dependency=");
    dependencies.push(library.parent().expect("the artifact has a parent"));
    let mut dependency = OsString::from("ego_core=");
    dependency.push(library);
    let output = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
        .current_dir(root)
        .args([
            "--edition=2024",
            "--emit=metadata",
            "--error-format=json",
            "-o",
            "-",
        ])
        .arg("-L")
        .arg(dependencies)
        .arg("--extern")
        .arg(dependency)
        .arg(&fixture)
        .output()
        .expect("rustc compiles the external fixture");
    assert!(
        !output.status.success(),
        "rustc accepted the privacy fixture"
    );
    let stderr = std::str::from_utf8(&output.stderr).expect("rustc diagnostics are UTF-8");

    // Imported source paths can be readable, remapped, or relative to a different
    // compiler working directory. The consumer's primary diagnostics are the contract.
    let mut rejected = Vec::new();
    let mut error_count = 0;
    for line in stderr.lines() {
        let diagnostic: Value = serde_json::from_str(line).expect("rustc emits JSON diagnostics");
        if diagnostic["level"] != "error" {
            continue;
        }
        let Some(code) = diagnostic["code"]["code"].as_str() else {
            assert!(
                diagnostic["message"]
                    .as_str()
                    .is_some_and(|message| message.starts_with("aborting due to")),
                "unexpected compiler error: {diagnostic}"
            );
            continue;
        };
        error_count += 1;
        let message = diagnostic["message"]
            .as_str()
            .expect("the error has a message");
        let spans = diagnostic["spans"]
            .as_array()
            .expect("the error has a span list");
        for span in spans.iter().filter(|span| span["is_primary"] == true) {
            let path = span["file_name"].as_str().expect("the span names its file");
            assert_eq!(Path::new(path), fixture);
            let start = span["byte_start"].as_u64().expect("the span has a start") as usize;
            let end = span["byte_end"].as_u64().expect("the span has an end") as usize;
            let token = std::str::from_utf8(&source[start..end]).expect("the span covers a token");
            rejected.push((code.to_owned(), token, message.to_owned()));
        }
    }
    let mut expected = [
        ("E0425", "Sealed", "cannot find function, tuple struct or tuple variant `Sealed` in module `ego_core::permit`"),
        ("E0425", "ModeApprover", "cannot find type `ModeApprover` in module `ego_core::permit`"),
        ("E0277", "Permit", "the trait bound `ego_core::permit::Permit: Clone` is not satisfied"),
        ("E0624", "mint", "method `mint` is private"),
        ("E0599", "with_approver", "no method named `with_approver` found for reference `&PolicyEngine` in the current scope"),
    ].map(|(code, token, message)| (code.to_owned(), token, message.to_owned()));
    assert_eq!(error_count, expected.len(), "{stderr}");
    rejected.sort_unstable();
    expected.sort_unstable();
    assert_eq!(rejected, expected, "{stderr}");
}

fn cargo_metadata_artifact(root: &Path) -> PathBuf {
    let output = Command::new("cargo")
        .current_dir(root)
        .args([
            "check",
            "--locked",
            "--offline",
            "--lib",
            "--message-format=json",
        ])
        .output()
        .expect("Cargo checks the ego-core library");
    assert!(
        output.status.success(),
        "Cargo could not check ego-core:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    std::str::from_utf8(&output.stdout)
        .expect("Cargo diagnostics are UTF-8")
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|message| {
            message["reason"] == "compiler-artifact" && message["target"]["name"] == "ego_core"
        })
        .find_map(|message| {
            message["filenames"]
                .as_array()
                .expect("Cargo lists the artifact files")
                .iter()
                .filter_map(Value::as_str)
                .map(Path::new)
                .find(|path| {
                    path.extension()
                        .is_some_and(|extension| extension == "rmeta")
                })
                .map(Path::to_owned)
        })
        .expect("Cargo reports the ego-core metadata artifact")
}
