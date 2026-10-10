//! Cross-provider judge evidence must keep ordinary workspace paths visible.
//! The shell skeleton is exactly what `request_with_context` sends as
//! `action.structure` when the judge runs on another provider.
use ego_core::permit::shell::observe;

fn skeleton(script: &str) -> String {
    observe(&["/bin/bash".into(), "-c".into(), script.into()]).skeleton
}

// Catches: the scheme-less URL heuristic treats a dot-prefixed relative path as
// a host, so the judge sees a URL instead of the deletion target.
#[test]
fn dot_prefixed_relative_rm_targets_are_not_projected_as_urls() {
    for (script, target) in [
        ("rm -rf ./build/cache", "build/cache"),
        ("rm -rf .github/workflows", ".github/workflows"),
        ("rm -rf ../ego/target", "../ego/target"),
    ] {
        let structure = skeleton(script);
        assert!(!structure.contains("<url:"), "{script}: {structure}");
        assert!(structure.contains(target), "{script}: {structure}");
    }
}

// Catches: the widened entropy fallback (slash and dot allowed) classifies a
// long relative path as a credential and erases the rm target.
#[test]
fn long_relative_paths_are_not_redacted_as_high_entropy_secrets() {
    let structure = skeleton("rm -rf crates/ego-core/src/permit");
    assert!(!structure.contains("<secret:"), "{structure}");
    assert!(structure.contains("crates/ego-core/src/permit"), "{structure}");
}

// Catches: URL projection runs before path classification, so a quoted
// dot-prefixed credential path loses its sensitive marker.
#[test]
fn quoted_dot_prefixed_secret_path_keeps_sensitive_marker() {
    let structure = skeleton("cat './.env'");
    assert!(structure.contains(":sensitive"), "{structure}");
}
