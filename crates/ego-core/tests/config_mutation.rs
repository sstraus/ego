//! regression case contract for the core-owned managed mutation vocabulary.

use ego_core::config::{
    ManagedConfigKey, ManagedConfigValue, ProfileName, UserConfigMutation, UserConfigTarget,
};

#[test]
fn managed_set_and_unset_carry_one_typed_target_and_key() {
    let target = UserConfigTarget::Profile(ProfileName::parse("ci").unwrap());
    let key = ManagedConfigKey::Model;
    let set = UserConfigMutation::ManagedSet {
        target: target.clone(),
        key: key.clone(),
        value: ManagedConfigValue::parse(key.clone(), "'provider/model'").unwrap(),
    };
    let unset = UserConfigMutation::ManagedUnset { target, key };
    let UserConfigMutation::ManagedSet {
        target: set_target,
        key: set_key,
        value: set_value,
    } = set
    else {
        panic!("expected managed set");
    };
    assert_eq!(
        set_target,
        UserConfigTarget::Profile(ProfileName::parse("ci").unwrap())
    );
    assert_eq!(set_key, ManagedConfigKey::Model);
    assert_eq!(
        set_value,
        ManagedConfigValue::parse(ManagedConfigKey::Model, "'provider/model'").unwrap()
    );
    let UserConfigMutation::ManagedUnset {
        target: unset_target,
        key: unset_key,
    } = unset
    else {
        panic!("expected managed unset");
    };
    assert_eq!(
        unset_target,
        UserConfigTarget::Profile(ProfileName::parse("ci").unwrap())
    );
    assert_eq!(unset_key, ManagedConfigKey::Model);
}

#[test]
fn managed_mutation_facts_and_result_preserve_changed_and_publication_state() {
    let result = ego_core::config::UserConfigMutationResult {
        target: UserConfigTarget::Base,
        facts: ego_core::config::UserConfigMutationFacts::Set {
            key: ManagedConfigKey::Bare,
            value: ManagedConfigValue::parse(ManagedConfigKey::Bare, "true").unwrap(),
        },
        changed: true,
        publication: ego_core::config::UserConfigPublication::PublishedDurably,
    };
    assert_eq!(result.target, UserConfigTarget::Base);
    assert_eq!(
        result.facts,
        ego_core::config::UserConfigMutationFacts::Set {
            key: ManagedConfigKey::Bare,
            value: ManagedConfigValue::parse(ManagedConfigKey::Bare, "true").unwrap(),
        }
    );
    assert!(result.changed);
    assert_eq!(
        result.publication,
        ego_core::config::UserConfigPublication::PublishedDurably
    );
    let no_op = ego_core::config::UserConfigMutationResult {
        target: UserConfigTarget::Base,
        facts: ego_core::config::UserConfigMutationFacts::Unset {
            key: ManagedConfigKey::Bare,
        },
        changed: false,
        publication: ego_core::config::UserConfigPublication::NotPublished,
    };
    assert!(!no_op.changed);
    assert_eq!(
        no_op.publication,
        ego_core::config::UserConfigPublication::NotPublished
    );
}
