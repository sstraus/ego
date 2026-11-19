//! Operator-selected restore, outside model dispatch and conversation lineage.
use crate::cli::{CheckpointAction, CheckpointArgs};
use crate::output::Stdout;
use ego_core::run::RunTermination;
use ego_tools::checkpoint::CheckpointFs;

pub fn execute(args: &CheckpointArgs, out: &Stdout) -> RunTermination {
    let result = (|| -> Result<String, String> {
        match &args.action {
            CheckpointAction::Preview => serde_json::to_string_pretty(
                &CheckpointFs::preview(&args.directory, args.session).map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string()),
            CheckpointAction::Restore { checkpoint } => serde_json::to_string_pretty(
                &CheckpointFs::restore(&args.directory, args.session, checkpoint)
                    .map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string()),
            CheckpointAction::Close => {
                CheckpointFs::close(&args.directory, args.session).map_err(|e| e.to_string())?;
                Ok(format!("closed checkpoints for {}", args.session))
            }
        }
    })();
    match result {
        Ok(text) => {
            out.line(&text);
            RunTermination::Done
        }
        Err(message) => RunTermination::Failed { message },
    }
}
