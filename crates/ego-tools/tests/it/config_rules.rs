//! Story 084 contract for durable user-rule authority through the OS adapter.

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

// Catches first-profile publication masking inherited user denies on restart,
// and accidental merging of an already explicit profile permissions block.
#[test]
fn first_profile_rule_preserves_inheritance_after_fresh_resolution() {
    for profile_block in [None, Some("allow = ['list(src/**)']\n"), Some("")] {
        let explicit = profile_block.is_some();
        let mut text = String::from(
            "[permissions]\njudge = 'provider/judge'\nallow = ['read(src/**)']\ndeny = ['write(private/**)']\n[profile.dev]\nmode = 'yolo'\n",
        );
        if let Some(block) = profile_block {
            text.push_str("[profile.dev.permissions]\n");
            text.push_str(block);
        }
        let (dir, io) = fixture(&text);
        let path = dir.path().join("config.toml");
        adapter(&path, &io)
            .mutate(UserConfigMutation::Rules {
                target: UserConfigTarget::Profile("dev".parse().unwrap()),
                changes: vec![RuleMutation {
                    effect: ManagedRuleEffect::Deny,
                    rule: rule("write(other/**)"),
                }],
            })
            .unwrap();
        let user =
            ConfigLayer::parse(Source::User, &std::fs::read_to_string(&path).unwrap()).unwrap();
        let system = ConfigLayer::parse(
            Source::System,
            "[permissions]\ndeny = ['write(admin/**)']\n",
        )
        .unwrap();
        let effective = ego_core::config::resolve_selected(
            &[system, user],
            ego_core::config::Trust::Trusted,
            Some(("dev", Source::Flags)),
        )
        .unwrap();
        let allows = effective
            .permissions
            .allow
            .iter()
            .map(|entry| entry.rule().to_string())
            .collect::<Vec<_>>();
        assert_eq!(allows.contains(&"read(src/**)".to_owned()), !explicit);
        assert_eq!(
            allows.contains(&"list(src/**)".to_owned()),
            profile_block.is_some_and(|block| !block.is_empty())
        );
        // Catches list-only seeding that silently drops the inherited judge.
        assert_eq!(
            effective.judge,
            (!explicit).then(|| ego_core::config::ModelRef("provider/judge".into()))
        );
        let denies = &effective.permissions.deny;
        assert!(
            denies
                .iter()
                .any(|entry| entry.rule() == &rule("write(other/**)"))
        );
        assert_eq!(
            denies
                .iter()
                .any(|entry| entry.rule() == &rule("write(private/**)")),
            !explicit
        );
        let admin = denies
            .iter()
            .find(|entry| entry.rule() == &rule("write(admin/**)"))
            .unwrap();
        assert!(
            admin
                .origins()
                .iter()
                .any(|origin| origin == Source::System)
        );
        let fresh = adapter(&path, &io)
            .rule_snapshot(UserConfigTarget::Profile("dev".parse().unwrap()))
            .unwrap();
        assert_eq!(fresh.deny.len(), if explicit { 1 } else { 2 });
    }
}

// Catches inherited opposite-effect rules being bypassed during first-block creation.
#[test]
fn first_profile_rule_conflicts_with_inherited_opposite_effect_without_publication() {
    let (dir, io) =
        fixture("[permissions]\ndeny = ['write(private/**)']\n[profile.dev]\nmode = 'yolo'\n");
    let path = dir.path().join("config.toml");
    let before = std::fs::read(&path).unwrap();
    assert!(
        adapter(&path, &io)
            .mutate(UserConfigMutation::Rules {
                target: UserConfigTarget::Profile("dev".parse().unwrap()),
                changes: vec![RuleMutation {
                    effect: ManagedRuleEffect::Allow,
                    rule: rule("write(private/**)")
                }],
            })
            .is_err()
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(io.replaces.load(Ordering::SeqCst), 0);
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
