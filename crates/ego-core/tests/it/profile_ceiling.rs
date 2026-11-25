//! Host profile clamps reuse the administrator ceiling ordering.
use ego_core::config::{
    ConfigLayer, Mode, SettingProvenance, Source, Trust, resolve_selected,
    resolve_with_profile_ceiling,
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

// Catches a clamped mode or sandbox still naming the profile that asked for
// more, and a kept profile value losing its layer, name or selector.
#[test]
fn profile_ceiling_provenance_names_the_clamp_and_keeps_the_profile() {
    let layers = vec![
        ConfigLayer::parse(
            Source::User,
            "[profile.machine]\nmode='default'\nsandbox='workspace'\n\
             [profile.wide]\nmode='yolo'\nsandbox='off'\n\
             [profile.narrow]\nmode='plan'\nsandbox='ro'",
        )
        .unwrap(),
    ];
    let ceiling =
        resolve_selected(&layers, Trust::Untrusted, Some(("machine", Source::Flags))).unwrap();
    let wide = resolve_with_profile_ceiling(
        &layers,
        Trust::Untrusted,
        Some(("wide", Source::Env)),
        Some(&ceiling),
    )
    .unwrap();
    assert_eq!(wide.mode_provenance, SettingProvenance::ProfileCeiling);
    assert_eq!(wide.sandbox_provenance, SettingProvenance::ProfileCeiling);
    assert_eq!(wide.sandbox_provenance.to_string(), "host profile ceiling");
    let narrow = resolve_with_profile_ceiling(
        &layers,
        Trust::Untrusted,
        Some(("narrow", Source::Env)),
        Some(&ceiling),
    )
    .unwrap();
    let kept = SettingProvenance::Profile {
        source: Source::User,
        name: "narrow".to_owned(),
        selected_by: Some(Source::Env),
    };
    assert_eq!(narrow.mode_provenance, kept);
    assert_eq!(narrow.sandbox_provenance, kept);
    assert_eq!(kept.to_string(), "user profile narrow selected by env");
}

// Catches: an Adaptive ceiling silently replaces an explicit strict selection.
#[test]
fn adaptive_ceiling_preserves_explicit_strict_sandbox() {
    use ego_core::config::resolve;
    for sandbox in ["ro", "workspace"] {
        let layers = vec![
            ConfigLayer::parse(
                Source::User,
                &format!("sandbox='{sandbox}'\n[profile.machine]\nmode='default'"),
            )
            .unwrap(),
        ];
        let mut ceiling = resolve(&layers, Trust::Trusted, Some("machine")).unwrap();
        ceiling.sandbox = Some(Confinement::Adaptive);
        let effective =
            resolve_with_profile_ceiling(&layers, Trust::Trusted, None, Some(&ceiling)).unwrap();
        assert_eq!(effective.sandbox, Some(sandbox.parse().unwrap()));
        assert!(
            effective.profile_warnings.is_empty(),
            "{:?}",
            effective.profile_warnings
        );
        let system = ConfigLayer::parse(Source::System, "sandbox='adaptive'").unwrap();
        let mut capped = layers.clone();
        capped.push(system);
        assert_eq!(
            resolve(&capped, Trust::Trusted, None).unwrap().sandbox,
            Some(sandbox.parse().unwrap())
        );
    }
}
