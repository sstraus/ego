//! Compact terminal projection of already selected run authority.

use std::path::Path;

use ego_core::config::{Effective, Mode};
use ego_core::permit::{Confinement, RootAccess, RunConfinement, terminal_safe};

use crate::render::Line;

/// A labelled row with continuation text aligned below its value.
pub(crate) fn row(label: &str, value: &str, width: usize) -> String {
    let width = if width == 0 { 80 } else { width.max(40) };
    let prefix = if label.starts_with("! ") {
        format!("{label:<10} ")
    } else {
        format!("  {label:<7}  ")
    };
    Line::plain(format!("{prefix}{}", terminal_safe(value)))
        .wrap_hanging(width, 11)
        .into_iter()
        .map(|line| line.text())
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn render(
    model: &str,
    auth: &str,
    effective: &Effective,
    confinement: &RunConfinement,
    attachment: Option<&Path>,
    width: usize,
) -> String {
    let mut rows = vec![
        row("model", model, width),
        row("auth", auth, width),
        row(
            "mode",
            &format!(
                "{} ({})",
                effective.mode.unwrap_or(Mode::Default),
                effective.mode_provenance
            ),
            width,
        ),
        row(
            "sandbox",
            &format!("{} ({})", confinement.level(), effective.sandbox_provenance),
            width,
        ),
    ];
    let provider = model
        .split_once('/')
        .map_or(model, |(provider, _)| provider);
    let filter = crate::model::selected_protocol_filter(
        &effective.auth,
        effective.protocol_filter.as_ref(),
        provider,
    );
    if let Some((filter, source)) = filter {
        let command = filter
            .command
            .file_name()
            .unwrap_or(filter.command.as_os_str())
            .to_string_lossy();
        rows.push(row(
            "filter",
            &format!(
                "{} ({source}) · {command} · /config details",
                if filter.enabled {
                    "enabled"
                } else {
                    "disabled"
                }
            ),
            width,
        ));
    }
    let mut grants = confinement
        .roots()
        .iter()
        .map(|root| (root.path().as_path(), root.access()))
        .collect::<Vec<_>>();
    if let Some(adaptive) = confinement.adaptive_perimeter() {
        for path in &adaptive.writable {
            if !grants.iter().any(|(given, _)| *given == path.as_path()) {
                let restriction = confinement.roots().iter().find(|root| {
                    root.access() == RootAccess::Read
                        && path.is_or_under(root.path())
                        && root.restricts_writes(confinement.workspace(), confinement.roots())
                });
                grants.push((
                    path.as_path(),
                    restriction.map_or(RootAccess::ReadWrite, |root| root.access()),
                ));
            }
        }
    }
    let labels = distinct_paths(
        &grants.iter().map(|(path, _)| *path).collect::<Vec<_>>(),
        width,
    );
    for (label, attached) in [("attach", true), ("roots", false)] {
        let group = grants
            .iter()
            .zip(&labels)
            .filter(|((path, _), _)| {
                attachment.is_some_and(|attachment| attachment == *path) == attached
            })
            .collect::<Vec<_>>();
        if group.is_empty() {
            continue;
        }
        rows.push(row(
            label,
            &format!(
                "{} {} · /config perimeter",
                group.len(),
                if group.len() == 1 {
                    "directory"
                } else {
                    "directories"
                }
            ),
            width,
        ));
        for ((_, access), path) in group.iter().take(3) {
            rows.push(row(
                "",
                &format!(
                    "{} {path}",
                    if *access == RootAccess::Read {
                        "ro"
                    } else {
                        "rw"
                    }
                ),
                width,
            ));
        }
        if group.len() > 3 {
            rows.push(row(
                "",
                &format!("+{} more · /config perimeter", group.len() - 3),
                width,
            ));
        }
        if attached {
            rows.push(row(
                "",
                "read-only applies only to this directory; workspace access is separate",
                width,
            ));
        }
    }
    if confinement.level() == Confinement::Off {
        rows.push(row(
            "! security",
            "sandbox off: commands can access your user files",
            width,
        ));
    }
    rows.join("\n")
}

fn distinct_paths(paths: &[&Path], width: usize) -> Vec<String> {
    let components = paths
        .iter()
        .map(|path| {
            path.components()
                .filter(|part| !matches!(part, std::path::Component::RootDir))
                .map(|part| part.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let mut depths = vec![2; paths.len()];
    let mut uuid_lengths = vec![8; paths.len()];
    loop {
        let names = components
            .iter()
            .enumerate()
            .map(|(index, parts)| {
                let tail = parts
                    .iter()
                    .skip(parts.len().saturating_sub(depths[index]))
                    .map(|part| {
                        if let Some(uuid) = part.strip_prefix("pty-")
                            && uuid::Uuid::parse_str(uuid).is_ok()
                        {
                            return format!(
                                "pty-{}",
                                uuid.chars().take(uuid_lengths[index]).collect::<String>()
                            );
                        }
                        part.clone()
                    })
                    .collect::<Vec<_>>()
                    .join("/");
                format!("…/{tail}")
            })
            .collect::<Vec<_>>();
        let mut changed = false;
        for index in 0..names.len() {
            if names.iter().enumerate().any(|(other, name)| {
                other != index && name == &names[index] && paths[index] != paths[other]
            }) {
                if depths[index] < components[index].len() {
                    depths[index] += 1;
                    changed = true;
                } else if uuid_lengths[index] < 36 {
                    uuid_lengths[index] += 1;
                    changed = true;
                }
            }
        }
        if !changed {
            let available = (if width == 0 { 80 } else { width.max(40) }).saturating_sub(14);
            return names
                .into_iter()
                .enumerate()
                .map(|(index, name)| {
                    // A component is indivisible. Use an explicit reference rather than
                    // silently clipping a long name or letting the terminal wrap it.
                    if Line::plain(&name).width() > available {
                        format!("directory {} (full path: /config perimeter)", index + 1)
                    } else {
                        name
                    }
                })
                .collect();
        }
    }
}

#[cfg(test)]
mod tests;
