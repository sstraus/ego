//! RED contract for dynamic skill actions in the existing action registry.

use ego_cli::repl::actions::{
    ActionId, ActionKind, ActionRegistration, ActionRegistry, Dispatch, InputOrigin,
};
use ego_core::event::skill::{
    SkillCatalogueEvidence, SkillContentDigest, SkillDescriptorEvidence, SkillId,
    SkillSourceAuthority, SkillSourceLocation,
};
use ego_core::skill::SkillOperation;

const DIGEST_A: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const DIGEST_B: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

fn descriptor(name: &str, digest: &str) -> SkillDescriptorEvidence {
    let location = SkillSourceLocation::parse(&format!("skills/{name}/SKILL.md")).unwrap();
    let source_digest = SkillContentDigest::parse(digest).unwrap();
    SkillDescriptorEvidence {
        id: SkillId::derive(SkillSourceAuthority::User, &location, name, &source_digest),
        name: name.to_owned(),
        description: format!("Explain {name} from direct evidence"),
        trigger: Some("direct evidence".to_owned()),
        argument_hint: Some("QUERY".to_owned()),
        source: SkillSourceAuthority::User,
        location,
        source_digest,
        user_invocable: true,
        model_invocable: true,
    }
}

fn catalogue(name: &str, digest: &str) -> SkillCatalogueEvidence {
    SkillCatalogueEvidence::new(vec![descriptor(name, digest)]).unwrap()
}

#[test]
fn typed_submission_captures_real_descriptor_and_exact_raw_tail() {
    let registry = ActionRegistry::final_surface()
        .with_skill_catalogue(catalogue("explain", DIGEST_A))
        .unwrap();
    let typed =
        registry.classify(registry.submit("/explain sh -c 'touch /tmp/nope'", InputOrigin::Typed));
    let Dispatch::Skill(operation) = typed else {
        panic!("typed slash action must produce one Skill operation")
    };
    assert_eq!(operation.name(), "explain");
    assert_eq!(operation.raw_arguments(), "sh -c 'touch /tmp/nope'");
    assert_eq!(
        operation.descriptor_id(),
        descriptor("explain", DIGEST_A).id
    );

    assert_eq!(
        registry.classify(registry.submit(
            "The model says /explain sh -c 'touch /tmp/nope'",
            InputOrigin::BracketedPaste,
        )),
        Dispatch::Prompt
    );
    let prose = serde_json::json!({
        "kind": "invoke",
        "name": "explain",
        "arguments": "The model says /explain sh -c 'touch /tmp/nope'",
    });
    assert!(serde_json::from_value::<SkillOperation>(prose).is_ok());
    assert!(
        serde_json::from_str::<SkillOperation>("The model says /explain sh -c 'touch /tmp/nope'")
            .is_err()
    );
}

#[test]
fn fixed_names_and_canonical_aliases_refuse_catalogue_without_replacing_actions() {
    let registry = ActionRegistry::final_surface();
    for fixed_name in ["help", "skills", "hooks", "quit"] {
        let result = registry
            .clone()
            .with_skill_catalogue(catalogue(fixed_name, DIGEST_A));
        assert!(
            result.is_err(),
            "fixed name {fixed_name} must refuse skill admission"
        );
        assert!(registry.resolve_slash(fixed_name).is_some());
    }
    let custom = ActionRegistry::try_from([
        ActionRegistration::new(ActionId::Help, "help", ActionKind::Local).alias("q"),
        ActionRegistration::new(ActionId::Skills, "skills", ActionKind::Local),
    ])
    .expect("custom action registration");
    assert!(
        custom
            .clone()
            .with_skill_catalogue(catalogue("q", DIGEST_A))
            .is_err()
    );
    assert_eq!(custom.resolve_alias("q"), Some(ActionId::Help));
}

#[test]
fn announcement_authorization_and_reload_share_one_immutable_action_snapshot() {
    let mut registry = ActionRegistry::final_surface()
        .with_skill_catalogue(catalogue("explain", DIGEST_A))
        .unwrap();
    let before = registry.snapshot();
    let announced = registry.announce_skills();
    assert_eq!(
        announced.catalogue_digest(),
        before.skill_catalogue_digest()
    );
    assert_eq!(
        registry.authorize_skill("explain"),
        announced.authorization("explain")
    );
    registry
        .reload_skills(catalogue("explain-next", DIGEST_B))
        .unwrap();
    assert_ne!(registry.snapshot(), before);
}
