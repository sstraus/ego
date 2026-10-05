//! Text projection of captured configuration; this module never resolves or writes policy.

use ego_core::config::{ConfigLayer, Effective, ManagedConfigValue, Posture, Source, Sourced};

/// Show the startup values and every eligible declaration that contributed them.
pub(crate) fn repl_text(
    effective: &Effective,
    layers: &[Sourced<ConfigLayer>],
    perimeter: &crate::perimeter::Perimeter,
) -> String {
    let mut lines =
        vec!["Configuration (startup snapshot; saved edits apply to future sessions)".into()];
    let profile = effective
        .active_profile
        .as_ref()
        .map(|profile| profile.name.as_str());
    let mut ordered = layers
        .iter()
        .filter(|layer| {
            !effective
                .discarded
                .iter()
                .any(|discarded| discarded.source == layer.source)
                && !(effective.posture == Posture::Bare
                    && matches!(layer.source, Source::Project | Source::Local))
        })
        .collect::<Vec<_>>();
    ordered.sort_by_key(|layer| {
        Source::ALL
            .iter()
            .position(|source| *source == layer.source)
    });
    for (section, key) in [
        ("Model", "model"),
        ("Model", "thinking"),
        ("Mode", "mode"),
        ("Sandbox", "sandbox"),
        ("Perimeter", "roots"),
        ("Perimeter", "network"),
        ("Perimeter", "exec.env"),
        ("Perimeter", "exec.env_allowlist"),
        ("Budget", "budget"),
    ] {
        if matches!(key, "model" | "mode" | "sandbox" | "roots" | "budget") {
            lines.push(section.into());
        }
        let declarations = declarations(key, profile, &ordered);
        if declarations.is_empty() {
            let value = match key {
                "sandbox" => effective.sandbox.unwrap_or_default().to_string(),
                "network" => ManagedConfigValue::Network(effective.network).to_toml(),
                "exec.env" => {
                    ManagedConfigValue::ExecEnv(effective.exec.env.unwrap_or_default()).to_toml()
                }
                "exec.env_allowlist" => "[]".into(),
                "roots" => "see admitted perimeter below".into(),
                _ => "<unset>".into(),
            };
            lines.push(format!("  {key}\t{value}\tdefault"));
        }
        for (index, (value, origin)) in declarations.iter().enumerate() {
            // Money and token ceilings are independent: a token declaration cannot override money.
            let winning =
                declarations
                    .iter()
                    .skip(index + 1)
                    .rev()
                    .find(|(later, later_origin)| match (value, later) {
                        _ if later_origin.starts_with(&format!("{origin} profile ")) => true,
                        (
                            ManagedConfigValue::Budget(ego_core::ceiling::Budget::Money(_)),
                            ManagedConfigValue::Budget(ego_core::ceiling::Budget::Tokens(_)),
                        )
                        | (
                            ManagedConfigValue::Budget(ego_core::ceiling::Budget::Tokens(_)),
                            ManagedConfigValue::Budget(ego_core::ceiling::Budget::Money(_)),
                        ) => false,
                        _ => true,
                    });
            let warning = winning.map_or_else(String::new, |(_, winner)| {
                format!("; overridden by {winner}")
            });
            lines.push(format!(
                "  {key}\t{}\t{origin}{warning}",
                super::escape_field(&value.to_toml())
            ));
        }
    }
    lines.push(format!(
        "  effective mode\t{}",
        effective
            .mode
            .map_or_else(|| "default".into(), |mode| mode.to_string())
    ));
    lines.push(format!(
        "  effective ceilings\tmoney={} tokens={}",
        effective.ceilings.money.map_or_else(
            || "none".into(),
            |money| format!("{}{}", money.amount, money.currency)
        ),
        effective
            .ceilings
            .tokens
            .map_or_else(|| "none".into(), |tokens| tokens.to_string())
    ));
    if effective.clamped.any() {
        lines.push("  warning: budget overridden by system ceiling".into());
    }
    if let Some(maximum) = effective.max_mode {
        lines.push(format!(
            "  system max_mode\t{maximum}\tsystem (binding ceiling)"
        ));
    }
    lines.push(perimeter.text());
    lines.extend(effective.discarded.iter().map(ToString::to_string));
    lines.push("Save defaults: /config set KEY TOML_VALUE (user base layer); live controls: /model /thinking /mode".into());
    lines.join("\n")
}

fn declarations(
    key: &str,
    profile: Option<&str>,
    ordered: &[&Sourced<ConfigLayer>],
) -> Vec<(ManagedConfigValue, String)> {
    let mut declarations = Vec::new();
    for layer in ordered {
        let base = &layer.value;
        let selected = profile.and_then(|name| base.profile.get(name));
        let base_value = match key {
            "model" => base.model.clone().map(ManagedConfigValue::Model),
            "thinking" => base.thinking.map(ManagedConfigValue::Thinking),
            "mode" => base.mode.map(ManagedConfigValue::Mode),
            "sandbox" => base.sandbox.map(ManagedConfigValue::Sandbox),
            "roots" => base.roots.clone().map(ManagedConfigValue::Roots),
            "network" => base.network.map(ManagedConfigValue::Network),
            "exec.env" => base
                .exec
                .as_ref()
                .and_then(|exec| exec.env)
                .map(ManagedConfigValue::ExecEnv),
            "exec.env_allowlist" => base
                .exec
                .as_ref()
                .and_then(|exec| exec.env_allowlist.clone())
                .map(ManagedConfigValue::ExecEnvAllowlist),
            "budget" => base.budget.clone().map(ManagedConfigValue::Budget),
            _ => unreachable!("fixed config keys"),
        };
        let profile_value = selected.and_then(|selected| match key {
            "model" => selected.model.clone().map(ManagedConfigValue::Model),
            "thinking" => selected.thinking.map(ManagedConfigValue::Thinking),
            "mode" => selected.mode.map(ManagedConfigValue::Mode),
            "sandbox" => selected.sandbox.map(ManagedConfigValue::Sandbox),
            "roots" => selected.roots.clone().map(ManagedConfigValue::Roots),
            "network" => selected.network.map(ManagedConfigValue::Network),
            "exec.env" => selected
                .exec
                .as_ref()
                .and_then(|exec| exec.env)
                .map(ManagedConfigValue::ExecEnv),
            "exec.env_allowlist" => selected
                .exec
                .as_ref()
                .and_then(|exec| exec.env_allowlist.clone())
                .map(ManagedConfigValue::ExecEnvAllowlist),
            "budget" => selected.budget.clone().map(ManagedConfigValue::Budget),
            _ => unreachable!("fixed config keys"),
        });
        if let Some(value) = base_value {
            declarations.push((value, layer.source.to_string()));
        }
        if let Some(value) = profile_value {
            declarations.push((
                value,
                format!(
                    "{} profile {}",
                    layer.source,
                    profile.expect("selected profile")
                ),
            ));
        }
    }
    declarations
}

/// Name captured authority that defeats a newly saved base-user default.
pub(crate) fn saved_override(
    args: &crate::cli::ConfigArgs,
    effective: &Effective,
    layers: &[Sourced<ConfigLayer>],
) -> Option<String> {
    let crate::cli::ConfigAction::Set { key, value, .. } = &args.action else {
        return None;
    };
    if !matches!(
        key.as_str(),
        "model"
            | "thinking"
            | "mode"
            | "sandbox"
            | "roots"
            | "network"
            | "exec.env"
            | "exec.env_allowlist"
            | "budget"
    ) {
        return None;
    }
    let mut ordered = layers
        .iter()
        .filter(|layer| {
            Source::ALL
                .iter()
                .position(|source| *source == layer.source)
                >= Some(1)
                && !(effective.posture == Posture::Bare
                    && matches!(layer.source, Source::Project | Source::Local))
        })
        .collect::<Vec<_>>();
    ordered.sort_by_key(|layer| {
        Source::ALL
            .iter()
            .position(|source| *source == layer.source)
    });
    let profile = effective
        .active_profile
        .as_ref()
        .map(|profile| profile.name.as_str());
    let declarations = declarations(key.as_str(), profile, &ordered);
    declarations
        .into_iter()
        .rev()
        .find(|(later, origin)| {
            origin != "user"
                && !matches!(
                    (value, later),
                    (
                        ManagedConfigValue::Budget(ego_core::ceiling::Budget::Money(_)),
                        ManagedConfigValue::Budget(ego_core::ceiling::Budget::Tokens(_)),
                    ) | (
                        ManagedConfigValue::Budget(ego_core::ceiling::Budget::Tokens(_)),
                        ManagedConfigValue::Budget(ego_core::ceiling::Budget::Money(_)),
                    )
                )
        })
        .map(|(_, origin)| {
            format!(
                "Saved user {} is overridden by {origin} in this session's startup configuration",
                key.as_str()
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ego_core::config::{Trust, resolve, resolve_selected};
    use ego_core::permit::{CanonPath, RunConfinement};

    // Catches: a flag/profile override looks like an applied user default, or independent budget axes erase each other.
    #[test]
    fn config_repl_names_overrides_and_keeps_independent_ceiling_axes() {
        let layers = vec![
            ConfigLayer::parse(Source::User, "model='ollama/user'\nmode='plan'\nbudget='2USD'\n[profile.review]\nmodel='ollama/profile'").unwrap(),
            ConfigLayer::parse(Source::Flags, "model='ollama/flag'\nmode='default'\nbudget='20k'").unwrap(),
        ];
        let effective =
            resolve_selected(&layers, Trust::Untrusted, Some(("review", Source::Flags))).unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let confinement = RunConfinement::new(
            effective.sandbox.unwrap_or_default(),
            false,
            CanonPath::new(scratch.path().canonicalize().unwrap()),
        )
        .unwrap();
        let perimeter = crate::perimeter::Perimeter::session(&confinement, &effective, &[]);
        let text = repl_text(&effective, &layers, &perimeter);
        for section in [
            "\nModel\n",
            "\nMode\n",
            "\nSandbox\n",
            "\nPerimeter\n",
            "\nBudget\n",
        ] {
            assert!(text.contains(section), "{text}");
        }
        assert!(text.contains("user; overridden by flags"), "{text}");
        assert!(
            text.contains("user profile review; overridden by flags"),
            "{text}"
        );
        let set = super::super::repl_set_args("set model 'ollama/saved'").unwrap();
        assert!(
            saved_override(&set, &effective, &layers)
                .unwrap()
                .contains("overridden by flags")
        );
        let budget = text
            .lines()
            .filter(|line| line.starts_with("  budget\t"))
            .collect::<Vec<_>>();
        assert_eq!(budget.len(), 2);
        assert!(
            budget.iter().all(|line| !line.contains("overridden")),
            "{text}"
        );
    }

    // Catches: trusted-looking project rows leak into bare inspection, or a system clamp goes unreported.
    #[test]
    fn config_repl_reports_system_clamp_and_discards_project_authority() {
        let layers = vec![
            ConfigLayer::parse(Source::System, "budget='5k'").unwrap(),
            ConfigLayer::parse(Source::User, "bare=true\nbudget='20k'").unwrap(),
            ConfigLayer::parse(Source::Project, "model='ollama/ignored'").unwrap(),
        ];
        let effective = resolve(&layers, Trust::Untrusted, None).unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let confinement = RunConfinement::new(
            effective.sandbox.unwrap_or_default(),
            false,
            CanonPath::new(scratch.path().canonicalize().unwrap()),
        )
        .unwrap();
        let perimeter = crate::perimeter::Perimeter::session(&confinement, &effective, &[]);
        let text = repl_text(&effective, &layers, &perimeter);
        assert!(!text.contains("ollama/ignored"));
        assert!(text.contains("budget overridden by system ceiling"));
        assert!(text.contains("tokens=5000"));
    }
}
