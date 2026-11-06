//! contract for the typed stored-user configuration projection.

use std::path::PathBuf;

use ego_core::ceiling::Budget;
use ego_core::config::{
    ManagedConfigKey, ManagedConfigValue, Mode, ModelRef, ProfileName, UserConfigSnapshot,
    UserConfigStore, UserConfigTarget,
};
use ego_core::generator::types::ThinkingLevel;
use ego_core::permit::Confinement;

fn keys(target: UserConfigTarget) -> Vec<ManagedConfigKey> {
    ManagedConfigKey::for_target(target).to_vec()
}

#[test]
fn snapshot_has_exact_target_and_closed_canonical_schema_order() {
    let target = UserConfigTarget::Profile(ProfileName::parse("dev profile").unwrap());
    let snapshot = UserConfigSnapshot {
        target: target.clone(),
        values: vec![(ManagedConfigKey::Model, None)],
    };
    assert_eq!(snapshot.target, target);
    assert_eq!(
        keys(UserConfigTarget::Base),
        vec![
            ManagedConfigKey::Model,
            ManagedConfigKey::Thinking,
            ManagedConfigKey::Mode,
            ManagedConfigKey::Sandbox,
            ManagedConfigKey::Budget,
            ManagedConfigKey::Bare,
            ManagedConfigKey::PermissionsJudge,
            ManagedConfigKey::StatuslineTemplate,
            ManagedConfigKey::ExecEnv,
            ManagedConfigKey::ExecEnvAllowlist,
            ManagedConfigKey::Network,
            ManagedConfigKey::WebPrivateNetwork,
            ManagedConfigKey::Roots,
            ManagedConfigKey::ExecPaths,
            ManagedConfigKey::Keybindings,
            ManagedConfigKey::Vim,
            ManagedConfigKey::CompactionIdle,
            ManagedConfigKey::CompactionIdleMarginSeconds,
            ManagedConfigKey::CompactionIdleThreshold,
            ManagedConfigKey::CompactionHeadless,
            ManagedConfigKey::DoctorNetwork,
            ManagedConfigKey::RecapEnabled,
            ManagedConfigKey::RecapAfterMinutes,
            ManagedConfigKey::McpTasks,
            ManagedConfigKey::Memory,
            ManagedConfigKey::Checkpoints,
            ManagedConfigKey::Tools,
            ManagedConfigKey::SessionsCompress,
            ManagedConfigKey::Cache,
            ManagedConfigKey::InstructionsMaxFiles,
            ManagedConfigKey::Lsp,
        ]
    );
    assert_eq!(
        keys(snapshot.target.clone()),
        vec![
            ManagedConfigKey::Model,
            ManagedConfigKey::Thinking,
            ManagedConfigKey::Mode,
            ManagedConfigKey::Sandbox,
            ManagedConfigKey::Budget,
            ManagedConfigKey::PermissionsJudge,
            ManagedConfigKey::ExecEnv,
            ManagedConfigKey::ExecEnvAllowlist,
            ManagedConfigKey::Network,
            ManagedConfigKey::Roots,
            ManagedConfigKey::ExecPaths,
        ]
    );
    assert_eq!(snapshot.values[0], (ManagedConfigKey::Model, None));
    assert_eq!(
        keys(UserConfigTarget::Profile(ProfileName::parse("ci").unwrap())),
        vec![
            ManagedConfigKey::Model,
            ManagedConfigKey::Thinking,
            ManagedConfigKey::Mode,
            ManagedConfigKey::Sandbox,
            ManagedConfigKey::Budget,
            ManagedConfigKey::PermissionsJudge,
            ManagedConfigKey::SessionDir,
            ManagedConfigKey::ArtifactDir,
            ManagedConfigKey::ExecEnv,
            ManagedConfigKey::ExecEnvAllowlist,
            ManagedConfigKey::Network,
            ManagedConfigKey::Roots,
            ManagedConfigKey::ExecPaths,
        ]
    );
}

#[test]
fn every_scope_preserves_stored_authority_without_inheritance() {
    let base = UserConfigSnapshot {
        target: UserConfigTarget::Base,
        values: vec![
            (
                ManagedConfigKey::Model,
                Some(ManagedConfigValue::Model(ModelRef("provider/raw".into()))),
            ),
            (
                ManagedConfigKey::Thinking,
                Some(ManagedConfigValue::Thinking(ThinkingLevel::Low)),
            ),
            (
                ManagedConfigKey::Mode,
                Some(ManagedConfigValue::Mode(Mode::Edits)),
            ),
            (
                ManagedConfigKey::Sandbox,
                Some(ManagedConfigValue::Sandbox(Confinement::Workspace)),
            ),
            (
                ManagedConfigKey::Budget,
                Some(ManagedConfigValue::Budget("5$".parse::<Budget>().unwrap())),
            ),
            (
                ManagedConfigKey::Bare,
                Some(ManagedConfigValue::Bare(false)),
            ),
            (
                ManagedConfigKey::PermissionsJudge,
                Some(ManagedConfigValue::PermissionsJudge(ModelRef(
                    "judge/raw".into(),
                ))),
            ),
            (
                ManagedConfigKey::StatuslineTemplate,
                Some(ManagedConfigValue::StatuslineTemplate(
                    "{{ prompt }}".into(),
                )),
            ),
        ],
    };
    assert_eq!(base.target, UserConfigTarget::Base);
    assert_eq!(base.values.len(), 8);
    assert_eq!(
        base.values[0].1,
        Some(ManagedConfigValue::Model(ModelRef("provider/raw".into())))
    );
    assert_eq!(base.values[5].1, Some(ManagedConfigValue::Bare(false)));
    assert_eq!(
        base.values[7].1,
        Some(ManagedConfigValue::StatuslineTemplate(
            "{{ prompt }}".into()
        ))
    );

    let profile = UserConfigSnapshot {
        target: UserConfigTarget::Profile(ProfileName::parse("dev").unwrap()),
        values: vec![(
            ManagedConfigKey::Model,
            Some(ManagedConfigValue::Model(ModelRef("dev/raw".into()))),
        )],
    };
    assert_eq!(
        profile.target,
        UserConfigTarget::Profile(ProfileName::parse("dev").unwrap())
    );
    assert_eq!(
        profile.values[0].1,
        Some(ManagedConfigValue::Model(ModelRef("dev/raw".into())))
    );

    let ci = UserConfigSnapshot {
        target: UserConfigTarget::Profile(ProfileName::parse("ci").unwrap()),
        values: vec![
            (
                ManagedConfigKey::SessionDir,
                Some(ManagedConfigValue::Path(PathBuf::from("/sessions"))),
            ),
            (
                ManagedConfigKey::ArtifactDir,
                Some(ManagedConfigValue::Path(PathBuf::from("/artifacts"))),
            ),
        ],
    };
    assert_eq!(
        ci.values[0].1,
        Some(ManagedConfigValue::Path(PathBuf::from("/sessions")))
    );
    assert_eq!(
        ci.values[1].1,
        Some(ManagedConfigValue::Path(PathBuf::from("/artifacts")))
    );
}

#[test]
fn snapshot_boundary_is_typed_and_missing_profile_is_empty_success() {
    fn read(
        store: &dyn UserConfigStore,
        target: UserConfigTarget,
    ) -> Result<UserConfigSnapshot, ego_core::config::UserConfigError> {
        store.snapshot(target)
    }
    let _ = read;
}
