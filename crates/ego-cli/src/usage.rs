//! `ego usage` and the shared human rendering used by `/usage`.

use ego_core::run::RunTermination;
use ego_core::usage::{CostBreakdown, UsageLine, UsagePeriod, UsageReport, UsageReportService};
use ego_tools::clock::SystemClock;

use crate::cli::{UsageArgs, UsageError};
use crate::output::Stdout;
use crate::{catalogue, run, session};

/// Report this workspace's durable local sessions.
pub async fn execute(args: &UsageArgs, out: &Stdout) -> RunTermination {
    match report_workspace(args).await {
        Ok(report) => {
            if args.json {
                match serde_json::to_string(&report) {
                    Ok(json) => out.line(&json),
                    Err(error) => {
                        return RunTermination::Failed {
                            message: format!("the usage report could not be rendered: {error}"),
                        };
                    }
                }
            } else {
                out.write(&render_text(&report));
            }
            RunTermination::Done
        }
        Err(termination) => termination,
    }
}

async fn report_workspace(args: &UsageArgs) -> Result<UsageReport, RunTermination> {
    let usage = |error: UsageError| RunTermination::UsageError {
        message: error.to_string(),
    };
    let root = run::workspace(args.directory.as_ref()).map_err(usage)?;
    let store_root = run::rooted_path(
        &root,
        session::store_root(args.session_dir.as_ref()).map_err(usage)?,
    );
    let store = session::jsonl(store_root, root.clone());
    let metadata =
        catalogue::listing(&store, &root, false).map_err(|error| RunTermination::Failed {
            message: format!("the session store could not be listed: {error}"),
        })?;
    let clock = SystemClock;
    UsageReportService::new(&store, &clock)
        .report(&metadata, args.period)
        .await
        .map_err(|error| RunTermination::Failed {
            message: format!("the usage report could not be built: {error}"),
        })
}

/// Render an already-reconciled report without performing accounting.
pub(crate) fn render_text(report: &UsageReport) -> String {
    let period = match report.period {
        UsagePeriod::All => "all".to_string(),
        UsagePeriod::Last { seconds } => format!("last {}s", seconds.get()),
    };
    let mut rows = vec![format!(
        "usage as_of={} period={period}",
        report.as_of.as_millis()
    )];
    for session in &report.sessions {
        rows.push(format!(
            "session {} model={}",
            session.session,
            session.model.as_deref().unwrap_or("unknown")
        ));
        if let Some(lineage) = session.lineage {
            rows.push(format!("  lineage={lineage:?}"));
        }
        if let Some(context) = session.context {
            rows.push(format!(
                "  context used_tokens={} provenance={:?}",
                context.used_tokens, context.provenance
            ));
        }
        append_lines(&mut rows, "  ", &session.lines);
    }
    rows.push("workspace".to_string());
    append_lines(&mut rows, "  ", &report.totals);
    rows.push(String::new());
    rows.join("\n")
}

fn append_lines(rows: &mut Vec<String>, prefix: &str, lines: &ego_core::usage::UsageLines) {
    for (name, line) in [
        ("main", &lines.main),
        ("judge", &lines.judge),
        ("compaction", &lines.compaction),
        ("subagent", &lines.subagent),
        ("hook", &lines.hook),
        ("total", &lines.total),
    ] {
        rows.push(format!(
            "{prefix}{name}: {} cost={}",
            usage_text(line),
            cost_text(&line.cost)
        ));
    }
}

fn usage_text(line: &UsageLine) -> String {
    let usage = &line.usage;
    let reasoning = usage
        .reasoning_tokens
        .map_or_else(|| "unknown".to_string(), |tokens| tokens.to_string());
    format!(
        "uncached_input={} cache_read={} cache_write={} cache_write_extended={} output={} reasoning={reasoning}",
        usage.uncached_input_tokens,
        usage.cache_read_tokens,
        usage.cache_write_tokens,
        usage.cache_write_extended_tokens,
        usage.output_tokens,
    )
}

fn cost_text(cost: &CostBreakdown) -> String {
    let mut parts: Vec<String> = cost
        .known
        .iter()
        .map(|money| format!("{} {}", money.amount, money.currency))
        .collect();
    parts.extend(
        cost.unknown
            .iter()
            .map(|reason| serde_json::to_string(reason).unwrap_or_else(|_| format!("{reason:?}"))),
    );
    if parts.is_empty() {
        "none".to_string()
    } else {
        parts.join(", ")
    }
}
