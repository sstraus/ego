use ego_core::config::{ConfigLayer, Source};

// Catches: validating a child partial threshold against defaults rejects a valid inherited band.
#[test]
fn nested_root_partial_threshold_inherits_parent_without_rejecting_valid_config() {
    let loaded = ConfigLayer::parse(
        Source::User,
        r#"
[jev]
[[jev.roots]]
path = "/workspace"
router = { low = 0.9, high = 0.95 }
[[jev.roots]]
path = "/workspace/project"
router = { low = 0.92 }
"#,
    );
    assert!(
        loaded.is_ok(),
        "child low=0.92 must inherit parent high=0.95: {loaded:?}"
    );
}
