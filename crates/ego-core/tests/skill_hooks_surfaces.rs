//! RED contracts for the S8 user operation and immutable hook catalogue seams.

use ego_core::event::skill::{
    SkillContentDigest, SkillDescriptorEvidence, SkillId, SkillSourceAuthority, SkillSourceLocation,
};
use ego_core::skill::{AdmittedSkillOperation, SkillCatalogue};

const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn descriptor() -> SkillDescriptorEvidence {
    let location = SkillSourceLocation::parse("skills/demo/SKILL.md").unwrap();
    let digest = SkillContentDigest::parse(DIGEST).unwrap();
    SkillDescriptorEvidence {
        id: SkillId::derive(SkillSourceAuthority::User, &location, "demo", &digest),
        name: "demo".into(),
        description: "demo".into(),
        trigger: None,
        argument_hint: None,
        source: SkillSourceAuthority::User,
        location,
        source_digest: digest,
        user_invocable: true,
        model_invocable: false,
    }
}

#[test]
fn direct_user_operation_requires_name_and_descriptor_authority_and_stale_ids_refuse() {
    let d = descriptor();
    let catalogue = SkillCatalogue::new(vec![
        ego_core::skill::SkillSpec::new(d.clone(), b"body".to_vec()).unwrap(),
    ])
    .unwrap();
    let operation = AdmittedSkillOperation::invoke(&d, "QUERY");
    assert!(catalogue.authorize_user_operation(&operation).is_ok());
    let mut stale_descriptor = descriptor();
    stale_descriptor.source_digest = SkillContentDigest::parse(
        "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
    )
    .unwrap();
    stale_descriptor.id = SkillId::derive(
        SkillSourceAuthority::User,
        &stale_descriptor.location,
        "demo",
        &stale_descriptor.source_digest,
    );
    let stale = AdmittedSkillOperation::invoke(&stale_descriptor, "QUERY");
    assert!(catalogue.authorize_user_operation(&stale).is_err());
}

#[test]
fn hook_runtime_snapshot_dispatch_and_whole_replacement_share_one_generation() {
    let empty =
        ego_core::hook::HookCatalogue::admit_typed(&[], &[], ego_core::config::Trust::Trusted)
            .unwrap();
    let dispatcher =
        ego_core::hook::HookLifecycleDispatcher::new(ego_core::hook::HookRunner::new(), empty);
    let before = dispatcher.catalogue_snapshot().unwrap();
    let layer = ego_core::config::ConfigLayer::parse(ego_core::config::Source::User, "[[hooks]]\nevent = 'PreToolUse'\nmatcher = 'read'\n[hooks.handler]\ntype = 'prompt'\nprompt = 'ok'\n").unwrap();
    let replacement =
        ego_core::hook::HookCatalogue::admit_typed(&[layer], &[], ego_core::config::Trust::Trusted)
            .unwrap();
    dispatcher.replace_catalogue(replacement).unwrap();
    let after = dispatcher.catalogue_snapshot().unwrap();
    assert!(before.specs().is_empty());
    assert_eq!(after.specs().len(), 1);
    assert_eq!(
        serde_json::to_value(after.specs()[0].matcher()).unwrap(),
        serde_json::json!("read")
    );
    assert_ne!(
        std::sync::Arc::as_ptr(&before),
        std::sync::Arc::as_ptr(&after)
    );
}
