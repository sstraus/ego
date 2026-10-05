use ego_core::config::{ConfigLayer, Source, Trust, resolve};

// Catches: a higher-precedence layer raises the system max_mode, or the
// declaration order of Mode incorrectly places auto below edits.
#[test]
fn system_max_mode_refuses_widening_in_every_layer() {
    let modes = ["plan", "default", "edits", "auto", "yolo"];
    for (ceiling_index, ceiling) in modes.iter().enumerate() {
        for (selected_index, selected) in modes.iter().enumerate() {
            for source in [
                Source::User,
                Source::Project,
                Source::Local,
                Source::Env,
                Source::AgentFile,
                Source::Flags,
            ] {
                let layers = [
                    ConfigLayer::parse(Source::System, &format!("max_mode='{ceiling}'")).unwrap(),
                    ConfigLayer::parse(source, &format!("mode='{selected}'")).unwrap(),
                ];
                let result = resolve(&layers, Trust::Trusted, None);
                assert_eq!(
                    result.is_ok(),
                    selected_index <= ceiling_index,
                    "{source}, maximum {ceiling}, selected {selected}: {result:?}"
                );
            }
        }
    }
}

// Catches: a system profile or a flag disables the system sandbox floor.
#[test]
fn system_sandbox_floor_binds_profiles_and_flags() {
    for (maximum, allowed) in [
        ("ro", vec!["ro"]),
        ("workspace", vec!["ro", "workspace"]),
        ("off", vec!["ro", "workspace", "off"]),
    ] {
        for selected in ["ro", "workspace", "off"] {
            let layers = [
                ConfigLayer::parse(
                    Source::System,
                    &format!("sandbox='{maximum}'\n[profile.dev]\nsandbox='{selected}'"),
                )
                .unwrap(),
                ConfigLayer::parse(Source::Flags, &format!("sandbox='{selected}'")).unwrap(),
            ];
            assert_eq!(
                resolve(&layers, Trust::Trusted, Some("dev")).is_ok(),
                allowed.contains(&selected)
            );
        }
    }
}

// Catches: system-only ceilings can be set outside system authority,
// or one system declaration erases a stricter independent ceiling.
#[test]
fn system_ceiling_fields_are_exclusive_and_intersect() {
    for source in [
        Source::User,
        Source::Project,
        Source::Local,
        Source::Env,
        Source::AgentFile,
        Source::Flags,
    ] {
        assert!(ConfigLayer::parse(source, "max_mode='yolo'").is_err());
        assert!(ConfigLayer::parse(source, "roots=[]").is_err());
    }
    let layers = [
        ConfigLayer::parse(Source::System, "max_mode='edits'\nroots=[]").unwrap(),
        ConfigLayer::parse(Source::System, "max_mode='yolo'").unwrap(),
        ConfigLayer::parse(Source::Flags, "mode='auto'").unwrap(),
    ];
    assert!(resolve(&layers, Trust::Trusted, None).is_err());
    let effective = resolve(&layers[..2], Trust::Trusted, None).unwrap();
    assert_eq!(effective.root_ceilings, vec![vec![]]);
}
