//! Human-readable roots commands; config and JSON retain their stored-value contract.

use super::*;
use ego_core::config::ManagedConfigKey;
use ego_core::permit::RootAccess;

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
        return render_json(
            target,
            [(
                ManagedConfigKey::Roots,
                roots.unwrap_or(ManagedConfigValue::Roots(Vec::new())),
            )],
            out,
        );
    }
    match roots {
        Some(ManagedConfigValue::Roots(roots)) if !roots.is_empty() => {
            out.line("ACCESS\tPATH");
            for root in roots {
                out.line(&format!(
                    "{}\t{}",
                    access(root.access),
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
        Some(_) => unreachable!("the roots key holds roots"),
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
        .map(|selected| format!(" ({})", access(selected)))
        .unwrap_or_default();
    out.line(&format!(
        "{action}: {}{permission} in {}.",
        escape_field(&path.display().to_string()),
        user_target(&result.target)
    ));
    if let UserConfigPublication::PublishedDurabilityUncertain { diagnostic } = result.publication {
        output::note(&format!(
            "Root was saved, but durability could not be confirmed: {diagnostic}"
        ));
    }
    Ok(())
}

fn access(access: RootAccess) -> &'static str {
    match access {
        RootAccess::Read => "read",
        RootAccess::ReadWrite => "read-write",
    }
}
