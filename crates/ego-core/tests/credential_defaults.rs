//! Credential read defaults and their explicit user exception.
use ego_core::config::rule_management::{RuleManagement, RuleOutcome};
use ego_core::config::{
    ConfigLayer, Mode, OriginSet, PermCfg, Ruleset, Source, Sourced, Trust, resolve,
};
use ego_core::permit::rule::Rule;
use ego_core::permit::{
    CanonPath, Classification, Op, PolicyDenied, PolicyEngine, PreparedCall, ProfileRoot,
    RootAccess,
};
use std::path::Path;

fn read(path: &str) -> PreparedCall {
    PreparedCall::new(
        "read",
        Op::Read {
            path: CanonPath::new(path),
        },
    )
}
fn resolved(allow: &[&str], deny: &[&str], source: Source) -> Ruleset {
    resolve(
        &[Sourced {
            source,
            value: ConfigLayer {
                permissions: Some(PermCfg {
                    allow: allow.iter().map(|r| Rule::load(r).unwrap()).collect(),
                    deny: deny.iter().map(|r| Rule::load(r).unwrap()).collect(),
                    ..Default::default()
                }),
                ..Default::default()
            },
        }],
        Trust::Trusted,
        None,
    )
    .unwrap()
    .permissions
}

// Catches credential reads becoming available merely by widening roots or selecting yolo.
#[test]
fn credential_defaults_deny_credentials_in_workspace_and_extra_roots() {
    let rules = resolved(&[], &[], Source::User);
    let engine = PolicyEngine::new(CanonPath::new("/workspace"), Mode::Yolo, rules.clone())
        .with_profile_roots(vec![ProfileRoot::new(
            CanonPath::new("/other"),
            RootAccess::Read,
        )]);
    for base in ["/workspace", "/other"] {
        for file in [
            ".env",
            "nested/.env.local",
            ".ssh/id_ed25519",
            ".aws/credentials",
            ".netrc",
            "Library/Keychains/login.keychain-db",
            "secrets/login.keychain",
        ] {
            let path = format!("{base}/{file}");
            assert!(
                matches!(
                    engine.classify(read(&path)),
                    Classification::Settled(Err(PolicyDenied::Denied {
                        from: Source::Default,
                        ..
                    }))
                ),
                "{path}"
            );
            assert_eq!(
                RuleManagement::explain(&read(&path), &rules, Path::new("/workspace")).outcome,
                RuleOutcome::Denied,
                "{path}"
            );
        }
    }
    assert!(matches!(
        engine.classify(read("/workspace/src/lib.rs")),
        Classification::Ask(_)
    ));
}

// Catches a built-in default overriding an explicit user allow, or project/flag grants lifting it.
#[test]
fn credential_defaults_only_effective_user_allow_can_lift_the_preset() {
    for source in [Source::User, Source::Project, Source::Flags, Source::System] {
        let rules = resolved(&["read(/workspace/.env)"], &[], source);
        let engine = PolicyEngine::new(CanonPath::new("/workspace"), Mode::Yolo, rules.clone());
        let explanation =
            RuleManagement::explain(&read("/workspace/.env"), &rules, Path::new("/workspace"));
        if source == Source::User {
            assert!(matches!(
                engine.classify(read("/workspace/.env")),
                Classification::Settled(Ok(_))
            ));
            assert_eq!(explanation.outcome, RuleOutcome::Allowed);
        } else {
            assert!(
                matches!(
                    engine.classify(read("/workspace/.env")),
                    Classification::Settled(Err(_))
                ),
                "{source}"
            );
            assert_eq!(explanation.outcome, RuleOutcome::Denied);
        }
        assert!(matches!(
            engine.classify(read("/workspace/.env.production")),
            Classification::Settled(Err(_))
        ));
    }
}

// Catches user re-allow bypassing an explicit deny, including the same predicate as a default.
#[test]
fn credential_defaults_user_allow_never_lifts_explicit_denies() {
    for source in [Source::System, Source::User, Source::Project, Source::Flags] {
        for deny in ["read(/workspace/.env)", "read(/**/.env*)"] {
            let mut rules = resolved(&["read(/workspace/.env)"], &[], Source::User);
            Ruleset::merge(&mut rules.deny, Rule::load(deny).unwrap(), source);
            let engine = PolicyEngine::new(CanonPath::new("/workspace"), Mode::Yolo, rules.clone());
            assert!(
                matches!(
                    engine.classify(read("/workspace/.env")),
                    Classification::Settled(Err(_))
                ),
                "{source}: {deny}"
            );
            let explanation =
                RuleManagement::explain(&read("/workspace/.env"), &rules, Path::new("/workspace"));
            assert_eq!(explanation.outcome, RuleOutcome::Denied);
            assert_eq!(explanation.authorities[0].from, source);
        }
    }
    assert_eq!(
        OriginSet::from(vec![Source::Default, Source::System])
            .iter()
            .collect::<Vec<_>>(),
        vec![Source::System, Source::Default]
    );
}
