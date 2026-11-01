//! Configuration setup is a human terminal flow, not an agent or doctor probe.

use ego_core::run::RunTermination;

use crate::cli::SetupArgs;
use crate::output::Stdout;

pub async fn execute(args: &SetupArgs, _out: &Stdout) -> RunTermination {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return RunTermination::Failed {
            message: "setup needs a terminal; use ego config ls for non-interactive inspection"
                .into(),
        };
    }
    let Some(home) = crate::config::ego_home() else {
        return RunTermination::Failed {
            message: "setup needs EGO_HOME or HOME for user configuration".into(),
        };
    };
    let workspace = args
        .directory
        .clone()
        .or_else(|| std::env::current_dir().ok());
    let Some(workspace) = workspace else {
        return RunTermination::Failed {
            message: "setup workspace unavailable".into(),
        };
    };
    let mut screen = crate::setup_config::TerminalScreen;
    match crate::setup_config::run(
        &mut screen,
        &workspace,
        &home,
        args.profile.as_deref(),
        None,
    ) {
        Ok(()) => RunTermination::Done,
        Err(message) => RunTermination::Failed { message },
    }
}
