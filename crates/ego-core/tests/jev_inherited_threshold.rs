use ego_core::config::{ConfigLayer, JevUse, Source};
use std::path::Path;

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
    let settings = loaded
        .unwrap()
        .value
        .jev
        .unwrap()
        .select(
            Path::new("/workspace/project"),
            &["/workspace".into(), "/workspace/project".into()],
        )
        .unwrap();
    assert!((settings.policy(JevUse::Router).low.get() - 0.92).abs() < f64::EPSILON);
    assert!((settings.policy(JevUse::Router).high.get() - 0.95).abs() < f64::EPSILON);
}

// Catches: deferring partial-band validation lets an invalid inherited effective band through.
#[test]
fn nested_partial_threshold_refuses_an_invalid_effective_band() {
    let config = ConfigLayer::parse(
        Source::User,
        r#"
[[jev.roots]]
path = "/workspace"
router = { low = 0.2, high = 0.4 }
[[jev.roots]]
path = "/workspace/project"
router = { low = 0.5 }
"#,
    )
    .unwrap()
    .value
    .jev
    .unwrap();
    let error = config
        .select(
            Path::new("/workspace/project"),
            &["/workspace".into(), "/workspace/project".into()],
        )
        .unwrap_err();
    assert!(error.contains("router.low"), "{error}");
}
