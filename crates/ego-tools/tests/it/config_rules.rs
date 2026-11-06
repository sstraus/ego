//! contract for durable user-rule authority through the OS adapter.

use std::io;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ego_core::config::{
    ConfigLayer, ManagedRuleEffect, RuleMutation, RuleSnapshot, Source, UserConfigMutation,
    UserConfigStore, UserConfigTarget,
};
use ego_core::permit::rule::{Rule, RuleId};
use ego_tools::config::OsUserConfigStore;
use ego_tools::store::{AtomicReplacePublication, OsIo, StoreIo};

#[derive(Default)]
struct RuleIo {
    inner: OsIo,
    replaces: AtomicUsize,
    fail_read: std::sync::atomic::AtomicBool,
}

impl StoreIo for RuleIo {
    fn append_line(&self, path: &Path, line: &[u8]) -> io::Result<()> {
        self.inner.append_line(path, line)
    }
    fn fsync(&self, path: &Path) -> io::Result<()> {
        self.inner.fsync(path)
    }
    fn exists(&self, path: &Path) -> io::Result<bool> {
        self.inner.exists(path)
    }
    fn read(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        if self.fail_read.load(Ordering::SeqCst) {
            return Err(io::Error::other("injected read failure"));
        }
        self.inner.read(path)
    }
    fn replace_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.inner.replace_atomic(path, bytes)
    }
    fn replace_atomic_publication(
        &self,
        path: &Path,
        bytes: &[u8],
    ) -> io::Result<AtomicReplacePublication> {
        self.replaces.fetch_add(1, Ordering::SeqCst);
        self.inner.replace_atomic_publication(path, bytes)
    }
    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        self.inner.truncate(path, len)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.inner.remove_file(path)
    }
    fn list_dirs(&self, path: &Path) -> io::Result<Vec<std::path::PathBuf>> {
        self.inner.list_dirs(path)
    }
}

fn rule(raw: &str) -> Rule {
    Rule::load(raw).expect("fixture rule is enforceable")
}

fn adapter(path: &Path, io: &Arc<RuleIo>) -> OsUserConfigStore<Arc<RuleIo>> {
    OsUserConfigStore::new(Arc::clone(io), path)
}

fn fixture(text: &str) -> (tempfile::TempDir, Arc<RuleIo>) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), text).unwrap();
    ConfigLayer::parse(Source::User, text).expect("fixture is strict");
    (dir, Arc::new(RuleIo::default()))
}

#[test]
fn snapshot_keeps_allow_deny_order_effect_and_permissions_presence_per_target() {
    let (dir, io) = fixture(
        "[permissions]\nallow = ['read(src/**)', 'write(src/**)']\ndeny = ['exec(cargo *)']\n[profile.dev.permissions]\nallow = ['list(src/**)']\n",
    );
    let store = adapter(&dir.path().join("config.toml"), &io);
    let base: RuleSnapshot = store.rule_snapshot(UserConfigTarget::Base).unwrap();
    assert!(base.permissions_present);
    assert_eq!(
        base.allow
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        vec!["read(src/**)", "write(src/**)"]
    );
    assert_eq!(
        base.deny
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        vec!["exec(cargo *)"]
    );
    let profile = store
        .rule_snapshot(UserConfigTarget::Profile("dev".parse().unwrap()))
        .unwrap();
    assert_eq!(
        profile
            .allow
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        vec!["list(src/**)"]
    );
    assert!(profile.deny.is_empty());
    assert!(profile.permissions_present);
}

#[test]
fn duplicate_is_unchanged_opposite_effect_conflicts_and_batch_is_atomic() {
    let (dir, io) = fixture("[permissions]\nallow = ['read(src/**)']\n");
    let path = dir.path().join("config.toml");
    let store = adapter(&path, &io);
    let target = UserConfigTarget::Base;
    let duplicate = UserConfigMutation::Rules {
        target: target.clone(),
        changes: vec![RuleMutation {
            effect: ManagedRuleEffect::Allow,
            rule: rule("read( src/** )"),
        }],
    };
    let result = store.mutate(duplicate).unwrap();
    assert!(!result.changed);
    assert_eq!(io.replaces.load(Ordering::SeqCst), 0);
    let conflict = UserConfigMutation::Rules {
        target: target.clone(),
        changes: vec![RuleMutation {
            effect: ManagedRuleEffect::Deny,
            rule: rule("read(src/**)"),
        }],
    };
    assert!(store.mutate(conflict).is_err());
    let batch = UserConfigMutation::Rules {
        target: target.clone(),
        changes: vec![
            RuleMutation {
                effect: ManagedRuleEffect::Allow,
                rule: rule("list(src/**)"),
            },
            RuleMutation {
                effect: ManagedRuleEffect::Allow,
                rule: rule("exec(cargo *)"),
            },
        ],
    };
    assert!(store.mutate(batch).is_ok());

    let before = std::fs::read(&path).unwrap();
    let intra_batch_conflict = UserConfigMutation::Rules {
        target,
        changes: vec![
            RuleMutation {
                effect: ManagedRuleEffect::Allow,
                rule: rule("search(src/**)"),
            },
            RuleMutation {
                effect: ManagedRuleEffect::Deny,
                rule: rule("search(src/**)"),
            },
        ],
    };
    assert!(store.mutate(intra_batch_conflict).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(io.replaces.load(Ordering::SeqCst), 1);
}

#[test]
fn revoke_removes_predicate_from_either_effect_and_absent_revoke_is_noop() {
    let (dir, io) = fixture(
        "# retain this comment\nmode = 'edits'\n[permissions]\nallow = ['read(src/**)', 'read(src/**)']\ndeny = ['read(src/**)']\n",
    );
    let path = dir.path().join("config.toml");
    let store = adapter(&path, &io);
    let id = rule("read(src/**)").id();
    let result = store
        .mutate(UserConfigMutation::RevokeRule {
            target: UserConfigTarget::Base,
            id,
        })
        .unwrap();
    assert!(result.changed);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("# retain this comment"));
    assert!(text.contains("mode = 'edits'"));
    let absent = RuleId::from_str("r1:000000000000").unwrap();
    let result = store
        .mutate(UserConfigMutation::RevokeRule {
            target: UserConfigTarget::Base,
            id: absent,
        })
        .unwrap();
    assert!(!result.changed);
    assert_eq!(io.replaces.load(Ordering::SeqCst), 1);

    let final_text = std::fs::read_to_string(&path).unwrap();
    assert!(!final_text.contains("permissions"));
}

#[test]
fn malformed_or_unreadable_rules_fail_closed_and_publication_cuts_remain_shared() {
    let (dir, io) = fixture("[permissions]\nallow = ['read(src/**)']\n");
    let path = dir.path().join("config.toml");
    let store = adapter(&path, &io);
    std::fs::write(&path, "[permissions]\nallow = [1]\n").unwrap();
    assert!(store.rule_snapshot(UserConfigTarget::Base).is_err());
    assert_eq!(io.replaces.load(Ordering::SeqCst), 0);

    std::fs::write(&path, "[permissions]\nallow = ['read(src/**)']\n").unwrap();
    io.fail_read.store(true, Ordering::SeqCst);
    assert!(store.rule_snapshot(UserConfigTarget::Base).is_err());
    assert!(
        store
            .mutate(UserConfigMutation::RevokeRule {
                target: UserConfigTarget::Base,
                id: rule("read(src/**)").id(),
            })
            .is_err()
    );
    assert_eq!(io.replaces.load(Ordering::SeqCst), 0);
}
