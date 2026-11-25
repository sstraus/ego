use super::*;

struct Environment(&'static [&'static str]);
impl EnvironmentPresence for Environment {
    fn has_nonblank(&self, name: &str) -> bool {
        self.0.contains(&name)
    }
    fn is_set(&self, name: &str) -> bool {
        self.has_nonblank(name)
    }
}

// Catches: an unbound banner reports a retired provider slot as a usable login.
#[test]
fn unbound_startup_ignores_retired_subscription_slot() {
    let home = tempfile::tempdir().unwrap();
    let store = AuthStore::new(home.path());
    let mut effective = named_effective("oauth");
    effective.connection = None;
    effective.connections.clear();
    store
        .put(
            "openai-codex",
            AuthEntry::OAuth {
                access: "synthetic-retired-access".into(),
                refresh: Some("synthetic-retired-refresh".into()),
                expires: Some(0),
                scope_binding: None,
            },
        )
        .unwrap();
    let before = std::fs::read(store.path()).unwrap();
    for (present, expected) in [
        (&[][..], "not signed in · /login"),
        (
            &[ego_ai::provider::OPENAI_CODEX_ACCESS_TOKEN_ENV][..],
            "access token · env token",
        ),
    ] {
        assert_eq!(
            summary_at(
                "openai-codex",
                &effective,
                Some(&store),
                &Environment(present)
            ),
            expected
        );
    }
    assert_eq!(std::fs::read(store.path()).unwrap(), before);
}

// Catches: unbound auth descriptions miss a usable environment source or report the wrong precedence.
#[test]
fn environment_sources_follow_request_precedence() {
    let strategy = CredentialStrategy::Anthropic {
        provider: "anthropic".into(),
        api_key_var: "API_KEY".into(),
        oauth_var: "ACCESS_TOKEN".into(),
    };
    for (present, expected) in [
        (
            &["API_KEY", "ACCESS_TOKEN"][..],
            "access token · environment",
        ),
        (&["API_KEY"][..], "API key · environment"),
    ] {
        assert_eq!(
            describe("anthropic", &strategy, None, &Environment(present)),
            expected
        );
    }
    assert_eq!(
        describe(
            "key-provider",
            &CredentialStrategy::OAuthOrEnv {
                provider: "key-provider".into(),
                var: "API_KEY".into()
            },
            None,
            &Environment(&["API_KEY"])
        ),
        "API key · environment"
    );
    assert_eq!(
        describe("local", &CredentialStrategy::None, None, &Environment(&[])),
        "none (local provider)"
    );
}

// Catches: the banner claims a configured command already authenticated.
#[test]
fn missing_credentials_are_read_only_and_command_login_is_not_effective_auth() {
    assert_eq!(
        describe(
            "provider",
            &CredentialStrategy::OAuth {
                provider: "provider".into()
            },
            Some("/not-executed"),
            &Environment(&[])
        ),
        "not signed in · command login configured · /login"
    );
}

fn named_effective(access: &str) -> Effective {
    ego_core::config::resolve(&[ego_core::config::ConfigLayer::parse(
        ego_core::config::Source::User,
        &format!("connection='work'\n[connections.work]\nname='Work'\nvendor='anthropic'\naccess='{access}'\n{}", if access == "key" { "key_env='WORK_API_KEY'\n" } else { "" }),
    ).unwrap()], ego_core::config::Trust::Untrusted, None).unwrap()
}

// Catches: startup reports the ambient vendor source instead of the declared custom key.
#[test]
fn account_findings_startup_uses_selected_custom_key_and_truthful_remedy() {
    let home = tempfile::tempdir().unwrap();
    let absent = home.path().join("absent");
    let store = AuthStore::new(&absent);
    let effective = named_effective("key");
    for (present, expected) in [
        (
            &["WORK_API_KEY"][..],
            "API key · environment (WORK_API_KEY)",
        ),
        (
            &["ANTHROPIC_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN"][..],
            "missing API key · set WORK_API_KEY for the selected account",
        ),
    ] {
        assert_eq!(
            summary_at("anthropic", &effective, Some(&store), &Environment(present)),
            expected
        );
    }
    assert!(
        !absent.exists(),
        "named key observation must not create a store"
    );
}

// Catches: a missing named OAuth slot appears authenticated by a contradictory legacy slot.
#[test]
fn account_findings_startup_observes_only_named_oauth_without_renewal() {
    let home = tempfile::tempdir().unwrap();
    let store = AuthStore::new(home.path());
    let effective = named_effective("oauth");
    let environment = Environment(&["ANTHROPIC_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN"]);
    let entry = || AuthEntry::OAuth {
        access: "synthetic-access".into(),
        refresh: Some("synthetic-refresh".into()),
        expires: Some(0),
        scope_binding: None,
    };
    store.put("anthropic", entry()).unwrap();
    let before = std::fs::read(store.path()).unwrap();
    assert_eq!(
        summary_at("anthropic", &effective, Some(&store), &environment),
        "not signed in · see 'ego auth status anthropic', then run 'ego auth login anthropic --renew' with --account N if needed"
    );
    assert_eq!(std::fs::read(store.path()).unwrap(), before);
    store.remove("anthropic").unwrap();
    store.put("connection:work", entry()).unwrap();
    let before = std::fs::read(store.path()).unwrap();
    assert_eq!(
        summary_at("anthropic", &effective, Some(&store), &Environment(&[])),
        "OAuth · stored credential"
    );
    assert_eq!(
        std::fs::read(store.path()).unwrap(),
        before,
        "observation must not renew an expired token"
    );
    let mut removed = effective.clone();
    removed.connections.clear();
    assert_eq!(
        summary_at("anthropic", &removed, Some(&store), &environment),
        "unavailable (account selection) · select --account N"
    );
}

// Catches: startup silently picks an editable label or map order, or describes a mismatched route.
#[test]
fn account_findings_startup_refuses_ambiguous_and_wrong_route_sources() {
    let mut effective = named_effective("key");
    effective.connection = None;
    effective
        .connections
        .insert("other".into(), effective.connections["work"].clone());
    assert_eq!(
        summary_at(
            "anthropic",
            &effective,
            None,
            &Environment(&["WORK_API_KEY"])
        ),
        "unavailable (account selection) · select --account N"
    );
    let mut effective = named_effective("oauth");
    effective.connections.get_mut("work").unwrap().vendor = "openai".into();
    assert_eq!(
        summary_at(
            "openai",
            &effective,
            None,
            &Environment(&["OPENAI_API_KEY"])
        ),
        "unavailable · selected account requires route openai-codex"
    );
}
