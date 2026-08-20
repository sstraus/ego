//! Strict config schema, precedence resolution and the trust gate.
//!
//! See `plans/m0a-contracts.md` Step 12 and
//! `ARCHITECTURE.md`.

use ego_core::config::{
    ConfigError, ConfigLayer, Mode, ModelRef, PermCfg, Posture, Source, Sourced, Trust, resolve,
};

fn layer(source: Source, value: ConfigLayer) -> Sourced<ConfigLayer> {
    Sourced { source, value }
}

fn model(name: &str) -> Option<ModelRef> {
    Some(ModelRef(name.to_string()))
}

#[test]
fn config_unknown_top_level_key_names_key_and_source() {
    let err = ConfigLayer::parse(Source::Project, "bogus = 1\n").unwrap_err();
    assert_eq!(
        err,
        ConfigError::UnknownKey {
            key: "bogus".to_string(),
            source: Source::Project,
        }
    );
    let message = err.to_string();
    assert!(
        message.contains("bogus"),
        "message should name the key: {message}"
    );
    assert!(
        message.contains("project"),
        "message should name the source: {message}"
    );
}

#[test]
fn config_unknown_key_in_nested_permissions_table_is_named() {
    let err = ConfigLayer::parse(Source::User, "[permissions]\nbogus = 1\n").unwrap_err();
    assert_eq!(
        err,
        ConfigError::UnknownKey {
            key: "bogus".to_string(),
            source: Source::User,
        }
    );
}

#[test]
fn config_unknown_key_in_profile_table_is_named() {
    let err = ConfigLayer::parse(Source::System, "[profile.ci]\nbogus = 1\n").unwrap_err();
    assert_eq!(
        err,
        ConfigError::UnknownKey {
            key: "bogus".to_string(),
            source: Source::System,
        }
    );
}

#[test]
fn config_known_keys_parse_cleanly() {
    let sourced = ConfigLayer::parse(
        Source::Flags,
        "model = \"anthropic/claude-sonnet-5\"\nmode = \"edits\"\n\n[permissions]\nallow = [\"bash(cargo *)\"]\ndeny = []\n",
    )
    .unwrap();
    assert_eq!(sourced.source, Source::Flags);
    assert_eq!(sourced.value.model, model("anthropic/claude-sonnet-5"));
    assert_eq!(sourced.value.mode, Some(Mode::Edits));
    assert_eq!(
        sourced.value.permissions,
        Some(PermCfg {
            allow: vec!["bash(cargo *)".to_string()],
            deny: vec![],
        })
    );
}

#[test]
fn config_full_precedence_order_flags_beats_env_beats_local_beats_project_beats_user_beats_system()
{
    let all = [
        layer(
            Source::System,
            ConfigLayer {
                model: model("system"),
                ..Default::default()
            },
        ),
        layer(
            Source::User,
            ConfigLayer {
                model: model("user"),
                ..Default::default()
            },
        ),
        layer(
            Source::Project,
            ConfigLayer {
                model: model("project"),
                ..Default::default()
            },
        ),
        layer(
            Source::Local,
            ConfigLayer {
                model: model("local"),
                ..Default::default()
            },
        ),
        layer(
            Source::Env,
            ConfigLayer {
                model: model("env"),
                ..Default::default()
            },
        ),
        layer(
            Source::Flags,
            ConfigLayer {
                model: model("flags"),
                ..Default::default()
            },
        ),
    ];

    assert_eq!(
        resolve(&all, Trust::Trusted, None).unwrap().model,
        model("flags")
    );
    assert_eq!(
        resolve(&all[..5], Trust::Trusted, None).unwrap().model,
        model("env")
    );
    assert_eq!(
        resolve(&all[..4], Trust::Trusted, None).unwrap().model,
        model("local")
    );
    assert_eq!(
        resolve(&all[..3], Trust::Trusted, None).unwrap().model,
        model("project")
    );
    assert_eq!(
        resolve(&all[..2], Trust::Trusted, None).unwrap().model,
        model("user")
    );
    assert_eq!(
        resolve(&all[..1], Trust::Trusted, None).unwrap().model,
        model("system")
    );
}

#[test]
fn config_precedence_is_independent_of_slice_order() {
    let scrambled = [
        layer(
            Source::Flags,
            ConfigLayer {
                model: model("flags"),
                ..Default::default()
            },
        ),
        layer(
            Source::System,
            ConfigLayer {
                model: model("system"),
                ..Default::default()
            },
        ),
        layer(
            Source::Project,
            ConfigLayer {
                model: model("project"),
                ..Default::default()
            },
        ),
    ];
    assert_eq!(
        resolve(&scrambled, Trust::Trusted, None).unwrap().model,
        model("flags")
    );
}

#[test]
fn config_system_deny_ceiling_cannot_be_relaxed_by_flags() {
    let layers = [
        layer(
            Source::System,
            ConfigLayer {
                permissions: Some(PermCfg {
                    allow: vec![],
                    deny: vec!["bash(rm -rf *)".to_string()],
                }),
                ..Default::default()
            },
        ),
        layer(
            Source::Flags,
            ConfigLayer {
                permissions: Some(PermCfg {
                    allow: vec!["bash(rm -rf *)".to_string()],
                    deny: vec![],
                }),
                ..Default::default()
            },
        ),
    ];
    let effective = resolve(&layers, Trust::Trusted, None).unwrap();
    assert!(
        effective
            .permissions
            .deny
            .contains(&"bash(rm -rf *)".to_string())
    );
    assert!(
        !effective
            .permissions
            .allow
            .contains(&"bash(rm -rf *)".to_string())
    );
}

#[test]
fn config_non_system_deny_can_be_relaxed_by_higher_precedence() {
    let layers = [
        layer(
            Source::Project,
            ConfigLayer {
                permissions: Some(PermCfg {
                    allow: vec![],
                    deny: vec!["bash(curl *)".to_string()],
                }),
                ..Default::default()
            },
        ),
        layer(
            Source::Flags,
            ConfigLayer {
                permissions: Some(PermCfg {
                    allow: vec!["bash(curl *)".to_string()],
                    deny: vec![],
                }),
                ..Default::default()
            },
        ),
    ];
    let effective = resolve(&layers, Trust::Trusted, None).unwrap();
    assert!(
        effective
            .permissions
            .allow
            .contains(&"bash(curl *)".to_string())
    );
    assert!(
        !effective
            .permissions
            .deny
            .contains(&"bash(curl *)".to_string())
    );
}

#[test]
fn config_profile_overrides_apply_only_when_active_and_only_within_their_own_layer() {
    let toml = "mode = \"default\"\n\n[profile.ci]\nmode = \"yolo\"\n";
    let project = ConfigLayer::parse(Source::Project, toml).unwrap();
    let user = layer(
        Source::User,
        ConfigLayer {
            mode: Some(Mode::Auto),
            ..Default::default()
        },
    );

    // No active profile: project's base mode wins over user (project > user).
    let effective = resolve(&[project.clone(), user.clone()], Trust::Trusted, None).unwrap();
    assert_eq!(effective.mode, Some(Mode::Default));

    // Active profile "ci": project's profile override wins.
    let effective = resolve(&[project.clone(), user.clone()], Trust::Trusted, Some("ci")).unwrap();
    assert_eq!(effective.mode, Some(Mode::Yolo));

    // A higher-precedence layer without the profile still wins over the
    // lower layer's profile override.
    let flags = layer(
        Source::Flags,
        ConfigLayer {
            mode: Some(Mode::Plan),
            ..Default::default()
        },
    );
    let effective = resolve(&[project, user, flags], Trust::Trusted, Some("ci")).unwrap();
    assert_eq!(effective.mode, Some(Mode::Plan));
}

#[test]
fn config_unknown_profile_name_is_a_hard_error() {
    let toml = "mode = \"default\"\n\n[profile.ci]\nmode = \"yolo\"\n";
    let project = ConfigLayer::parse(Source::Project, toml).unwrap();
    let err = resolve(&[project], Trust::Trusted, Some("nope")).unwrap_err();
    assert_eq!(
        err,
        ConfigError::UnknownProfile {
            name: "nope".to_string(),
        }
    );
}

#[test]
fn config_profile_defined_by_only_one_eligible_layer_still_applies() {
    let toml = "mode = \"default\"\n\n[profile.ci]\nmode = \"yolo\"\n";
    let project = ConfigLayer::parse(Source::Project, toml).unwrap();
    let user = layer(
        Source::User,
        ConfigLayer {
            mode: Some(Mode::Auto),
            ..Default::default()
        },
    );
    // Only `project` defines "ci" — that's enough for the profile to be
    // valid, and it still applies within the layer that defines it.
    let effective = resolve(&[project, user], Trust::Trusted, Some("ci")).unwrap();
    assert_eq!(effective.mode, Some(Mode::Yolo));
}

#[test]
fn config_system_deny_ceiling_survives_profile_override_that_clears_deny() {
    let toml = "[permissions]\ndeny = [\"bash(rm -rf *)\"]\n\n[profile.ci.permissions]\nallow = [\"bash(rm -rf *)\"]\ndeny = []\n";
    let system = ConfigLayer::parse(Source::System, toml).unwrap();
    let effective = resolve(&[system], Trust::Trusted, Some("ci")).unwrap();
    assert!(
        effective
            .permissions
            .deny
            .contains(&"bash(rm -rf *)".to_string())
    );
    assert!(
        !effective
            .permissions
            .allow
            .contains(&"bash(rm -rf *)".to_string())
    );
}

#[test]
fn config_system_deny_ceiling_unions_across_multiple_system_layers() {
    let system_a = layer(
        Source::System,
        ConfigLayer {
            permissions: Some(PermCfg {
                allow: vec![],
                deny: vec!["bash(rm -rf *)".to_string()],
            }),
            ..Default::default()
        },
    );
    let system_b = layer(
        Source::System,
        ConfigLayer {
            permissions: Some(PermCfg {
                allow: vec!["bash(rm -rf *)".to_string()],
                deny: vec!["bash(curl *)".to_string()],
            }),
            ..Default::default()
        },
    );
    let effective = resolve(&[system_a, system_b], Trust::Trusted, None).unwrap();
    assert!(
        effective
            .permissions
            .deny
            .contains(&"bash(rm -rf *)".to_string())
    );
    assert!(
        effective
            .permissions
            .deny
            .contains(&"bash(curl *)".to_string())
    );
    assert!(
        !effective
            .permissions
            .allow
            .contains(&"bash(rm -rf *)".to_string())
    );
}

#[test]
fn config_untrusted_project_and_local_cannot_affect_resolution() {
    let layers = [
        layer(
            Source::Project,
            ConfigLayer {
                model: model("project"),
                mode: Some(Mode::Yolo),
                permissions: Some(PermCfg {
                    allow: vec!["bash(*)".to_string()],
                    deny: vec![],
                }),
                ..Default::default()
            },
        ),
        layer(
            Source::Local,
            ConfigLayer {
                model: model("local"),
                mode: Some(Mode::Yolo),
                ..Default::default()
            },
        ),
    ];
    let effective = resolve(&layers, Trust::Untrusted, None).unwrap();
    assert_eq!(effective.posture, Posture::Bare);
    assert_eq!(effective.model, None);
    assert_eq!(effective.mode, None);
    assert!(effective.permissions.allow.is_empty());
}

#[test]
fn config_untrusted_project_profile_is_not_eligible() {
    let toml = "[profile.ci]\nmode = \"yolo\"\n";
    let project = ConfigLayer::parse(Source::Project, toml).unwrap();
    let err = resolve(&[project], Trust::Untrusted, Some("ci")).unwrap_err();
    assert_eq!(
        err,
        ConfigError::UnknownProfile {
            name: "ci".to_string(),
        }
    );
}

#[test]
fn config_bare_forced_excludes_project_and_local_even_when_trusted() {
    let layers = [
        layer(
            Source::Flags,
            ConfigLayer {
                bare: Some(true),
                ..Default::default()
            },
        ),
        layer(
            Source::Project,
            ConfigLayer {
                mode: Some(Mode::Yolo),
                ..Default::default()
            },
        ),
    ];
    let effective = resolve(&layers, Trust::Trusted, None).unwrap();
    assert_eq!(effective.posture, Posture::Bare);
    assert_eq!(effective.mode, None);
}

#[test]
fn config_unknown_key_containing_backtick_is_named_correctly() {
    let err = ConfigLayer::parse(Source::User, "\"bad`key\" = 1\n").unwrap_err();
    assert_eq!(
        err,
        ConfigError::UnknownKey {
            key: "bad`key".to_string(),
            source: Source::User,
        }
    );
}

#[test]
fn config_untrusted_workspace_resolves_bare_regardless_of_config() {
    let layers = [layer(Source::Project, ConfigLayer::default())];
    let effective = resolve(&layers, Trust::Untrusted, None).unwrap();
    assert_eq!(effective.posture, Posture::Bare);
}

#[test]
fn config_trusted_workspace_allows_ambient() {
    let layers = [layer(Source::Project, ConfigLayer::default())];
    let effective = resolve(&layers, Trust::Trusted, None).unwrap();
    assert_eq!(effective.posture, Posture::Ambient);
}

#[test]
fn config_bare_flag_forces_bare_even_when_trusted() {
    let layers = [layer(
        Source::Flags,
        ConfigLayer {
            bare: Some(true),
            ..Default::default()
        },
    )];
    let effective = resolve(&layers, Trust::Trusted, None).unwrap();
    assert_eq!(effective.posture, Posture::Bare);
}
