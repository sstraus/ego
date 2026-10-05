//! Windows admission for a direct unconfined start.
//!
//! The matrix is pure so it can be proved on every development host.
//! Filesystem and terminal effects remain in the direct CLI adapter below it.

use ego_core::permit::Confinement;

/// The authority available at a direct CLI start.
#[cfg(any(windows, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartKind {
    Direct,
    InteractiveNoHuman,
    InteractiveHuman,
}

/// Facts needed by the pure Windows admission policy.
#[cfg(any(windows, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AdmissionInput {
    start: StartKind,
    cli_sandbox: Option<Confinement>,
    effective_sandbox: Option<Confinement>,
    offline: bool,
    acknowledged: bool,
}

/// The action one direct CLI start must take.
#[cfg(any(windows, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Admission {
    RequireExplicitOff,
    RequireAcknowledgement,
    ProceedOff,
    Preserve,
}

/// Decide the Windows unconfined-start policy without performing effects.
#[cfg(any(windows, test))]
const fn decide(input: AdmissionInput) -> Admission {
    match input.start {
        StartKind::Direct | StartKind::InteractiveNoHuman => {
            if !matches!(input.cli_sandbox, Some(Confinement::Off)) {
                return Admission::RequireExplicitOff;
            }
            if input.offline || !matches!(input.effective_sandbox, Some(Confinement::Off)) {
                Admission::Preserve
            } else {
                Admission::ProceedOff
            }
        }
        StartKind::InteractiveHuman if input.offline => Admission::Preserve,
        StartKind::InteractiveHuman => match (input.cli_sandbox, input.effective_sandbox) {
            (_, Some(Confinement::Off)) | (Some(Confinement::Off), None) => Admission::ProceedOff,
            (_, Some(Confinement::Workspace | Confinement::Ro))
            | (Some(Confinement::Workspace | Confinement::Ro), None) => Admission::Preserve,
            (None, None) if input.acknowledged => Admission::ProceedOff,
            (None, None) => Admission::RequireAcknowledgement,
        },
    }
}

#[cfg(windows)]
pub(super) fn admit(
    args: &super::Args,
    effective_sandbox: Option<Confinement>,
    offline: bool,
    driver: &super::Driver<'_>,
    root: &std::path::Path,
    home: Option<&std::path::Path>,
    diagnostics: &dyn crate::output::Diagnostics,
) -> Result<Option<Confinement>, crate::cli::UsageError> {
    let (start, human_seat) = match driver {
        super::Driver::Once => (StartKind::Direct, None),
        super::Driver::Interactive { seat, .. } if seat.has_human() => {
            (StartKind::InteractiveHuman, Some(seat))
        }
        super::Driver::Interactive { .. } => (StartKind::InteractiveNoHuman, None),
    };
    match decide(AdmissionInput {
        start,
        cli_sandbox: args.sandbox,
        effective_sandbox,
        offline,
        acknowledged: false,
    }) {
        Admission::ProceedOff | Admission::Preserve => Ok(None),
        Admission::RequireExplicitOff => Err(crate::cli::UsageError(
            "Windows direct execution is unconfined; pass the explicit CLI flag \
             `--sandbox off` to proceed"
                .to_owned(),
        )),
        Admission::RequireAcknowledgement => {
            let seat = human_seat.expect("only a human interactive start requests acknowledgement");
            let accepted = crate::trust::resolve_unconfined(root, home, diagnostics, || {
                seat.acknowledge_windows_unconfined(root)
            });
            if accepted {
                Ok(Some(Confinement::Off))
            } else {
                Err(crate::cli::UsageError(
                    "Windows execution is unconfined and was not acknowledged; \
                     use `--sandbox off` only when that is acceptable"
                        .to_owned(),
                ))
            }
        }
    }
}

#[cfg(not(windows))]
pub(super) fn admit(
    _args: &super::Args,
    _effective_sandbox: Option<Confinement>,
    _offline: bool,
    _driver: &super::Driver<'_>,
    _root: &std::path::Path,
    _home: Option<&std::path::Path>,
    _diagnostics: &dyn crate::output::Diagnostics,
) -> Result<Option<Confinement>, crate::cli::UsageError> {
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(
        start: StartKind,
        cli_sandbox: Option<Confinement>,
        effective_sandbox: Option<Confinement>,
        offline: bool,
        acknowledged: bool,
    ) -> AdmissionInput {
        AdmissionInput {
            start,
            cli_sandbox,
            effective_sandbox,
            offline,
            acknowledged,
        }
    }

    #[test]
    fn every_windows_admission_pair_has_one_explicit_offline_and_confinement_result() {
        let cases: &[(&str, AdmissionInput, Admission)] = &[
            (
                "direct missing",
                input(StartKind::Direct, None, None, false, false),
                Admission::RequireExplicitOff,
            ),
            (
                "direct config-only off",
                input(
                    StartKind::Direct,
                    None,
                    Some(Confinement::Off),
                    false,
                    false,
                ),
                Admission::RequireExplicitOff,
            ),
            (
                "direct missing offline",
                input(StartKind::Direct, None, None, true, false),
                Admission::RequireExplicitOff,
            ),
            (
                "direct config-only off offline",
                input(StartKind::Direct, None, Some(Confinement::Off), true, false),
                Admission::RequireExplicitOff,
            ),
            (
                "no-human missing",
                input(StartKind::InteractiveNoHuman, None, None, false, false),
                Admission::RequireExplicitOff,
            ),
            (
                "no-human missing offline",
                input(StartKind::InteractiveNoHuman, None, None, true, false),
                Admission::RequireExplicitOff,
            ),
            (
                "no-human config-only off offline",
                input(
                    StartKind::InteractiveNoHuman,
                    None,
                    Some(Confinement::Off),
                    true,
                    false,
                ),
                Admission::RequireExplicitOff,
            ),
            (
                "no-human config-only off",
                input(
                    StartKind::InteractiveNoHuman,
                    None,
                    Some(Confinement::Off),
                    false,
                    false,
                ),
                Admission::RequireExplicitOff,
            ),
            (
                "direct explicit off online",
                input(
                    StartKind::Direct,
                    Some(Confinement::Off),
                    Some(Confinement::Off),
                    false,
                    false,
                ),
                Admission::ProceedOff,
            ),
            (
                "no-human explicit off online",
                input(
                    StartKind::InteractiveNoHuman,
                    Some(Confinement::Off),
                    Some(Confinement::Off),
                    false,
                    false,
                ),
                Admission::ProceedOff,
            ),
            (
                "direct explicit off offline",
                input(
                    StartKind::Direct,
                    Some(Confinement::Off),
                    Some(Confinement::Off),
                    true,
                    false,
                ),
                Admission::Preserve,
            ),
            (
                "no-human explicit off offline",
                input(
                    StartKind::InteractiveNoHuman,
                    Some(Confinement::Off),
                    Some(Confinement::Off),
                    true,
                    false,
                ),
                Admission::Preserve,
            ),
            (
                "direct explicit off with workspace",
                input(
                    StartKind::Direct,
                    Some(Confinement::Off),
                    Some(Confinement::Workspace),
                    false,
                    false,
                ),
                Admission::Preserve,
            ),
            (
                "no-human explicit off with ro",
                input(
                    StartKind::InteractiveNoHuman,
                    Some(Confinement::Off),
                    Some(Confinement::Ro),
                    false,
                    false,
                ),
                Admission::Preserve,
            ),
            (
                "human offline implicit",
                input(StartKind::InteractiveHuman, None, None, true, false),
                Admission::Preserve,
            ),
            (
                "human offline configured off",
                input(
                    StartKind::InteractiveHuman,
                    None,
                    Some(Confinement::Off),
                    true,
                    false,
                ),
                Admission::Preserve,
            ),
            (
                "human offline explicit off",
                input(
                    StartKind::InteractiveHuman,
                    Some(Confinement::Off),
                    Some(Confinement::Off),
                    true,
                    false,
                ),
                Admission::Preserve,
            ),
            (
                "human explicit off",
                input(
                    StartKind::InteractiveHuman,
                    Some(Confinement::Off),
                    None,
                    false,
                    false,
                ),
                Admission::ProceedOff,
            ),
            (
                "human configured off",
                input(
                    StartKind::InteractiveHuman,
                    None,
                    Some(Confinement::Off),
                    false,
                    false,
                ),
                Admission::ProceedOff,
            ),
            (
                "human explicit and configured off",
                input(
                    StartKind::InteractiveHuman,
                    Some(Confinement::Off),
                    Some(Confinement::Off),
                    false,
                    false,
                ),
                Admission::ProceedOff,
            ),
            (
                "human explicit workspace",
                input(
                    StartKind::InteractiveHuman,
                    Some(Confinement::Workspace),
                    Some(Confinement::Workspace),
                    false,
                    false,
                ),
                Admission::Preserve,
            ),
            (
                "human explicit ro without configured value",
                input(
                    StartKind::InteractiveHuman,
                    Some(Confinement::Ro),
                    None,
                    false,
                    false,
                ),
                Admission::Preserve,
            ),
            (
                "human configured ro",
                input(
                    StartKind::InteractiveHuman,
                    None,
                    Some(Confinement::Ro),
                    false,
                    false,
                ),
                Admission::Preserve,
            ),
            (
                "human implicit unacknowledged",
                input(StartKind::InteractiveHuman, None, None, false, false),
                Admission::RequireAcknowledgement,
            ),
            (
                "human implicit acknowledged",
                input(StartKind::InteractiveHuman, None, None, false, true),
                Admission::ProceedOff,
            ),
        ];

        for (name, facts, expected) in cases {
            assert_eq!(decide(*facts), *expected, "{name}");
        }
    }
}
