//! Public atomic managed-config batch contracts.
use std::sync::Arc;

use ego_core::config::{
    ManagedConfigKey, ManagedConfigValue, UserConfigMutation, UserConfigStore, UserConfigTarget,
};
use ego_tools::config::OsUserConfigStore;
use ego_tools::store::OsIo;

fn set(key: ManagedConfigKey, raw: &str) -> UserConfigMutation {
    UserConfigMutation::ManagedSet {
        target: UserConfigTarget::Base,
        value: ManagedConfigValue::parse(key.clone(), raw).unwrap(),
        key,
    }
}

// Catches: a batch publishes only its last mutation or erases unrelated operator configuration.
#[test]
fn managed_batch_publishes_all_changes_and_preserves_unrelated_configuration() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("config.toml");
    std::fs::write(
        &path,
        "# operator note\nmodel = 'old/model'\n[profile.ci]\nmodel = 'ci/model'\n",
    )
    .unwrap();
    let store = OsUserConfigStore::new(Arc::new(OsIo), &path);
    let changes = vec![
        set(ManagedConfigKey::Model, "'new/model'"),
        set(ManagedConfigKey::Thinking, "'high'"),
    ];
    assert!(store.managed_batch(changes.clone()).unwrap().changed);
    let text = std::fs::read_to_string(&path).unwrap();
    let parsed: toml::Value = toml::from_str(&text).unwrap();
    assert_eq!(parsed["model"].as_str(), Some("new/model"));
    assert_eq!(parsed["thinking"].as_str(), Some("high"));
    assert_eq!(parsed["profile"]["ci"]["model"].as_str(), Some("ci/model"));
    assert!(text.contains("# operator note"));
    assert!(!store.managed_batch(changes).unwrap().changed);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
}

// Catches: conditional batch ignores a stale revision and overwrites a concurrent operator edit.
#[test]
fn conditional_managed_batch_refuses_stale_revision_without_overwriting_bytes() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("config.toml");
    std::fs::write(&path, "model = 'old/model'\n").unwrap();
    let store = OsUserConfigStore::new(Arc::new(OsIo), &path);
    let (_, revision) = store.revision_snapshot(UserConfigTarget::Base).unwrap();
    assert!(
        store
            .managed_batch_conditional(
                vec![set(ManagedConfigKey::Model, "'first/model'")],
                &revision
            )
            .unwrap()
            .changed
    );
    let committed = std::fs::read(&path).unwrap();
    assert!(
        store
            .managed_batch_conditional(
                vec![set(ManagedConfigKey::Model, "'stale/model'")],
                &revision
            )
            .is_err()
    );
    assert_eq!(std::fs::read(&path).unwrap(), committed);
}

// Catches: empty or cross-target batches create files despite invalid atomic transaction scope.
#[test]
fn invalid_managed_batch_has_no_filesystem_effect() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("absent/config.toml");
    let store = OsUserConfigStore::new(Arc::new(OsIo), &path);
    assert!(store.managed_batch(Vec::new()).is_err());
    assert!(
        store
            .managed_batch(vec![UserConfigMutation::Thinking {
                thinking: ego_core::generator::types::ThinkingLevel::High
            }])
            .is_err()
    );
    assert!(!path.parent().unwrap().exists());
}
