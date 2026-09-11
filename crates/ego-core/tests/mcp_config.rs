//! contract for regression case typed MCP configuration and credentials.

use std::collections::HashMap;
use std::sync::Mutex;

use ego_core::config::{
    ConfigLayer, McpConfigProvenance, McpLayerEntry, McpServerConfig, McpServerName, ProfileName,
    ResolvedMcpEntry, Source, Sourced, Trust, UserConfigMutation, UserConfigTarget, resolve,
};
use ego_core::credentials::{
    CredentialKey, CredentialRemoval, CredentialStore, CredentialStoreError,
    CredentialStoreErrorKind, CredentialStoreOperation, CredentialTarget, SecretMaterial,
};

fn layer(source: Source, body: &str) -> Sourced<ConfigLayer> {
    ConfigLayer::parse(source, body).expect("strict MCP fixture")
}

#[test]
fn strict_decode_asserts_stdio_http_defaults_oauth_and_tombstone() {
    let config = layer(
        Source::User,
        r#"
[mcp.servers.local]
transport = "stdio"
command = "mcp-local"
args = ["--safe"]
cwd = "/workspace"
inherit_env = ["TOKEN", "HOME"]

[mcp.servers.relative]
transport = "stdio"
command = "mcp-relative"
cwd = "."

[mcp.servers.remote]
transport = "http"
url = "https://mcp.example.test/v1"
auth = "oauth"

[mcp.servers.plain]
transport = "http"
url = "http://localhost:8787"
allow_insecure_loopback = true

[mcp.servers.ipv4]
transport = "http"
url = "http://127.0.0.1:8787"
allow_insecure_loopback = true

[mcp.servers.ipv6]
transport = "http"
url = "http://[::1]:8787"
allow_insecure_loopback = true

[mcp.servers.default-auth]
transport = "http"
url = "https://mcp.example.test/default"

[mcp.servers.hidden]
removed = true
"#,
    );
    let local = config
        .value
        .mcp
        .servers
        .get(&"local".parse().expect("name"));
    assert!(
        matches!(local, Some(McpLayerEntry::Server(McpServerConfig::Stdio(s))) if s.command == "mcp-local" && s.args == ["--safe"] && s.cwd.as_deref().map(|p| p.to_string_lossy().into_owned()) == Some("/workspace".into()) && s.inherit_env.iter().map(ToString::to_string).collect::<Vec<_>>() == ["TOKEN", "HOME"])
    );
    assert!(matches!(
        config.value.mcp.servers.get(&"relative".parse().expect("name")),
        Some(McpLayerEntry::Server(McpServerConfig::Stdio(s)))
            if s.cwd.as_deref() == Some(std::path::Path::new("."))
    ));
    let remote = config
        .value
        .mcp
        .servers
        .get(&"remote".parse().expect("name"));
    assert!(
        matches!(remote, Some(McpLayerEntry::Server(McpServerConfig::Http(h))) if h.url.to_string() == "https://mcp.example.test/v1" && h.auth == ego_core::config::McpHttpAuth::Oauth && !h.allow_insecure_loopback)
    );
    let plain = config
        .value
        .mcp
        .servers
        .get(&"plain".parse().expect("name"));
    assert!(
        matches!(plain, Some(McpLayerEntry::Server(McpServerConfig::Http(h))) if h.allow_insecure_loopback)
    );
    assert!(matches!(
        config.value.mcp.servers.get(&"ipv4".parse().expect("name")),
        Some(McpLayerEntry::Server(McpServerConfig::Http(h))) if h.allow_insecure_loopback
    ));
    assert!(matches!(
        config.value.mcp.servers.get(&"ipv6".parse().expect("name")),
        Some(McpLayerEntry::Server(McpServerConfig::Http(h))) if h.allow_insecure_loopback
    ));
    assert!(matches!(
        config.value.mcp.servers.get(&"default-auth".parse().expect("name")),
        Some(McpLayerEntry::Server(McpServerConfig::Http(h))) if h.auth == ego_core::config::McpHttpAuth::None && !h.allow_insecure_loopback
    ));
    assert!(matches!(
        config
            .value
            .mcp
            .servers
            .get(&"hidden".parse().expect("name")),
        Some(McpLayerEntry::Tombstone)
    ));
}

#[test]
fn strict_decode_rejects_names_env_duplicates_legacy_mixed_and_url_matrix() {
    let invalid = [
        "[mcp.servers.Bad_Name]\ntransport='stdio'\ncommand='x'",
        "[mcp.servers.x]\ntransport='stdio'\ncommand='x'\ninherit_env=['TOKEN','token']",
        "[mcp.servers.x]\ntransport='stdio'\ncommand='x'\ninherit_env=['GOOD','bad-name']",
        "[mcp.servers.x]\ntransport='stdio'\ncommand=''",
        "[mcp.servers.x]\ntransport='stdio'\ncommand='x'\nurl='https://x'",
        "[mcp.servers.x]\ntransport='http'\nurl='https://x'\nallow_insecure_loopback=true",
        "[mcp.servers.x]\ntransport='http'\nurl='http://localhost'\nallow_insecure_loopback=false",
        "[mcp.servers.x]\ntransport='http'\nurl='http://example.test'\nallow_insecure_loopback=true",
        "[mcp.servers.x]\ntransport='http'\nurl='http://127.0.0.2'\nallow_insecure_loopback=false",
        "[mcp.servers.x]\ntransport='ftp'\nurl='ftp://x'",
        "[mcp.servers.x]\ntransport='http'\nurl='https://u:p@x'",
        "[mcp.servers.x]\ntransport='http'\nurl='https://x/#fragment'",
        "[mcp.servers.x]\nsse='https://old'",
        "[mcp.servers.x]\nremoved=false",
        "[mcp.servers.x]\nremoved=true\ntransport='stdio'\ncommand='x'",
        "[mcp.servers.x]\ntransport='http'\nurl='http://localhost.'\nallow_insecure_loopback=true",
        "[mcp.servers.x]\ntransport='http'\nurl='http://192.168.1.1'\nallow_insecure_loopback=true",
        "[mcp.servers.x]\ntransport='http'\nurl='http://localhost'\nallow_insecure_loopback=true\nunknown='field'",
    ];
    for text in invalid {
        assert!(
            ConfigLayer::parse(Source::User, text).is_err(),
            "accepted invalid fixture: {text}"
        );
    }
}

#[test]
fn resolution_is_whole_entry_profile_trust_and_ambient_aware() {
    let base = layer(
        Source::User,
        "[mcp.servers.x]\ntransport='stdio'\ncommand='base'",
    );
    let profile_layer = layer(
        Source::User,
        "[profile.dev.mcp.servers.x]\ntransport='http'\nurl='https://profile'",
    );
    let system = layer(
        Source::System,
        "[mcp.servers.x]\ntransport='stdio'\ncommand='system'",
    );
    let effective = resolve(
        &[base, profile_layer, system.clone()],
        Trust::Trusted,
        Some("dev"),
    )
    .expect("resolution");
    let name = "x".parse().expect("name");
    assert!(
        matches!(effective.mcp.entry(&name), Some(ResolvedMcpEntry::Configured { server: McpServerConfig::Http(h), provenance: McpConfigProvenance::Profile { source: Source::User, name: profile_name } }) if h.url.to_string() == "https://profile" && profile_name == "dev")
    );
    assert!(effective.mcp.configured(&name).is_some());
    let tombstone = layer(Source::User, "[mcp.servers.x]\nremoved=true");
    let hidden = resolve(&[tombstone, system], Trust::Trusted, None).expect("tombstone resolution");
    assert!(matches!(
        hidden.mcp.entry(&name),
        Some(ResolvedMcpEntry::Tombstone {
            provenance: McpConfigProvenance::Layer {
                source: Source::User
            }
        })
    ));
    assert!(hidden.mcp.configured(&name).is_none());
    let project = layer(
        Source::Project,
        "[mcp.servers.x]\ntransport='stdio'\ncommand='project'",
    );
    let ambient = resolve(&[project], Trust::Untrusted, None).expect("untrusted resolution");
    assert!(ambient.mcp.entry(&name).is_none());
}

#[test]
fn user_mutations_encode_exact_slot_expected_entry_and_reveal_semantics() {
    let name: McpServerName = "remote".parse().expect("name");
    let server = McpServerConfig::Stdio(ego_core::config::McpStdioConfig {
        command: "mcp".into(),
        args: vec![],
        cwd: None,
        inherit_env: vec![],
    });
    let set = UserConfigMutation::McpSet {
        target: UserConfigTarget::Profile(ProfileName::parse("dev").unwrap()),
        name: name.clone(),
        expected_user_entry: Some(McpLayerEntry::Tombstone),
        server: server.clone(),
    };
    assert!(
        matches!(set, UserConfigMutation::McpSet { target: UserConfigTarget::Profile(profile), name: n, expected_user_entry: Some(McpLayerEntry::Tombstone), server: McpServerConfig::Stdio(_), .. } if profile.as_str() == "dev" && n == name)
    );
    let delete = UserConfigMutation::McpDeleteUserEntry {
        target: UserConfigTarget::Base,
        name: name.clone(),
        expected_entry: McpLayerEntry::Server(server),
    };
    assert!(
        matches!(delete, UserConfigMutation::McpDeleteUserEntry { target: UserConfigTarget::Base, expected_entry: McpLayerEntry::Server(McpServerConfig::Stdio(s)), .. } if s.command == "mcp")
    );
    let insert = UserConfigMutation::McpInsertUserTombstone {
        name: name.clone(),
        expected_user_entry: None,
    };
    assert!(
        matches!(insert, UserConfigMutation::McpInsertUserTombstone { expected_user_entry: None, name: n } if n == name)
    );
}

#[derive(Default)]
struct FakeStore(Mutex<HashMap<CredentialKey, Vec<u8>>>);

impl CredentialStore for FakeStore {
    fn get(&self, key: &CredentialKey) -> Result<Option<SecretMaterial>, CredentialStoreError> {
        Ok(self
            .0
            .lock()
            .expect("fake lock")
            .get(key)
            .cloned()
            .map(SecretMaterial::new))
    }
    fn put(&self, key: &CredentialKey, value: SecretMaterial) -> Result<(), CredentialStoreError> {
        self.0
            .lock()
            .expect("fake lock")
            .insert(key.clone(), value.expose_secret_bytes().to_vec());
        Ok(())
    }
    fn delete(&self, key: &CredentialKey) -> Result<CredentialRemoval, CredentialStoreError> {
        Ok(if self.0.lock().expect("fake lock").remove(key).is_some() {
            CredentialRemoval::Removed
        } else {
            CredentialRemoval::Absent
        })
    }
}

#[test]
fn credential_fake_proves_resource_audience_account_identity_and_redaction() {
    let resource = CredentialKey::new(
        "https://issuer".into(),
        CredentialTarget::ProtectedResource("https://same".into()),
        Some("account-a".into()),
    )
    .expect("key");
    let audience = CredentialKey::new(
        "https://issuer".into(),
        CredentialTarget::Audience("https://same".into()),
        Some("account-a".into()),
    )
    .expect("key");
    let account = CredentialKey::new(
        "https://issuer".into(),
        CredentialTarget::ProtectedResource("https://same".into()),
        Some("account-b".into()),
    )
    .expect("key");
    let store = FakeStore::default();
    store
        .put(&resource, SecretMaterial::new(b"secret-a".to_vec()))
        .expect("put");
    assert_eq!(
        store
            .get(&resource)
            .expect("get")
            .expect("present")
            .expose_secret_bytes(),
        b"secret-a"
    );
    assert!(store.get(&audience).expect("domain isolation").is_none());
    assert!(store.get(&account).expect("account isolation").is_none());
    assert_ne!(resource.canonical_bytes(), audience.canonical_bytes());
    assert_ne!(resource.canonical_bytes(), account.canonical_bytes());
    assert_eq!(
        store.delete(&resource).expect("delete"),
        CredentialRemoval::Removed
    );
    assert_eq!(
        store.delete(&resource).expect("delete absent"),
        CredentialRemoval::Absent
    );
    let debug = format!("{:?}", SecretMaterial::new(b"secret-a".to_vec()));
    assert!(!debug.contains("secret-a"));
}

#[test]
fn credential_store_errors_have_only_fixed_closed_diagnostics() {
    for kind in [
        CredentialStoreErrorKind::Unavailable,
        CredentialStoreErrorKind::Invalid,
        CredentialStoreErrorKind::Ambiguous,
        CredentialStoreErrorKind::Corrupt,
        CredentialStoreErrorKind::Unsupported,
        CredentialStoreErrorKind::UnsupportedVersion,
        CredentialStoreErrorKind::Collision,
    ] {
        let error = CredentialStoreError::new(CredentialStoreOperation::Get, kind);
        let repeated = CredentialStoreError::new(CredentialStoreOperation::Get, kind);
        assert!(!error.diagnostic().is_empty());
        assert_eq!(error.diagnostic(), repeated.diagnostic());
        assert!(!format!("{error:?} {error}").contains("opaque"));
    }
}
