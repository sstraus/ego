use super::*;

// Catches: the live composer wraps the compact status or drops its core at 120 columns.
#[test]
fn compact_status_in_real_pty_at_120_columns() {
    compact_status_contract(120);
}

// Catches: width pressure hides reasoning, mode, or context before optional fields.
#[test]
fn compact_status_in_real_pty_at_60_columns() {
    compact_status_contract(60);
}

fn compact_status_contract(cols: u16) {
    use ego_core::event::domain::{DomainEvent, Envelope, EventKind, OccupancyProvenance};
    use ego_core::event::id::{EventId, Producer, Seq, SessionId, TurnId};
    use ego_core::event::time::Timestamp;
    use ego_core::session_lifecycle::SessionLifecycleStore;
    use ego_core::store::session_store::SessionStore;
    use ego_tools::store::{JsonlStore, OsIo};

    const ROWS: u16 = 24;
    let stub = Stub::start(Plan {
        context_length: Some(272_000),
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing();
    ego.file("AGENTS.md", "# compact status fixture\n");
    let git = std::process::Command::new("git")
        .current_dir(ego.workspace_path())
        .args(["init", "-b", "feat/compact"])
        .output()
        .unwrap();
    assert!(git.status.success(), "git fixture: {:?}", git.stderr);
    let root = std::fs::canonicalize(ego.workspace_path()).unwrap();
    let store = JsonlStore::new(
        ego.home_path().join("sessions"),
        root.clone(),
        Box::new(OsIo),
    );
    let id: SessionId = support::FIXTURE_SESSION_ID.parse().unwrap();
    store
        .publish_empty_session(id, &root, Timestamp::from_millis(1))
        .unwrap();
    let mut seq = Seq::ZERO;
    seq.next();
    store
        .append(
            &id,
            &DomainEvent {
                env: Envelope {
                    seq,
                    id: EventId::new(),
                    turn: TurnId::new(),
                    item: None,
                    parent: None,
                    ts: Timestamp::from_millis(2),
                    producer: Producer::Main,
                },
                kind: EventKind::ContextWindowUpdated {
                    used_tokens: 58_000,
                    provenance: OccupancyProvenance::GenerationBoundary,
                    window: Some(272_000),
                },
            },
        )
        .unwrap();
    seq.next();
    use ego_core::cost::{
        CostAttribution, CostDelta, CurrencyCode, MeteringRevision, Money, PricingRevision,
        SubscriptionEquivalent,
    };
    store
        .append(
            &id,
            &DomainEvent {
                env: Envelope {
                    seq,
                    id: EventId::new(),
                    turn: TurnId::new(),
                    item: None,
                    parent: None,
                    ts: Timestamp::from_millis(3),
                    producer: Producer::Main,
                },
                kind: EventKind::UsageUpdated {
                    usage: ego_core::state::Usage::IDENTITY,
                    cost_delta: CostDelta::Known {
                        amount: Money::new("0".parse().unwrap(), CurrencyCode::new("USD").unwrap()),
                        attribution: CostAttribution::MeteringPolicy {
                            revision: MeteringRevision("fixture-subscription".into()),
                            equivalent: Some(Box::new(SubscriptionEquivalent::V1 {
                                model: "ollama/qwen3".into(),
                                source: PricingRevision("fixture-prices".into()),
                                service_tier: "standard".into(),
                                amount: Money::new(
                                    "0.661".parse().unwrap(),
                                    CurrencyCode::new("USD").unwrap(),
                                ),
                            })),
                        },
                        fx: None,
                    },
                    attempt: None,
                },
            },
        )
        .unwrap();
    let ego = ego
        .subcommand("resume")
        .arg(support::FIXTURE_SESSION_ID)
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--mode",
            "yolo",
        ])
        .env("TERM", "xterm-256color")
        .env("COLORTERM", "truecolor");
    let (session, mut input, reader) = PtySession::start(&ego, ROWS, cols, Some((ROWS, cols)));
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, start);
    if cols == 120 {
        until_normalized(&reader, &mut output, start, "⎇ feat/compact●");
    }
    drain_until_quiet(&reader, &mut output);
    let mut parser = vt100::Parser::new(ROWS, cols, 0);
    parser.process(&output);
    let screen = parser.screen();
    let rows = screen.rows(0, cols).collect::<Vec<_>>();
    let status = rows
        .iter()
        .find(|row| row.contains("qwen3 · "))
        .unwrap_or_else(|| panic!("no status: {rows:?}"));
    assert!(
        status.contains("qwen3 · off · yolo ▰▰▱▱▱▱▱▱▱▱ 58k/272k"),
        "{rows:?}"
    );
    assert_eq!(
        rows.iter().filter(|row| row.contains("58k/272k")).count(),
        1,
        "{rows:?}"
    );
    assert!(status.contains("≈$0.66"), "{rows:?}");
    assert_eq!(status.contains("⎇ feat/compact●"), cols == 120, "{rows:?}");
    assert!(rows.iter().any(|row| row.starts_with('>')), "{rows:?}");
    if let Some(directory) = std::env::var_os("EGO_STATUS_CAPTURE_DIR") {
        let directory = std::path::PathBuf::from(directory);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join(format!("status-{cols}.html")),
            terminal_html(screen, ROWS, cols),
        )
        .unwrap();
        std::fs::write(directory.join(format!("status-{cols}.ansi")), &output).unwrap();
    }
    let start = output.len();
    input.write_all(b"/statusline\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "subscription-equivalent");
    input.write_all(b"\x1b").unwrap();
    input.flush().unwrap();
    drain_until_quiet(&reader, &mut output);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
}

/// Capture actual PTY cells, not a reconstructed status string, on a dark grid.
fn terminal_html(screen: &vt100::Screen, rows: u16, cols: u16) -> String {
    let mut html = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>Real PTY status {cols} columns</title><style>body{{background:#16181d;color:#eee}}pre{{font:16px/1.35 Menlo,monospace;white-space:pre}}</style><pre>"
    );
    for row in 0..rows {
        for column in 0..cols {
            let cell = screen.cell(row, column).unwrap();
            if cell.is_wide_continuation() {
                continue;
            }
            let text = cell.contents();
            let escaped = text
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;");
            let colour = match cell.fgcolor() {
                vt100::Color::Rgb(r, g, b) => format!("rgb({r},{g},{b})"),
                vt100::Color::Idx(2 | 10) => "#50c850".into(),
                vt100::Color::Idx(3 | 11) => "#e5c050".into(),
                vt100::Color::Idx(1 | 9) => "#ff6b80".into(),
                _ => "inherit".into(),
            };
            html.push_str(&format!(
                "<span style=\"color:{colour}\">{}</span>",
                if escaped.is_empty() { " " } else { &escaped }
            ));
        }
        html.push('\n');
    }
    html.push_str("</pre>");
    html
}
