//! Jev policy authority and configuration invariants.
use ego_core::config::{ConfigLayer, JevUse, Source, Trust, resolve};
fn load(text: &str) -> ego_core::config::JevConfig {
    let layer = ConfigLayer::parse(Source::User, text).unwrap();
    resolve(&[layer], Trust::Trusted, None).unwrap().jev
}
// Catches: an uncalibrated shipped use going live or an override affecting siblings.
#[test]
fn shipped_uses_follow_recorded_gates_and_overrides_change_only_the_named_use() {
    let config = load("[jev]");
    for usage in [JevUse::Wake, JevUse::Destructive, JevUse::Router] {
        assert_eq!(config.default_policy(usage).on, usage != JevUse::Wake);
    }
    let config = load(
        "[[jev.roots]]
path='/canonical/LS/**'
wake={on=true}
destructive={on=false}
[[jev.roots]]
path='/canonical/LS/repo'
router={on=false}",
    );
    let paths = vec!["/canonical/LS".into(), "/canonical/LS/repo".into()];
    let selected = config
        .select(std::path::Path::new("/canonical/LS/repo"), &paths)
        .unwrap();
    assert!(selected.policy(JevUse::Wake).on);
    assert!(!selected.policy(JevUse::Router).on);
    assert!(!selected.policy(JevUse::Destructive).on);
}
// Catches: project, local, flags, environment or agent-file redirecting judged text.
#[test]
fn only_system_and_user_can_supply_jev_even_when_layers_are_constructed() {
    for source in [
        Source::Project,
        Source::Local,
        Source::Flags,
        Source::Env,
        Source::AgentFile,
    ] {
        assert!(ConfigLayer::parse(source, "[jev]").is_err());
        let trusted = ConfigLayer::parse(Source::User, "[jev]").unwrap();
        let forged = ego_core::config::Sourced {
            source,
            value: trusted.value,
        };
        assert!(resolve(&[forged], Trust::Trusted, None).is_err());
    }
    assert!(ConfigLayer::parse(Source::System, "[jev]").is_ok());
}
// Catches: unsafe fail-open and silent threshold/config mistakes.
#[test]
fn invalid_fail_modes_bands_and_endpoints_name_the_key() {
    for (text, key) in [
        (
            "[[jev.roots]]
path='/root'
destructive={fail='open'}",
            "destructive.fail",
        ),
        (
            "[[jev.roots]]
path='/root'
wake={low=0.9,high=0.1}",
            "wake",
        ),
        (
            "[[jev.roots]]
path='/root'
router={low=-0.1}",
            "router",
        ),
        (
            "[jev]
endpoint='https://user:password@example.com/decisions'",
            "endpoint",
        ),
    ] {
        assert!(
            ConfigLayer::parse(Source::User, text)
                .unwrap_err()
                .to_string()
                .contains(key),
            "{key}"
        );
    }
    assert_eq!(
        load("[jev]").endpoint.as_str(),
        "https://openrouter.ai/api/alpha/decisions"
    );
}
// Catches: a more specific root incorrectly chosen by declaration order.
#[test]
fn canonical_specificity_wins_independent_of_entry_order() {
    let config = load(
        "[[jev.roots]]
path='/root/deep'
wake={low=0.4,high=0.6}
[[jev.roots]]
path='/root'
wake={low=0.1,high=0.9}",
    );
    let selected = config
        .select(
            std::path::Path::new("/real/deep/sub"),
            &["/real/deep".into(), "/real".into()],
        )
        .unwrap();
    assert_eq!(selected.policy(JevUse::Wake).low.get(), 0.4);
}
