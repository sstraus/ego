use super::{Invocation, SessionChoice, Stdin};

impl Invocation {
    /// Resolve resume's driver after capturing the selected profile and before
    /// installing diagnostics or admitting a run. Explicit machine inputs keep
    /// their one-shot contract even when launched from a human terminal.
    pub fn with_terminal(self, stdin_terminal: bool, stdout_terminal: bool) -> Self {
        let mut args = match self {
            Self::Resume(args) => args,
            Self::Interactive(args) if matches!(args.session, SessionChoice::Resume(_)) => args,
            other => return other,
        };
        if stdin_terminal
            && stdout_terminal
            && !args.output_explicit
            && args.stdin == Stdin::Ambient
            && args.answers.is_empty()
            && args.agent_file.is_none()
            && args.profile.as_deref() != Some("ci")
        {
            Self::Interactive(args)
        } else {
            // Only an implicit interactive default enables recovery; an explicit
            // --retry (including zero) already selected its finite allowance.
            if args.provider_recovery {
                args.retry = 0;
                args.provider_recovery = false;
            }
            Self::Resume(args)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv_slice(args: &[&str]) -> Vec<String> {
        std::iter::once("ego")
            .chain(args.iter().copied())
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn headless_resume_does_not_inherit_interactive_retry_recovery() {
        // Catches: seat conversion keeps six retries and a 24-hour recovery horizon.
        let mut failures = Vec::new();
        for (flags, stdin, stdout) in [
            (vec!["--output", "json"], true, true),
            (vec!["--output=text"], true, true),
            (vec!["--profile", "ci"], true, true),
            (vec![], false, true),
            (vec![], true, false),
            (vec![], false, false),
        ] {
            let mut tokens = vec!["resume"];
            tokens.extend_from_slice(&flags);
            let Invocation::Resume(args) = crate::cli::parse(argv_slice(&tokens))
                .unwrap()
                .with_terminal(stdin, stdout)
            else {
                panic!("headless resume: {tokens:?}");
            };
            if args.retry != 0 || args.provider_recovery {
                failures.push(format!(
                    "{tokens:?}, terminal=({stdin}, {stdout}): retry={}, recovery={}",
                    args.retry, args.provider_recovery
                ));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn resume_seat_resolution_preserves_explicit_retry_limits_and_human_recovery() {
        // Catches: clearing headless defaults also clears an explicit finite allowance.
        for (flags, stdin, stdout, interactive) in [
            (vec![], true, true, true),
            (vec!["--output", "json"], true, true, false),
            (vec!["--profile", "ci"], true, true, false),
            (vec![], false, false, false),
        ] {
            for (value, expected) in [("0", 0), ("2", 2), ("6", 6)] {
                let mut tokens = vec!["resume", "--retry", value];
                tokens.extend_from_slice(&flags);
                let invocation = crate::cli::parse(argv_slice(&tokens))
                    .unwrap()
                    .with_terminal(stdin, stdout);
                assert_eq!(
                    matches!(invocation, Invocation::Interactive(_)),
                    interactive
                );
                let (Invocation::Resume(args) | Invocation::Interactive(args)) = invocation else {
                    panic!("resume seat");
                };
                assert_eq!(args.retry, expected, "{tokens:?}");
                assert!(!args.provider_recovery, "{tokens:?}");
            }
        }
        let Invocation::Interactive(args) = crate::cli::parse(argv_slice(&["resume"]))
            .unwrap()
            .with_terminal(true, true)
        else {
            panic!("human resume");
        };
        assert_eq!(args.retry, 6);
        assert!(args.provider_recovery);
    }
}
