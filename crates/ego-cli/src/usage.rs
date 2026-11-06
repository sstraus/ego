//! `ego usage` and the shared human rendering used by `/usage`.

mod context;

use chrono::{Local, TimeZone};
use ego_core::cost::Money;
use ego_core::run::RunTermination;
use ego_core::state::ContextWindow;
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
                out.write(&render_text(&report, args.all_roles, None));
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
    let mut report = UsageReportService::new(&store, &clock)
        .report(&metadata, args.period)
        .await
        .map_err(|error| RunTermination::Failed {
            message: format!("the usage report could not be built: {error}"),
        })?;
    if args.json
        && report.sessions.iter().any(|session| {
            session
                .model
                .as_deref()
                .is_some_and(|model| model.starts_with("openrouter/"))
        })
    {
        report.provider_limits = current_openrouter_limits().await.into_iter().collect();
    }
    Ok(report)
}

/// A bounded, free key lookup. Failure means absence, never a zero or a guessed balance.
async fn current_openrouter_limits() -> Option<ego_core::usage::ProviderLimitsObservation> {
    let effective = crate::config::user_effective().ok()?;
    if effective.network == ego_core::config::Network::Off {
        return None;
    }
    let row = crate::model::rows(&effective.auth)
        .ok()?
        .into_iter()
        .find(|row| row.id() == "openrouter")?;
    // No keychain or credential command is needed for the OpenRouter environment key.
    let credential = row
        .credential
        .resolve(&ego_ai::provider::EnvCredentials)
        .ok()?;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let scope =
            crate::provider_http::CommandHttpScope::online(ego_core::turn::CancelToken::new())
                .await
                .ok()?;
        ego_ai::usage_limits::openrouter_key_limits(&row, &credential, scope.http(), &SystemClock)
            .await
            .ok()
            .flatten()
    })
    .await
    .ok()
    .flatten()
}

/// The columns of the text table, in order.
///
/// Cache hit counts reads only; writes remain visible in the cached-token total.
const COLUMNS: [&str; 8] = [
    "session",
    "model",
    "role",
    "in",
    "out",
    "cached",
    "cache_hit",
    "cost",
];

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
///
/// `context` is the active session's window, which only `/usage` has; the
/// workspace report of `ego usage` spans sessions and passes `None`.
pub(crate) fn render_text(
    report: &UsageReport,
    all_roles: bool,
    context: Option<ContextWindow>,
) -> String {
    render_text_in(report, all_roles, context, &Local, None)
}

/// Render interactive usage within the current terminal width.
pub(crate) fn render_interactive(
    report: &UsageReport,
    context: ContextWindow,
    width: usize,
) -> String {
    render_text_in(report, false, Some(context), &Local, Some(width.max(1)))
}

/// [`render_text`] with the time zone `as_of` is written in made explicit.
fn render_text_in<Tz: TimeZone>(
    report: &UsageReport,
    all_roles: bool,
    context: Option<ContextWindow>,
    zone: &Tz,
    width: Option<usize>,
) -> String
where
    Tz::Offset: std::fmt::Display,
{
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
        "usage as of {} period={period}",
        local_stamp(zone, report.as_of.as_millis())
    )];
    rows.extend(context.map(context_line));
    let table = aligned(&cells);
    if width.is_some_and(|width| table.iter().any(|row| row.chars().count() > width)) {
        // Keep the full identity, but do not let it push accounting cells off-screen.
        for row in cells.iter().skip(1) {
            rows.push(format!("session {} · model {}", row[0], row[1]));
            rows.extend(aligned(&[cells[0][2..].to_vec(), row[2..].to_vec()]));
        }
    } else {
        rows.extend(table);
    }
    rows.extend(context::lines(report));
    rows.push(String::new());
    rows.join("\n")
}

/// An instant as a minute-precision calendar time in `zone`.
fn local_stamp<Tz: TimeZone>(zone: &Tz, millis: u64) -> String
where
    Tz::Offset: std::fmt::Display,
{
    i64::try_from(millis)
        .ok()
        .and_then(|millis| zone.timestamp_millis_opt(millis).single())
        .map_or_else(
            || "an unrepresentable time".to_string(),
            |at| at.format("%Y-%m-%d %H:%M").to_string(),
        )
}

/// The active session's context occupancy against its window.
pub(crate) fn context_line(context: ContextWindow) -> String {
    match (context.used_tokens, context.window) {
        (Some(used), Some(window)) if window > 0 => format!(
            "context {used} of {window} tokens ({:.1}%)",
            used as f64 * 100.0 / window as f64
        ),
        (Some(used), _) => format!("context {used} tokens (window unknown)"),
        (None, Some(window)) => format!("context unknown of {window} tokens"),
        (None, None) => "context unknown".to_string(),
    }
}

/// Every accounting line of one set, in the fixed report order.
fn role_lines(lines: &ego_core::usage::UsageLines) -> [(&'static str, &UsageLine); 7] {
    [
        ("main", &lines.main),
        ("judge", &lines.judge),
        ("goal_check", &lines.goal_check),
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
        || !line.cost.subscription_equivalent.is_empty()
        || !line.cost.subscription_unknown.is_empty()
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
        line.cache_hit().map_or_else(
            || "unknown".to_owned(),
            |hit| format!("{:.1}%", hit * 100.0),
        ),
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
        .map(money_text)
        .collect::<Vec<_>>()
        .join(", ");
    let mut parts = match (known.is_empty(), cost.unknown.is_empty()) {
        (true, true) => Vec::new(),
        (true, false) => vec!["unknown".to_owned()],
        (false, true) => vec![known],
        (false, false) => vec![format!("{known} +unknown")],
    };
    parts.extend(cost.subscription_equivalent.iter().map(|amount| {
        let text = format!(
            "{} subscription-equivalent (standard API)",
            money_text(amount)
        );
        if cost.subscription_unknown.is_empty() {
            text
        } else {
            format!("{text} (partial; unpriced)")
        }
    }));
    if !cost.subscription_unknown.is_empty() {
        parts.push("unpriced nonmonetary usage (no API equivalent)".to_owned());
    }
    if parts.is_empty() {
        "-".to_owned()
    } else {
        parts.join(" + ")
    }
}

/// An amount at four decimals, rounded half up.
///
/// A nonzero amount that rounds to nothing reads `<0.0001`, never `0.0000`: a
/// zero that is not one is the false precision this view exists to avoid. The
/// exact amount stays in `--json`.
fn money_text(money: &Money) -> String {
    const PLACES: u32 = 4;
    let (coefficient, scale) = (money.amount.coefficient(), money.amount.scale());
    let hundredths = if scale <= PLACES {
        coefficient.checked_mul(10u128.pow(PLACES - scale))
    } else {
        let divisor = 10u128.pow(scale - PLACES);
        let rounded_up = coefficient % divisor * 2 >= divisor;
        Some(coefficient / divisor + u128::from(rounded_up))
    };
    let Some(scaled) = hundredths else {
        return format!("{} {}", money.amount, money.currency);
    };
    if scaled == 0 && coefficient != 0 {
        return format!("<0.0001 {}", money.currency);
    }
    let unit = 10u128.pow(PLACES);
    format!("{}.{:04} {}", scaled / unit, scaled % unit, money.currency)
}

#[cfg(test)]
mod text_red {
    use super::*;
    use chrono::FixedOffset;
    use ego_core::cost::{Amount, CurrencyCode, UnknownCost};
    use ego_core::event::time::Timestamp;
    use ego_core::state::Usage;
    use ego_core::usage::UsageLines;

    fn line(cost: CostBreakdown) -> UsageLine {
        UsageLine {
            usage: Usage::IDENTITY,
            cost,
        }
    }

    fn report(as_of: u64, total: CostBreakdown) -> UsageReport {
        let empty = || line(none());
        UsageReport {
            v: ego_core::usage::USAGE_REPORT_VERSION,
            as_of: Timestamp::from_millis(as_of),
            period: UsagePeriod::All,
            sessions: Vec::new(),
            provider_limits: Vec::new(),
            totals: UsageLines {
                main: empty(),
                judge: empty(),
                goal_check: empty(),
                compaction: empty(),
                subagent: empty(),
                hook: empty(),
                total: line(total),
            },
        }
    }

    fn none() -> CostBreakdown {
        CostBreakdown {
            known: Vec::new(),
            unknown: Vec::new(),
            subscription_equivalent: Vec::new(),
            subscription_unknown: Vec::new(),
        }
    }

    fn usd(coefficient: u128, scale: u32) -> CostBreakdown {
        CostBreakdown {
            known: vec![Money::new(
                Amount::new(coefficient, scale).expect("amount"),
                CurrencyCode::new("USD").expect("currency"),
            )],
            unknown: Vec::new(),
            subscription_equivalent: Vec::new(),
            subscription_unknown: Vec::new(),
        }
    }

    /// Catches: the header printing the raw epoch milliseconds, or the UTC
    /// calendar time where the reader's own zone was wanted.
    #[test]
    fn the_header_shows_a_local_date_and_not_an_epoch_number() {
        // 2026-09-29 06:13:31 UTC.
        let as_of = 1_790_662_411_741;
        let zone = FixedOffset::east_opt(2 * 3600).expect("offset");
        let text = render_text_in(&report(as_of, none()), false, None, &zone, None);
        let header = text.lines().next().expect("header");
        assert_eq!(header, "usage as of 2026-09-29 08:13 period=all");
        assert!(!text.contains(&as_of.to_string()), "{text}");
    }

    /// Catches: eight-decimal costs, and a tiny nonzero cost shown as 0.0000.
    #[test]
    fn cost_has_four_decimals_and_a_tiny_cost_is_not_a_false_zero() {
        for (coefficient, scale, expected) in [
            (156_247, 8, "0.0016 USD"),
            (15, 1, "1.5000 USD"),
            (2, 0, "2.0000 USD"),
            (99_995, 7, "0.0100 USD"),
            (1, 8, "<0.0001 USD"),
        ] {
            assert_eq!(
                cost_text(&usd(coefficient, scale)),
                expected,
                "{coefficient}e-{scale}"
            );
        }
    }

    /// Catches: an unpriced model rendered as a priced amount.
    #[test]
    fn an_unpriced_total_reads_unknown_and_not_a_number() {
        let unpriced = CostBreakdown {
            known: Vec::new(),
            unknown: vec![UnknownCost::NoPricingQuote],
            subscription_equivalent: Vec::new(),
            subscription_unknown: Vec::new(),
        };
        assert_eq!(cost_text(&unpriced), "unknown");
    }

    /// Catches: equivalent estimates rendered as spend or missing estimates hidden.
    #[test]
    fn subscription_cost_cells_label_equivalents_and_preserve_metered_zero() {
        let assert_cost_cells = |cost: CostBreakdown, expected: &str| {
            let mut report = report(0, cost.clone());
            let mut lines = report.totals.clone();
            lines.main = line(cost);
            report.sessions.push(ego_core::usage::SessionUsage {
                prompt_cache: None,
                session: "00000000-0000-0000-0000-000000000001"
                    .parse()
                    .expect("session id"),
                model: Some("fixture".to_owned()),
                lineage: None,
                context: None,
                context_capacity: None,
                provider_limits: None,
                context_attribution: Default::default(),
                lines,
            });
            let text = render_text(&report, false, None);
            let rows: Vec<_> = text
                .lines()
                .skip(2)
                .map(|row| row.split_whitespace().collect::<Vec<_>>())
                .collect();
            assert_eq!(rows.len(), 3, "main, session total and workspace: {text}");
            assert_eq!(rows[0][2], "main", "cost-only role stays visible: {text}");
            for row in rows {
                assert_eq!(row[7..].join(" "), expected, "{text}");
            }
            text
        };
        let mut subscription = none();
        subscription.subscription_equivalent = usd(11_458, 6).known;
        let label = "0.0115 USD subscription-equivalent (standard API)";
        let text = assert_cost_cells(subscription.clone(), label);
        assert!(!text.contains("0.0000 USD"), "{text}");
        subscription.known = usd(0, 0).known;
        assert_cost_cells(subscription.clone(), &format!("0.0000 USD + {label}"));
        subscription
            .subscription_unknown
            .push(UnknownCost::NoPricingQuote);
        assert_cost_cells(
            subscription.clone(),
            &format!(
                "0.0000 USD + {label} (partial; unpriced) + unpriced nonmonetary usage (no API equivalent)"
            ),
        );
        subscription.subscription_equivalent.clear();
        subscription.known.clear();
        let text = assert_cost_cells(
            subscription,
            "unpriced nonmonetary usage (no API equivalent)",
        );
        assert!(!text.contains("0.0000 USD"), "{text}");
        assert_cost_cells(usd(0, 0), "0.0000 USD");
    }

    /// Catches: `/usage` omitting the window, or computing the percent against
    /// the wrong whole.
    #[test]
    fn the_context_line_has_used_tokens_window_and_percent() {
        let context = ContextWindow {
            used_tokens: Some(50_000),
            window: Some(200_000),
            ..ContextWindow::default()
        };
        let zone = FixedOffset::east_opt(0).expect("offset");
        let text = render_text_in(&report(0, none()), false, Some(context), &zone, None);
        assert!(
            text.lines()
                .any(|row| row == "context 50000 of 200000 tokens (25.0%)"),
            "{text}"
        );
        assert_eq!(context_line(ContextWindow::default()), "context unknown");
    }

    /// Catches: the readable text view changing the machine form. `--json`
    /// serialises the report as it always did; this pins its bytes.
    #[test]
    fn the_json_form_is_unchanged_by_the_text_view() {
        let json =
            serde_json::to_string(&report(1_790_662_411_741, usd(156_247, 8))).expect("serialise");
        assert_eq!(
            json,
            r#"{"v":5,"as_of":1790662411741,"period":{"kind":"all"},"sessions":[],"totals":{"main":{"usage":{"uncached_input_tokens":0,"cache_read_tokens":0,"cache_write_tokens":0,"cache_write_extended_tokens":0,"output_tokens":0,"reasoning_tokens":0},"cost":{"known":[],"unknown":[],"subscription_equivalent":[],"subscription_unknown":[]}},"judge":{"usage":{"uncached_input_tokens":0,"cache_read_tokens":0,"cache_write_tokens":0,"cache_write_extended_tokens":0,"output_tokens":0,"reasoning_tokens":0},"cost":{"known":[],"unknown":[],"subscription_equivalent":[],"subscription_unknown":[]}},"goal_check":{"usage":{"uncached_input_tokens":0,"cache_read_tokens":0,"cache_write_tokens":0,"cache_write_extended_tokens":0,"output_tokens":0,"reasoning_tokens":0},"cost":{"known":[],"unknown":[],"subscription_equivalent":[],"subscription_unknown":[]}},"compaction":{"usage":{"uncached_input_tokens":0,"cache_read_tokens":0,"cache_write_tokens":0,"cache_write_extended_tokens":0,"output_tokens":0,"reasoning_tokens":0},"cost":{"known":[],"unknown":[],"subscription_equivalent":[],"subscription_unknown":[]}},"subagent":{"usage":{"uncached_input_tokens":0,"cache_read_tokens":0,"cache_write_tokens":0,"cache_write_extended_tokens":0,"output_tokens":0,"reasoning_tokens":0},"cost":{"known":[],"unknown":[],"subscription_equivalent":[],"subscription_unknown":[]}},"hook":{"usage":{"uncached_input_tokens":0,"cache_read_tokens":0,"cache_write_tokens":0,"cache_write_extended_tokens":0,"output_tokens":0,"reasoning_tokens":0},"cost":{"known":[],"unknown":[],"subscription_equivalent":[],"subscription_unknown":[]}},"total":{"usage":{"uncached_input_tokens":0,"cache_read_tokens":0,"cache_write_tokens":0,"cache_write_extended_tokens":0,"output_tokens":0,"reasoning_tokens":0},"cost":{"known":[{"amount":"0.00156247","currency":"USD"}],"unknown":[],"subscription_equivalent":[],"subscription_unknown":[]}}}}"#
        );
    }
    // Catches: human output swaps request/result estimates or drops duplicate source and provenance.
    #[test]
    fn tool_context_text_and_json_preserve_the_same_estimates_and_source_links() {
        let mut report = report(0, none());
        let mut value = serde_json::json!({
            "session": "00000000-0000-0000-0000-000000000001",
            "model": "fixture",
            "context_attribution": {
                "source": "durable_tool_events",
                "provenance": "utf8_bytes_div_4_ceil",
                "scope": "local_session_requests_in_report_period",
                "unmatched_results": 1,
                "tools": [{
                    "tool": "read", "request_estimated_tokens": 7,
                    "result_estimated_tokens": 19, "duplicate_read_estimated_tokens": 26,
                    "calls": [{
                        "item": "00000000-0000-0000-0000-000000000002",
                        "request_seq": 8, "request_bytes": 25, "request_estimated_tokens": 7,
                        "result": {"seq": 9, "bytes": 73, "estimated_tokens": 19,
                            "provenance": "builtin", "ok": true, "duplicate_of_request_seq": 3}
                    }]
                }]
            }
        });
        value["lines"] = serde_json::to_value(&report.totals).unwrap();
        report.sessions.push(serde_json::from_value(value).unwrap());
        let before = serde_json::to_value(&report).unwrap();
        let text = render_text(&report, false, None);
        assert!(
            text.contains("read: request ~7 result ~19 duplicate-read ~26 tokens"),
            "{text}"
        );
        assert!(text.contains("source=durable_tool_events provenance=utf8_bytes_div_4_ceil scope=local_session_requests_in_report_period"), "{text}");
        assert!(text.contains("request-seq:8 25 bytes ~7 tokens; result-seq:9 73 bytes ~19 tokens provenance=builtin ok=true duplicate-of=request-seq:3"), "{text}");
        assert!(
            text.contains("unmatched results: 1 (not attributed)"),
            "{text}"
        );
        assert!(text.contains("Not billing or provider occupancy"), "{text}");
        assert_eq!(serde_json::to_value(&report).unwrap(), before);
    }
}
