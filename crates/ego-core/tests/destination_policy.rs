//! Contract tests for the shared destination-policy diagnostic, without network effects.

use ego_core::config::{ConfigLayer, Effective, Source, Trust, resolve, resolve_selected};
use ego_core::health::perimeter::Perimeter;
use ego_core::permit::{CanonPath, Confinement, RunConfinement};

fn view(effective: &Effective, offline: bool) -> Perimeter {
    let workspace = std::env::current_dir().unwrap().canonicalize().unwrap();
    let confinement =
        RunConfinement::new(Confinement::Off, offline, CanonPath::new(workspace)).unwrap();
    Perimeter::session(&confinement, effective, &[])
}

// Catches: filtering permission declarations loses immutable deny authority or invents a child firewall.
#[test]
fn destination_policy_preserves_rule_authority_and_transport_boundaries() {
    let layers = [
        ConfigLayer::parse(Source::System, "[permissions]\ndeny=['net(BLOCKED.test:443)']").unwrap(),
        ConfigLayer::parse(Source::User, "[permissions]\nallow=['net(API.example.test:443)', 'read(src/**)']\ndeny=['net(blocked.test:443)']").unwrap(),
    ];
    let effective = resolve(&layers, Trust::Untrusted, None).unwrap();
    let perimeter = view(&effective, false);
    let json = serde_json::to_value(&perimeter).unwrap();
    let policy = &json["destination_policy"];
    assert_eq!(json["network"], "online");
    assert_eq!(policy["rules"].as_array().unwrap().len(), 2);
    assert_eq!(policy["rules"][0]["effect"], "deny");
    assert_eq!(policy["rules"][0]["rule"], "net(blocked.test:443)");
    assert_eq!(policy["rules"][0]["source"], "system");
    assert_eq!(policy["rules"][0]["also"], serde_json::json!(["user"]));
    assert_eq!(policy["rules"][1]["effect"], "allow");
    assert_eq!(policy["rules"][1]["rule"], "net(api.example.test:443)");
    assert_eq!(policy["rules"][1]["source"], "user");
    let text = perimeter.text();
    for expected in [
        "destination_deny\tnet(blocked.test:443)\tsystem; also user",
        "destination_allow\tnet(api.example.test:443)\tuser",
        "allow rules are not a default-deny firewall",
        "net rules do not filter child sockets",
        "one fresh DNS result",
        "redirect destination needs fresh policy authorization",
        "net grants do not bypass public web checks",
        "Linux offline needs a supported namespace backend",
    ] {
        assert!(text.contains(expected), "missing {expected:?}: {text}");
    }
    assert!(!text.contains("read(src/**)"));
}

// Catches: inspection shows discarded project grants or base rules replaced by the selected profile.
#[test]
fn destination_policy_uses_only_admitted_selected_profile_rules() {
    let layers = [
        ConfigLayer::parse(Source::User, "[permissions]\nallow=['net(base.test)']\n[profile.docs.permissions]\nallow=['net(docs.test:443)']").unwrap(),
        ConfigLayer::parse(Source::Project, "[permissions]\nallow=['net(untrusted.test)']").unwrap(),
    ];
    let effective =
        resolve_selected(&layers, Trust::Untrusted, Some(("docs", Source::Flags))).unwrap();
    let perimeter = view(&effective, false);
    let json = serde_json::to_value(&perimeter).unwrap();
    assert_eq!(json["destination_policy"]["profile"], "docs");
    let rules = json["destination_policy"]["rules"].as_array().unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0]["rule"], "net(docs.test:443)");
    assert!(perimeter.text().contains("destination_profile\tdocs"));
    assert!(!perimeter.text().contains("untrusted.test"));
    assert!(!perimeter.text().contains("base.test"));
}

// Catches: an allow declaration overrides invocation offline, or an inspection claims measured isolation.
#[test]
fn destination_policy_does_not_turn_declarations_into_offline_capabilities() {
    let effective = resolve(
        &[ConfigLayer::parse(Source::User, "[permissions]\nallow=['net(*)']").unwrap()],
        Trust::Untrusted,
        None,
    )
    .unwrap();
    let perimeter = view(&effective, true);
    let json = serde_json::to_value(&perimeter).unwrap();
    assert_eq!(json["network"], "offline");
    assert_eq!(json["capabilities"], "not_checked");
    assert_eq!(json["destination_policy"]["rules"][0]["rule"], "net(*)");
    assert!(perimeter.measured_capabilities().is_none());
    assert!(perimeter.text().contains("network\toffline\tinvocation"));
    assert!(
        perimeter
            .text()
            .contains("unmeasured capabilities are not a guarantee")
    );
}

// Catches: absence of net grants is mislabeled as a declared default-deny allowlist.
#[test]
fn destination_policy_without_rules_reports_mode_dependent_coverage() {
    let effective = resolve(&[], Trust::Untrusted, None).unwrap();
    let perimeter = view(&effective, false);
    let json = serde_json::to_value(&perimeter).unwrap();
    assert_eq!(json["destination_policy"]["rules"], serde_json::json!([]));
    assert!(perimeter.text().contains("destination_rules\tnone"));
    assert!(
        perimeter
            .text()
            .contains("mode and approval policy decide uncovered destinations")
    );
}
