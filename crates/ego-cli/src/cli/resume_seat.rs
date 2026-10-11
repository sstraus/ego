use super::{Invocation, SessionChoice, Stdin};

impl Invocation {
    /// Resolve resume's driver after capturing the selected profile and before
    /// installing diagnostics or admitting a run. Explicit machine inputs keep
    /// their one-shot contract even when launched from a human terminal.
    pub fn with_terminal(self, stdin_terminal: bool, stdout_terminal: bool) -> Self {
        let args = match self {
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
            Self::Resume(args)
        }
    }
}
