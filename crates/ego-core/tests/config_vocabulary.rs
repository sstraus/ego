//! Vocabulary-only contract for regression case.

use std::path::PathBuf;

use ego_core::ceiling::Budget;
use ego_core::config::{
    ConfigLayer, ManagedConfigKey, ManagedConfigValue, Mode, ModelRef, ProfileName, Source,
    StatusSegmentName, UserConfigTarget,
};
use ego_core::generator::types::ThinkingLevel;
use ego_core::permit::Confinement;
use std::str::FromStr;

#[test]
fn profile_name_is_utf8_byte_bounded_and_quotes_toml_keys() {
    for length in [1, 64] {
        assert!(ProfileName::parse(&"a".repeat(length)).is_ok());
    }
    assert!(ProfileName::parse(&"a".repeat(65)).is_err());
    assert!(ProfileName::parse(&"é".repeat(32)).is_ok());
    assert!(ProfileName::parse(&"é".repeat(33)).is_err());
    for byte in 0u8..=31 {
        let name = format!("dev{}profile", char::from(byte));
        assert!(ProfileName::parse(&name).is_err(), "byte {byte}");
    }
    assert!(ProfileName::parse("dev\u{7f}profile").is_err());
    for whitespace in ['\u{00a0}', '\u{2003}', '\u{3000}'] {
        assert!(ProfileName::parse(&format!("{whitespace}dev")).is_err());
        assert!(ProfileName::parse(&format!("dev{whitespace}")).is_err());
        assert!(ProfileName::parse(&format!("dev{whitespace}profile")).is_ok());
    }
    let name = ProfileName::parse("dev profile").unwrap();
    assert_eq!(name.as_str(), "dev profile");
    let mut document = toml::map::Map::new();
    let mut profiles = toml::map::Map::new();
    profiles.insert(
        name.as_str().to_owned(),
        toml::Value::Table(toml::map::Map::new()),
    );
    document.insert("profile".to_owned(), toml::Value::Table(profiles));
    let encoded = toml::to_string(&toml::Value::Table(document)).unwrap();
    let parsed = ConfigLayer::parse(Source::User, &encoded).unwrap();
    assert!(parsed.value.profile.contains_key("dev profile"));
}

#[test]
fn one_target_type_is_shared_by_base_and_all_profile_scopes() {
    let profile = ProfileName::parse("dev profile").unwrap();
    assert_eq!(UserConfigTarget::Base, UserConfigTarget::Base);
    assert_eq!(
        UserConfigTarget::Profile(profile.clone()),
        UserConfigTarget::Profile(profile)
    );
}

#[test]
fn managed_keys_have_exact_canonical_order_and_target_availability() {
    let base = [
        (ManagedConfigKey::Model, "model"),
        (ManagedConfigKey::Thinking, "thinking"),
        (ManagedConfigKey::Mode, "mode"),
        (ManagedConfigKey::Sandbox, "sandbox"),
        (ManagedConfigKey::Budget, "budget"),
        (ManagedConfigKey::Bare, "bare"),
        (ManagedConfigKey::PermissionsJudge, "permissions.judge"),
        (
            ManagedConfigKey::StatuslineSegments,
            "ui.statusline.segments",
        ),
        (ManagedConfigKey::ExecEnv, "exec.env"),
        (ManagedConfigKey::ExecEnvAllowlist, "exec.env_allowlist"),
        (ManagedConfigKey::Network, "network"),
        (ManagedConfigKey::Roots, "roots"),
    ];
    let profile = [
        (ManagedConfigKey::Model, "model"),
        (ManagedConfigKey::Thinking, "thinking"),
        (ManagedConfigKey::Mode, "mode"),
        (ManagedConfigKey::Sandbox, "sandbox"),
        (ManagedConfigKey::Budget, "budget"),
        (ManagedConfigKey::PermissionsJudge, "permissions.judge"),
        (ManagedConfigKey::ExecEnv, "exec.env"),
        (ManagedConfigKey::ExecEnvAllowlist, "exec.env_allowlist"),
        (ManagedConfigKey::Network, "network"),
        (ManagedConfigKey::Roots, "roots"),
    ];
    let ci = [
        (ManagedConfigKey::Model, "model"),
        (ManagedConfigKey::Thinking, "thinking"),
        (ManagedConfigKey::Mode, "mode"),
        (ManagedConfigKey::Sandbox, "sandbox"),
        (ManagedConfigKey::Budget, "budget"),
        (ManagedConfigKey::PermissionsJudge, "permissions.judge"),
        (ManagedConfigKey::SessionDir, "session_dir"),
        (ManagedConfigKey::ArtifactDir, "artifact_dir"),
        (ManagedConfigKey::ExecEnv, "exec.env"),
        (ManagedConfigKey::ExecEnvAllowlist, "exec.env_allowlist"),
        (ManagedConfigKey::Network, "network"),
        (ManagedConfigKey::Roots, "roots"),
    ];
    for (key, spelling) in base.iter().chain(profile.iter()).chain(ci.iter()) {
        assert_eq!(key.as_str(), *spelling);
    }
    assert_eq!(
        ManagedConfigKey::for_target(UserConfigTarget::Base),
        base.map(|(key, _)| key)
    );
    assert_eq!(
        ManagedConfigKey::for_target(UserConfigTarget::Profile(
            ProfileName::parse("dev").unwrap()
        )),
        profile.map(|(key, _)| key)
    );
    assert_eq!(
        ManagedConfigKey::for_target(UserConfigTarget::Profile(ProfileName::parse("ci").unwrap())),
        ci.map(|(key, _)| key)
    );
}

#[test]
fn managed_values_parse_to_existing_strong_types_and_canonical_toml() {
    let cases = [
        (
            ManagedConfigKey::Model,
            "\"provider/model\"",
            "\"provider/model\"",
            ManagedConfigValue::Model(ModelRef("provider/model".to_owned())),
        ),
        (
            ManagedConfigKey::Thinking,
            "\"high\"",
            "\"high\"",
            ManagedConfigValue::Thinking(ThinkingLevel::High),
        ),
        (
            ManagedConfigKey::Mode,
            "\"edits\"",
            "\"edits\"",
            ManagedConfigValue::Mode(Mode::Edits),
        ),
        (
            ManagedConfigKey::Sandbox,
            "\"workspace\"",
            "\"workspace\"",
            ManagedConfigValue::Sandbox(Confinement::Workspace),
        ),
        (
            ManagedConfigKey::Budget,
            "\"5$\"",
            "\"5$\"",
            ManagedConfigValue::Budget(Budget::from_str("5$").unwrap()),
        ),
        (
            ManagedConfigKey::Bare,
            "true",
            "true",
            ManagedConfigValue::Bare(true),
        ),
        (
            ManagedConfigKey::PermissionsJudge,
            "\"provider/judge\"",
            "\"provider/judge\"",
            ManagedConfigValue::PermissionsJudge(ModelRef("provider/judge".to_owned())),
        ),
        (
            ManagedConfigKey::StatuslineSegments,
            "[\"model\", \"effort\"]",
            "[\"model\", \"effort\"]",
            ManagedConfigValue::StatuslineSegments(vec![
                StatusSegmentName::Model,
                StatusSegmentName::Effort,
            ]),
        ),
        (
            ManagedConfigKey::SessionDir,
            "\"/tmp/sessions\"",
            "\"/tmp/sessions\"",
            ManagedConfigValue::Path(PathBuf::from("/tmp/sessions")),
        ),
        (
            ManagedConfigKey::ArtifactDir,
            "\"/tmp/artifacts\"",
            "\"/tmp/artifacts\"",
            ManagedConfigValue::Path(PathBuf::from("/tmp/artifacts")),
        ),
    ];
    for (key, raw, canonical, expected) in cases {
        let value = ManagedConfigValue::parse(key, raw).unwrap();
        assert_eq!(value, expected, "{key:?}");
        assert_eq!(value.to_toml(), canonical, "{key:?}");
    }
    let model =
        ManagedConfigValue::parse(ManagedConfigKey::Model, "\"future/vendor text\"").unwrap();
    assert_eq!(
        model,
        ManagedConfigValue::Model(ModelRef("future/vendor text".to_owned()))
    );
    assert!(ManagedConfigValue::parse(ManagedConfigKey::Mode, "\"unknown\"").is_err());
    assert!(ManagedConfigValue::parse(ManagedConfigKey::Budget, "\"not-money\"").is_err());
    assert!(
        ManagedConfigValue::parse(
            ManagedConfigKey::StatuslineSegments,
            "[\"model\", \"model\"]"
        )
        .is_err()
    );
    assert!(
        ManagedConfigValue::parse(ManagedConfigKey::StatuslineSegments, "[\"future\"]").is_err()
    );
    for (key, raw) in [
        (ManagedConfigKey::Model, "null"),
        (ManagedConfigKey::Mode, "{}"),
        (ManagedConfigKey::Budget, "[]"),
        (ManagedConfigKey::Bare, "[true]"),
    ] {
        assert!(
            ManagedConfigValue::parse(key, raw).is_err(),
            "{key:?}: {raw}"
        );
    }
    for raw in ["model[0]", "mode.alias", "unknown"] {
        assert!(ManagedConfigKey::parse(raw).is_err(), "{raw}");
    }
}
