//! Deterministic operator inspection; no generation, store writes or inbox reads.

use std::path::Path;
use std::time::Duration;

use ego_core::orchestrator::{Brief, Filter};
use ego_core::run::RunTermination;
use ego_tools::orchestrator::{project_brief, project_status, render_brief};

use crate::cli::OrchestratorArgs;
use crate::output::Stdout;

pub async fn execute(args: &OrchestratorArgs, out: &Stdout) -> RunTermination {
    match inspect(args).await {
        Ok(brief) => {
            out.line(&render_brief(&brief));
            RunTermination::Done
        }
        Err(message) => RunTermination::Failed { message },
    }
}

async fn inspect(args: &OrchestratorArgs) -> Result<Brief, String> {
    if std::env::var_os("TUIC_SOCKET").is_none_or(|socket| socket.is_empty()) {
        return Err("ego orchestrator brief requires TUIC_SOCKET".to_owned());
    }
    let home = std::env::var_os("HOME").ok_or("ego orchestrator brief requires HOME")?;
    let filter = Filter {
        root: args
            .root
            .as_ref()
            .map(std::fs::canonicalize)
            .transpose()
            .map_err(|error| format!("invalid brief root: {error}"))?,
    };
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new("tuic")
            .args(["mcp", "session", r#"{"action":"list"}"#])
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| "TUIC brief session read timed out".to_owned())?
    .map_err(|error| format!("TUIC brief session read failed: {error}"))?;
    if !output.status.success() {
        return Err(format!("TUIC brief session read failed: {}", output.status));
    }
    if output.stdout.len() > 1024 * 1024 {
        return Err("TUIC brief session read exceeded 1 MiB".to_owned());
    }
    let sessions = String::from_utf8(output.stdout).map_err(|error| error.to_string())?;
    let children = optional_text(&Path::new(&home).join("Gits/.tmp/coordinator/children.tsv"))?;
    let children = project_status(&sessions, &children, &filter)?;
    let results = Path::new(&home).join("Gits/.tmp/results");
    let stamp = match std::fs::metadata(results.join(".watch-stamp")) {
        Ok(meta) => Some(meta.modified().map_err(|error| error.to_string())?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.to_string()),
    };
    let mut unread = Vec::new();
    if let Some(stamp) = stamp {
        for entry in std::fs::read_dir(&results).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "md")
                && entry
                    .metadata()
                    .map_err(|error| error.to_string())?
                    .modified()
                    .map_err(|error| error.to_string())?
                    > stamp
            {
                unread.push(path.to_string_lossy().into_owned());
            }
        }
    }
    Ok(project_brief(children, &unread.join("\n")))
}

fn optional_text(path: &Path) -> Result<String, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error.to_string()),
    }
}
