//! Host profile clamps reuse the administrator ceiling ordering.
use ego_core::config::{
    ConfigLayer, Mode, Source, Trust, resolve_selected, resolve_with_profile_ceiling,
};
use ego_core::permit::Confinement;

// Catches host widening being refused, accepted, or replacing a narrower request.
#[test]
fn profile_ceiling_clamps_widening_and_keeps_narrower_requests() {
    let layers = vec![
        ConfigLayer::parse(
            Source::User,
            r#"
[profile.machine]
mode = "default"
sandbox = "workspace"
[profile.wide]
mode = "yolo"
sandbox = "off"
[profile.narrow]
mode = "plan"
sandbox = "ro"
"#,
        )
        .unwrap(),
    ];
    let ceiling =
        resolve_selected(&layers, Trust::Untrusted, Some(("machine", Source::Flags))).unwrap();
    for (name, mode, sandbox, warnings) in [
        ("wide", Mode::Default, Confinement::Workspace, 2),
        ("narrow", Mode::Plan, Confinement::Ro, 0),
    ] {
        let effective = resolve_with_profile_ceiling(
            &layers,
            Trust::Untrusted,
            Some((name, Source::Flags)),
            Some(&ceiling),
        )
        .unwrap();
        assert_eq!(effective.mode, Some(mode), "{name}");
        assert_eq!(effective.sandbox, Some(sandbox), "{name}");
        assert_eq!(effective.profile_warnings.len(), warnings, "{name}");
        assert_eq!(effective.max_mode, Some(Mode::Default));
    }
    let unchanged = resolve_with_profile_ceiling(
        &layers,
        Trust::Untrusted,
        Some(("wide", Source::Flags)),
        None,
    )
    .unwrap();
    assert_eq!(unchanged.mode, Some(Mode::Yolo));
    assert_eq!(unchanged.sandbox, Some(Confinement::Off));
    assert!(unchanged.profile_warnings.is_empty());
}

// Catches applying the host clamp after the system refusal or bypassing system caps.
#[test]
fn profile_ceiling_is_applied_before_system_checks_without_raising_them() {
    let layers = vec![
        ConfigLayer::parse(Source::System, "max_mode = 'default'\nsandbox = 'workspace'").unwrap(),
        ConfigLayer::parse(Source::User, "[profile.machine]\nmode='default'\nsandbox='workspace'\n[profile.repo]\nmode='yolo'\nsandbox='off'").unwrap(),
    ];
    let ceiling =
        resolve_selected(&layers, Trust::Untrusted, Some(("machine", Source::Flags))).unwrap();
    let effective = resolve_with_profile_ceiling(
        &layers,
        Trust::Untrusted,
        Some(("repo", Source::Flags)),
        Some(&ceiling),
    )
    .unwrap();
    assert_eq!(effective.mode, Some(Mode::Default));
    assert_eq!(effective.sandbox, Some(Confinement::Workspace));
    assert!(resolve_selected(&layers, Trust::Untrusted, Some(("repo", Source::Flags))).is_err());
    let strict = vec![
        ConfigLayer::parse(Source::System, "max_mode='plan'").unwrap(),
        layers[1].clone(),
    ];
    assert!(resolve_selected(&strict, Trust::Untrusted, Some(("machine", Source::Flags))).is_err());
}
