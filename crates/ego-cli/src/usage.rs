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
                out.write(&render_text(&report, args.all_roles));
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

/// The columns of the text table, in order.
///
/// Six facts and no more: the report is the complete record and `--json`
/// hands it over whole. The text view exists for a person deciding whether a
/// session cost anything, and every column it does not have is one that was
/// pushing the answer off the right-hand edge.
const COLUMNS: [&str; 7] = ["session", "model", "role", "in", "out", "cached", "cost"];

/// Render an already-reconciled report without performing accounting.
///
/// The `total` line is not one of the hideable roles: it is how a session
/// appears in the table at all, so a session that spent nothing still has a
/// row saying so rather than vanishing from its own report.
///
/// `all_roles` keeps the role rows that recorded nothing. Hidden by default
/// because four of the five are zero in every session nobody judged, compacted,
/// forked or hooked, and five zero rows per session is how the one row that
/// mattered got lost.
pub(crate) fn render_text(report: &UsageReport, all_roles: bool) -> String {
    let period = match report.period {
        UsagePeriod::All => "all".to_string(),
        UsagePeriod::Last { seconds } => format!("last {}s", seconds.get()),
    };
    let mut cells = vec![COLUMNS.map(str::to_owned).to_vec()];
    for session in &report.sessions {
        let id = session.session.to_string();
        let model = session
            .model
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        for (role, line) in role_lines(&session.lines) {
            if !all_roles && role != "total" && !spent(line) {
                continue;
            }
            cells.push(row(&id, &model, role, line));
        }
    }
    cells.push(row("workspace", "-", "total", &report.totals.total));

    let mut rows = vec![format!(
        "usage as_of={} period={period}",
        report.as_of.as_millis()
    )];
    rows.extend(aligned(&cells));
    rows.push(String::new());
    rows.join("\n")
}

/// Every accounting line of one set, in the fixed report order.
fn role_lines(lines: &ego_core::usage::UsageLines) -> [(&'static str, &UsageLine); 6] {
    [
        ("main", &lines.main),
        ("judge", &lines.judge),
        ("compaction", &lines.compaction),
        ("subagent", &lines.subagent),
        ("hook", &lines.hook),
        ("total", &lines.total),
    ]
}

/// Whether a role line records anything at all.
///
/// A cost with no tokens is still activity — an unknown-cost reason is
/// something the reader has to see — so this is not a token test alone.
fn spent(line: &UsageLine) -> bool {
    let usage = &line.usage;
    usage.uncached_input_tokens > 0
        || usage.cache_read_tokens > 0
        || usage.cache_write_tokens > 0
        || usage.cache_write_extended_tokens > 0
        || usage.output_tokens > 0
        || usage.reasoning_tokens.is_some_and(|tokens| tokens > 0)
        || !line.cost.known.is_empty()
        || !line.cost.unknown.is_empty()
}

fn row(session: &str, model: &str, role: &str, line: &UsageLine) -> Vec<String> {
    let usage = &line.usage;
    let cached =
        usage.cache_read_tokens + usage.cache_write_tokens + usage.cache_write_extended_tokens;
    vec![
        session.to_owned(),
        model.to_owned(),
        role.to_owned(),
        usage.uncached_input_tokens.to_string(),
        usage.output_tokens.to_string(),
        cached.to_string(),
        cost_text(&line.cost),
    ]
}

/// Pad every column to its widest cell, so the numbers line up in a terminal.
///
/// The last column is never padded: trailing spaces are invisible and a reader
/// that splits on whitespace should not have to strip them.
fn aligned(cells: &[Vec<String>]) -> Vec<String> {
    let mut widths = vec![0usize; COLUMNS.len()];
    for row in cells {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(cell.chars().count());
        }
    }
    cells
        .iter()
        .map(|row| {
            let last = row.len() - 1;
            row.iter()
                .enumerate()
                .map(|(index, cell)| {
                    if index == last {
                        cell.clone()
                    } else {
                        format!("{cell:<width$}", width = widths[index])
                    }
                })
                .collect::<Vec<_>>()
                .join("  ")
        })
        .collect()
}

/// One compact cost cell.
///
/// The unknown-cost *reasons* stay in the report rather than the table: they
/// are JSON objects, and one of them is wider than every other column
/// together. The cell says that some spend is unpriced; `--json` says why.
fn cost_text(cost: &CostBreakdown) -> String {
    let known = cost
        .known
        .iter()
        .map(|money| format!("{} {}", money.amount, money.currency))
        .collect::<Vec<_>>()
        .join(", ");
    match (known.is_empty(), cost.unknown.is_empty()) {
        (true, true) => "-".to_string(),
        (true, false) => "unknown".to_string(),
        (false, true) => known,
        (false, false) => format!("{known} +unknown"),
    }
}
