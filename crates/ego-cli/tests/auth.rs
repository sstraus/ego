//! Subscription credentials are one locked store with two operator plug points.

use std::collections::BTreeMap;
use std::fs;

use ego_ai::provider::ProviderRow;
use ego_cli::auth::{
    AuthConfig, AuthEntry, AuthError, AuthStore, CredentialSource, StoreCredentials,
    discover_plugin, overlay_headers, run_program, select_source,
};
use ego_cli::cli::{self, AuthAction, Invocation};
use tempfile::tempdir;

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};

fn oauth(access: &str) -> AuthEntry {
    AuthEntry::OAuth {
        access: access.to_string().into(),
        refresh: Some("refresh-secret".to_string().into()),
        expires: Some(4_102_444_800_000),
    }
}

#[test]
fn the_store_is_one_map_and_secrets_never_format() {
    let dir = tempdir().unwrap();
    let store = AuthStore::new(dir.path());
    store.put("openai-codex", oauth("access-secret")).unwrap();

    let entries = store.read().unwrap();
    assert!(entries.contains_key("openai-codex"));
    let printed = format!("{entries:?}");
    assert!(!printed.contains("access-secret"), "{printed}");
    assert!(!printed.contains("refresh-secret"), "{printed}");
}

#[cfg(unix)]
#[test]
fn the_store_directory_and_file_have_private_modes() {
    let root = tempdir().unwrap();
    let home = root.path().join("ego");
    let store = AuthStore::new(&home);
    store.put("openai-codex", oauth("secret")).unwrap();

    assert_eq!(fs::metadata(&home).unwrap().mode() & 0o777, 0o700);
    assert_eq!(fs::metadata(store.path()).unwrap().mode() & 0o777, 0o600);
}

#[test]
fn corrupt_store_bytes_are_refused_and_not_replaced() {
    let dir = tempdir().unwrap();
    let store = AuthStore::new(dir.path());
    fs::write(store.path(), b"not json").unwrap();

    let err = store.put("openai-codex", oauth("secret")).unwrap_err();
    assert!(matches!(err, AuthError::CorruptStore { .. }), "{err:?}");
    assert_eq!(fs::read(store.path()).unwrap(), b"not json");
}

#[test]
fn read_modify_write_keeps_updates_from_both_writers() {
    let dir = tempdir().unwrap();
    let first = AuthStore::new(dir.path());
    let second = first.clone();
    let a = std::thread::spawn(move || first.put("openai-codex", oauth("one")));
    let b = std::thread::spawn(move || second.put("nous", oauth("two")));
    a.join().unwrap().unwrap();
    b.join().unwrap().unwrap();

    let entries = AuthStore::new(dir.path()).read().unwrap();
    assert_eq!(entries.len(), 2, "{entries:?}");
}

#[cfg(unix)]
#[test]
fn home_plugin_wins_over_path_plugin_and_must_be_executable() {
    let root = tempdir().unwrap();
    let home = root.path().join("home");
    let bin = root.path().join("bin");
    fs::create_dir_all(home.join("plugins/auth")).unwrap();
    fs::create_dir_all(&bin).unwrap();
    let local = home.join("plugins/auth/nous");
    let path = bin.join("ego-auth-nous");
    fs::write(&local, "#!/bin/sh\n").unwrap();
    fs::write(&path, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&local, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();

    assert_eq!(discover_plugin("nous", &home, Some(&bin)), Some(local));
}

#[test]
fn command_then_plugin_then_builtin_then_environment_is_the_order() {
    let plugin = std::path::PathBuf::from("/tmp/ego-auth-nous");
    assert_eq!(
        select_source(
            "nous",
            Some("mint-token"),
            Some(plugin.clone()),
            true,
            Some("NOUS_KEY")
        ),
        CredentialSource::Command("mint-token".into())
    );
    assert_eq!(
        select_source("nous", None, Some(plugin.clone()), true, Some("NOUS_KEY")),
        CredentialSource::Plugin(plugin)
    );
    assert_eq!(
        select_source("nous", None, None, true, Some("NOUS_KEY")),
        CredentialSource::BuiltIn("nous".into())
    );
    assert_eq!(
        select_source("custom", None, None, false, Some("CUSTOM_KEY")),
        CredentialSource::Environment("CUSTOM_KEY".into())
    );
}

#[test]
fn header_overlay_is_case_insensitive_and_empty_deletes() {
    let defaults = vec![
        ("originator".to_string(), "ego".to_string()),
        ("User-Agent".to_string(), "ego/0.1".to_string()),
    ];
    let overlay = BTreeMap::from([
        ("ORIGINATOR".to_string(), "operator".to_string()),
        ("user-agent".to_string(), String::new()),
    ]);

    assert_eq!(
        overlay_headers(&defaults, &overlay).unwrap(),
        vec![("ORIGINATOR".to_string(), "operator".to_string())]
    );
}

#[test]
fn credential_and_wire_header_overrides_are_refused_at_load() {
    for name in [
        "authorization",
        "X-API-Key",
        "content-type",
        "Accept",
        "OpenAI-Beta",
        "chatgpt-account-id",
    ] {
        let text = format!("[auth.openai-codex.headers]\n\"{name}\" = \"wrong\"\n");
        let err = AuthConfig::parse(&text).unwrap_err();
        assert!(err.to_string().contains(name), "{name}: {err}");
    }
}

#[test]
fn auth_config_accepts_the_same_top_level_document_generation_reads() {
    let config = AuthConfig::parse(
        "model = \"openai/gpt-5\"\n[auth.openai-codex]\ncredential_command = \"mint-token\"\n",
    )
    .unwrap();

    assert_eq!(
        config
            .auth
            .get("openai-codex")
            .and_then(|provider| provider.credential_command.as_deref()),
        Some("mint-token")
    );
}

#[test]
fn every_selectable_subscription_row_resolves_from_the_one_store_or_names_login() {
    const CODEX_TOKEN: &str = "head.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdF8xIn19.sig";
    let root = tempdir().unwrap();
    let store = AuthStore::new(root.path());
    let credentials = StoreCredentials::new(root.path());
    let rows = [
        ProviderRow::openai_codex_default(),
        ProviderRow::github_copilot_default(),
        ProviderRow::xai_oauth_default(),
        ProviderRow::kimi_coding_default(),
        ProviderRow::qwen_oauth_default(),
        ProviderRow::minimax_oauth_default(),
        ProviderRow::nous_default(),
        ProviderRow::anthropic_default(),
        ProviderRow::openrouter_default(),
    ];

    for row in rows {
        if !matches!(row.id(), "anthropic" | "openrouter") {
            let missing = row.credential.resolve(&credentials).unwrap_err();
            assert_eq!(missing.var, format!("ego auth login {}", row.id()));
        }

        let entry = match row.id() {
            "openai-codex" => oauth(CODEX_TOKEN),
            "github-copilot" => {
                oauth("tid=1;proxy-ep=proxy.individual.githubcopilot.com;exp=4102444800")
            }
            "anthropic" => oauth("sk-ant-oat01-subscription"),
            "openrouter" => AuthEntry::ApiKey {
                key: "sk-or-minted".into(),
            },
            _ => oauth("access-token"),
        };
        store.put(row.id(), entry).unwrap();
        let resolved = row.credential.resolve(&credentials).unwrap();
        assert!(
            resolved
                .headers()
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("authorization")),
            "{} did not derive bearer auth",
            row.id()
        );
    }
}

#[test]
fn auth_is_a_built_command_with_three_explicit_verbs() {
    let parsed = cli::parse(vec![
        "ego".to_string(),
        "auth".to_string(),
        "login".to_string(),
        "openai-codex".to_string(),
        "--device".to_string(),
    ])
    .unwrap();
    let args = match parsed {
        Invocation::Auth(args) => args,
        other => panic!("auth was not dispatched as a built command: {other:?}"),
    };
    assert_eq!(args.action, AuthAction::Login);
    assert_eq!(args.provider.as_deref(), Some("openai-codex"));
    assert!(args.device);
}

#[cfg(unix)]
#[tokio::test]
async fn a_nonzero_plugin_is_a_missing_credential_naming_the_program() {
    let dir = tempdir().unwrap();
    let plugin = dir.path().join("broken-login");
    fs::write(&plugin, "#!/bin/sh\nexit 17\n").unwrap();
    fs::set_permissions(&plugin, fs::Permissions::from_mode(0o700)).unwrap();

    let err = run_program(&plugin, "login", None).await.unwrap_err();
    let AuthError::MissingCredential { program, reason } = err else {
        panic!("wrong failure: {err:?}");
    };
    assert_eq!(program, plugin.display().to_string());
    assert!(reason.contains("17"), "{reason}");
}
