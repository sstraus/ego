//! Human-readable roots commands; config and JSON retain their stored-value contract.

use super::*;
use ego_core::config::ManagedConfigKey;

pub(super) fn list(
    store: &impl UserConfigStore,
    target: &UserConfigTarget,
    json: bool,
    out: &Stdout,
) -> Result<(), RunTermination> {
    let roots = snapshot(store, target.clone())?
        .values
        .into_iter()
        .find_map(|(key, value)| (key == ManagedConfigKey::Roots).then_some(value).flatten());
    if json {
        return render_json(target, [(ManagedConfigKey::Roots, roots)], out);
    }
    match roots {
        Some(ManagedConfigValue::Roots(roots)) if !roots.is_empty() => {
            out.line("ACCESS\tPATH");
            for root in roots {
                out.line(&format!(
                    "{}\t{}",
                    root.access.as_str(),
                    escape_field(&root.path.display().to_string())
                ));
            }
        }
        Some(ManagedConfigValue::Roots(_)) => {
            out.line("No roots are stored; this target selects workspace-only access.")
        }
        None => {
            out.line("No roots are stored; platform defaults or other configuration layers apply.")
        }
        Some(_) => {
            return Err(RunTermination::Failed {
                message: "roots listing received an unrelated stored value".into(),
            });
        }
    }
    Ok(())
}

pub(super) fn receipt(
    result: UserConfigMutationResult,
    out: &Stdout,
) -> Result<(), RunTermination> {
    let UserConfigMutationFacts::Root {
        path,
        access: selected,
    } = result.facts
    else {
        return Err(RunTermination::Failed {
            message: "root mutation returned unrelated transaction facts".into(),
        });
    };
    let action = if !result.changed {
        "Root unchanged"
    } else if selected.is_some() {
        "Saved root"
    } else {
        "Removed root"
    };
    let permission = selected
        .map(|selected| format!(" ({})", selected.as_str()))
        .unwrap_or_default();
    out.line(&format!(
        "{action}: {}{permission} in {}.",
        escape_field(&path.display().to_string()),
        user_target(&result.target)
    ));
    if let UserConfigPublication::PublishedDurabilityUncertain { diagnostic } = result.publication {
        output::note(&format!(
            "Root settings changed, but saving them to disk could not be confirmed: {diagnostic}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct WrongTypedStore;

    impl UserConfigStore for WrongTypedStore {
        fn snapshot(
            &self,
            target: UserConfigTarget,
        ) -> Result<UserConfigSnapshot, ego_core::config::UserConfigError> {
            Ok(UserConfigSnapshot {
                target,
                values: vec![(
                    ManagedConfigKey::Roots,
                    Some(ManagedConfigValue::parse(ManagedConfigKey::Bare, "true").unwrap()),
                )],
            })
        }

        fn rule_snapshot(
            &self,
            _: UserConfigTarget,
        ) -> Result<ego_core::config::RuleSnapshot, ego_core::config::UserConfigError> {
            Err(ego_core::config::UserConfigError::new("not used"))
        }

        fn mutate(
            &self,
            _: UserConfigMutation,
        ) -> Result<UserConfigMutationResult, ego_core::config::UserConfigError> {
            Err(ego_core::config::UserConfigError::new("not used"))
        }
    }

    // Catches: an inconsistent port snapshot panics instead of reporting failure.
    #[test]
    fn wrong_typed_roots_snapshot_reports_failure_without_panicking() {
        let out = Stdout::new(CancelToken::new());
        let result = list(&WrongTypedStore, &UserConfigTarget::Base, false, &out);
        assert!(
            matches!(result, Err(RunTermination::Failed { message }) if message.contains("roots"))
        );
    }
}
