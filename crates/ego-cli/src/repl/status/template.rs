//! Value-only MiniJinja rendering. No loader or effectful functions are installed.
use super::*;
use minijinja::{Environment, Error, ErrorKind, UndefinedBehavior};

/// The compact renderer owns the 524 symbols, separators and width policy.
pub(crate) const DEFAULT_TEMPLATE: &str = "{{ compact }}\n{{ prompt }}";
const PROMPT: &str = "\u{e000}";
const START: char = '\u{e001}';
const END: char = '\u{e002}';
const RESET: char = '\u{e003}';
const GREEN: char = '\u{e004}';
const YELLOW: char = '\u{e005}';
const RED: char = '\u{e006}';

#[derive(Debug, Clone)]
pub(crate) struct StatusTemplate {
    source: String,
}

impl StatusTemplate {
    pub(crate) fn new(source: String) -> Self {
        Self { source }
    }

    pub(crate) fn is_compact(&self) -> bool {
        self.source == DEFAULT_TEMPLATE
    }

    /// Errors return the unchanged built-in layout. The seat deduplicates notices.
    pub(crate) fn render(
        &self,
        snapshot: &SurfaceSnapshot,
        values: &[(StatusSegmentName, SegmentValue)],
        width: usize,
    ) -> (StatusLine, Option<String>) {
        let configured = crate::repl::seat::DEFAULT_STATUSLINE_SEGMENTS
            .iter()
            .copied()
            .map(StatusSegmentName::from)
            .chain(values.iter().map(|(name, _)| name.clone()))
            .collect::<Vec<_>>();
        let compact = StatusLine::render_with_values(snapshot, &configured, values, width);
        match self.try_render(snapshot, &compact, values, width) {
            Ok(line) => (line, None),
            Err(error) => (
                Self::new(DEFAULT_TEMPLATE.into())
                    .try_render(snapshot, &compact, values, width)
                    .unwrap_or(compact),
                Some(format!(
                    "statusline: {} (line {})",
                    error.detail().unwrap_or("invalid template"),
                    error.line().unwrap_or_else(|| {
                        self.source
                            .lines()
                            .position(|line| line.contains("prompt"))
                            .map_or(1, |line| line + 1)
                    })
                )),
            ),
        }
    }

    fn try_render(
        &self,
        snapshot: &SurfaceSnapshot,
        compact: &StatusLine,
        values: &[(StatusSegmentName, SegmentValue)],
        width: usize,
    ) -> Result<StatusLine, Error> {
        if self.source.len() > 65536 {
            return Err(invalid("template exceeds 65536 bytes"));
        }
        if self
            .source
            .chars()
            .any(|ch| reserved(ch) || (ch.is_control() && ch != '\n'))
        {
            return Err(invalid("reserved template character"));
        }
        let env = environment();
        let template = env.template_from_str(&self.source)?;
        let mut compact_text = clean(compact.text());
        let context = snapshot.context_display();
        let percent = match (snapshot.used_tokens, snapshot.window) {
            (Some(used), Some(window)) if window > 0 => Some(used as f64 * 100.0 / window as f64),
            _ => None,
        };
        if compact_text.contains(&context)
            && let Some(percent) = percent
        {
            compact_text = compact_text.replace(&context, &colored(&context, threshold(percent)));
        }
        let providers = values
            .iter()
            .map(|(name, value)| (name.label(), clean(&value.0)))
            .collect::<std::collections::BTreeMap<_, _>>();
        let cost_amount = snapshot
            .cost_amount
            .as_ref()
            .and_then(|amount| amount.parse::<f64>().ok());
        let context = minijinja::context! {
            prompt => PROMPT,
            compact => compact_text,
            model => clean(&snapshot.model), effort => &snapshot.effort,
            fast => snapshot.fast(), mode => &snapshot.mode,
            ctx => minijinja::context! { used => snapshot.used_tokens, window => snapshot.window, percent => percent },
            cost => minijinja::context! { amount => cost_amount, equivalent => snapshot.cost_equivalent },
            cwd => clean(&snapshot.raw_cwd),
            git => minijinja::context! { branch => snapshot.git.as_ref().map(|git| clean(&git.branch)), dirty => snapshot.git.as_ref().is_some_and(|git| git.dirty) },
            queue => snapshot.queue.pending_steers + snapshot.queue.queued,
            drafts => snapshot.drafts, interrupt => snapshot.queue.interrupt_pending,
            trust => if snapshot.trust == Trust::Trusted { "trusted" } else { "untrusted" },
            goal => minijinja::context! { active => snapshot.goal.is_some(), objective => snapshot.goal_objective.as_deref().map(clean), turns_used => snapshot.goal.map(|goal| goal.0), turns_max => snapshot.goal.and_then(|goal| goal.1) },
            providers => providers,
        };
        let mut output = BoundedOutput(Vec::new());
        template.render_to_write(context, &mut output)?;
        let rendered = String::from_utf8(output.0).expect("MiniJinja writes UTF-8");
        if rendered.matches(PROMPT).count() != 1 {
            return Err(invalid("prompt must appear once on its own line"));
        }
        // A prompt cannot be dropped with an optional priority block.
        let mut blocks = parse_blocks(&rendered)?;
        let plain = blocks
            .iter()
            .map(|block| block.text.as_str())
            .collect::<String>();
        if !plain.lines().any(|line| line == PROMPT)
            || blocks
                .iter()
                .any(|block| block.priority.is_some() && block.text.contains(PROMPT))
        {
            return Err(invalid("prompt must appear once on its own line"));
        }
        // Operational facts get their own rows, outside optional blocks.
        let mut priorities = blocks
            .iter()
            .filter_map(|block| block.priority)
            .collect::<Vec<_>>();
        priorities.sort_unstable();
        priorities.dedup();
        for priority in priorities {
            let text = blocks
                .iter()
                .map(|block| block.text.as_str())
                .collect::<String>();
            if fits(&text, width) {
                break;
            }
            for block in &mut blocks {
                if block.priority == Some(priority) {
                    block.text.clear();
                }
            }
        }
        let text = blocks
            .iter()
            .map(|block| block.text.as_str())
            .collect::<String>();
        let (above, below) = text.split_once(PROMPT).expect("validated prompt marker");
        let mut above = above.strip_suffix('\n').unwrap_or(above).to_owned();
        // An unprioritised row is itself an indivisible block.
        above = above
            .split('\n')
            .filter(|row| visible_width(row) <= width)
            .collect::<Vec<_>>()
            .join("\n");
        let below = below
            .strip_prefix('\n')
            .unwrap_or(below)
            .split('\n')
            .filter(|row| visible_width(row) <= width)
            .collect::<Vec<_>>()
            .join("\n");
        let critical = snapshot
            .critical_values()
            .into_iter()
            .filter(|value| !above.contains(value) && !below.contains(value))
            .collect::<Vec<_>>();
        for value in critical {
            if !above.is_empty() {
                above.push('\n');
            }
            above.push_str(&value);
        }
        let rows = row_lines(&above);
        Ok(StatusLine {
            text: rows.iter().map(Line::text).collect::<Vec<_>>().join("\n"),
            width: above.lines().map(visible_width).max().unwrap_or(0),
            styled: Line::empty(),
            footer: row_lines(&below),
            template_rows: Some(rows),
            omitted: Vec::new(),
            diagnostics: compact.diagnostics.clone(),
            snapshot_revision: snapshot.revision.clone(),
        })
    }
}

/// Bound final output independently of loop fuel and source size.
struct BoundedOutput(Vec<u8>);
impl std::io::Write for BoundedOutput {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) > 65536 {
            return Err(std::io::Error::other("template output exceeds 65536 bytes"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn invalid(message: &str) -> Error {
    Error::new(ErrorKind::InvalidOperation, message.to_owned())
}

fn reserved(ch: char) -> bool {
    ('\u{e000}'..='\u{e006}').contains(&ch)
}
fn clean(text: &str) -> String {
    text.chars()
        .filter(|ch| !ch.is_control() && !reserved(*ch))
        .collect()
}
fn threshold(percent: f64) -> char {
    if percent >= 85.0 {
        RED
    } else if percent >= 60.0 {
        YELLOW
    } else {
        GREEN
    }
}
fn colored(text: &str, color: char) -> String {
    format!("{color}{text}{RESET}")
}

fn environment() -> Environment<'static> {
    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::Strict);
    env.set_fuel(Some(10000));
    env.set_recursion_limit(32);
    env.add_filter("tokens", |value: Option<u64>| {
        value.map(compact_tokens).unwrap_or_else(|| "?".into())
    });
    env.add_filter("money", |value: Option<f64>| {
        value
            .map(|amount| {
                if amount > 0.0 && amount < 0.0005 {
                    "<$0.001".into()
                } else if amount < 0.1 {
                    format!("${amount:.3}")
                } else {
                    format!("${amount:.2}")
                }
            })
            .unwrap_or_else(|| "?".into())
    });
    env.add_filter("short_path", |value: String| compact_cwd(&value));
    env.add_filter(
        "bar",
        |percent: Option<f64>, n: usize| -> Result<String, Error> {
            if n > 256 {
                return Err(invalid("bar width exceeds 256"));
            }
            let filled = ((percent.unwrap_or(0.0).clamp(0.0, 100.0) * n as f64 / 100.0).floor()
                as usize)
                .min(n);
            Ok(format!("{}{}", "▰".repeat(filled), "▱".repeat(n - filled)))
        },
    );
    env.add_filter(
        "color",
        |value: String, name: String| -> Result<String, Error> {
            let color = match name.as_str() {
                "green" => GREEN,
                "yellow" => YELLOW,
                "red" => RED,
                _ => return Err(invalid("unknown color")),
            };
            Ok(colored(&value, color))
        },
    );
    env.add_filter("threshold", |value: String, percent: Option<f64>| {
        percent.map_or_else(
            || value.clone(),
            |percent| colored(&value, threshold(percent)),
        )
    });
    env.add_filter(
        "priority",
        |value: String, priority: u32| -> Result<String, Error> {
            if value.contains(START) {
                return Err(invalid("nested priority blocks"));
            }
            Ok(format!("{START}{priority}:{value}{END}"))
        },
    );
    env
}

struct Block {
    priority: Option<u32>,
    text: String,
}
fn parse_blocks(mut text: &str) -> Result<Vec<Block>, Error> {
    let mut blocks = Vec::new();
    while let Some(start) = text.find(START) {
        blocks.push(Block {
            priority: None,
            text: text[..start].to_owned(),
        });
        let rest = &text[start + START.len_utf8()..];
        let (priority, rest) = rest
            .split_once(':')
            .ok_or_else(|| invalid("invalid priority block"))?;
        let (body, rest) = rest
            .split_once(END)
            .ok_or_else(|| invalid("invalid priority block"))?;
        blocks.push(Block {
            priority: Some(
                priority
                    .parse()
                    .map_err(|_| invalid("invalid priority block"))?,
            ),
            text: body.to_owned(),
        });
        text = rest;
    }
    blocks.push(Block {
        priority: None,
        text: text.to_owned(),
    });
    Ok(blocks)
}
fn visible_width(text: &str) -> usize {
    UnicodeWidthStr::width(
        text.chars()
            .filter(|ch| !reserved(*ch))
            .collect::<String>()
            .as_str(),
    )
}
fn fits(text: &str, width: usize) -> bool {
    text.lines().all(|line| visible_width(line) <= width)
}
fn row_lines(text: &str) -> Vec<Line> {
    if text.is_empty() {
        Vec::new()
    } else {
        text.split('\n').map(styled_line).collect()
    }
}
fn styled_line(text: &str) -> Line {
    let mut result = Line::empty();
    let mut style = Style::PLAIN;
    let mut run = String::new();
    for ch in text.chars() {
        if [RESET, GREEN, YELLOW, RED].contains(&ch) {
            result.push(Span::new(std::mem::take(&mut run), style));
            style = match ch {
                GREEN => Style::fg(Palette::ADDED_FG),
                YELLOW => Style::fg(Palette::WARNING),
                RED => Style::fg(Palette::ERROR),
                _ => Style::PLAIN,
            };
        } else {
            run.push(ch);
        }
    }
    result.push(Span::new(run, style));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> SurfaceSnapshot {
        let mut state = SessionState::default();
        state.context.used_tokens = Some(58_000);
        state.context.window = Some(272_000);
        SurfaceSnapshot::from_state_with_git(
            SessionId::new(),
            &state,
            SelectionFacts::new(
                ModelRef("openai/gpt-6-sol".into()),
                ThinkingLevel::Medium,
                Mode::Yolo,
            ),
            SeatFacts::new(QueueStatus::default(), 0, "/workspace/ego", Trust::Trusted),
            Some(GitFacts::new("feat/template", true)),
        )
    }

    // Catches a template migration changing 524 symbols or whole-value width policy.
    #[test]
    fn template_default_matches_compact_at_all_widths() {
        let snapshot = snapshot();
        let configured = crate::repl::seat::DEFAULT_STATUSLINE_SEGMENTS
            .iter()
            .copied()
            .map(StatusSegmentName::from)
            .collect::<Vec<_>>();
        for width in [1, 20, 40, 60, 120] {
            let expected = StatusLine::render(&snapshot, &configured, width);
            let (actual, error) =
                StatusTemplate::new(DEFAULT_TEMPLATE.into()).render(&snapshot, &[], width);
            assert_eq!(error, None);
            assert_eq!(actual.text(), expected.text(), "width={width}");
            assert!(actual.footer.is_empty());
            assert_eq!(
                actual
                    .template_rows
                    .as_ref()
                    .unwrap()
                    .iter()
                    .flat_map(|line| &line.spans)
                    .filter(|span| span.style != Style::PLAIN)
                    .collect::<Vec<_>>(),
                expected
                    .styled
                    .spans
                    .iter()
                    .filter(|span| span.style != Style::PLAIN)
                    .collect::<Vec<_>>()
            );
        }
    }

    // Catches the renderer treating prompt as ordinary text or losing below-buffer rows.
    #[test]
    fn template_variables_filters_and_prompt_position_are_observable() {
        let template = "{{ model }} {{ effort }} {{ mode }}\n{{ ctx.percent|bar(4) }} {{ ctx.used|tokens }}/{{ ctx.window|tokens }}\n{{ prompt }}\n{{ git.branch }} {{ git.dirty }} {{ cwd|short_path }} {{ trust }}";
        let (line, error) = StatusTemplate::new(template.into()).render(&snapshot(), &[], 120);
        assert_eq!(error, None);
        assert_eq!(line.text(), "openai/gpt-6-sol medium yolo\n▱▱▱▱ 58k/272k");
        assert_eq!(
            line.footer.iter().map(Line::text).collect::<Vec<_>>(),
            ["feat/template true /w/ego trusted"]
        );
    }

    // Catches an absent counter or a dropped priority block concealing operational state.
    #[test]
    fn template_critical_state_survives_omission_and_priority_drops() {
        let mut snapshot = snapshot();
        snapshot.set_queue(QueueStatus {
            pending_steers: 1,
            queued: 2,
            interrupt_pending: true,
        });
        snapshot.set_drafts(3);
        for template in [
            "{{ prompt }}",
            "{% filter priority(0) %}queue=1/2 drafts=3 interrupt=yes too long{% endfilter %}\n{{ prompt }}",
        ] {
            let (line, error) = StatusTemplate::new(template.into()).render(&snapshot, &[], 20);
            assert_eq!(error, None);
            for value in ["queue=1/2", "drafts=3", "interrupt=yes"] {
                assert!(line.text().contains(value), "{}", line.text());
            }
        }
    }

    // Catches priorities following source order, or truncating retained semantic values.
    #[test]
    fn template_drops_whole_blocks_by_priority() {
        let template = "{% filter priority(20) %}MODEL{% endfilter %}{% filter priority(10) %} BRANCH{% endfilter %}\n{{ prompt }}";
        for (width, expected) in [(12, "MODEL BRANCH"), (5, "MODEL"), (2, "")] {
            let (line, error) =
                StatusTemplate::new(template.into()).render(&snapshot(), &[], width);
            assert_eq!(error, None);
            assert_eq!(line.text(), expected);
        }
    }

    // Catches malformed source or a hidden/repeated/inline prompt disabling the composer.
    #[test]
    fn template_errors_keep_default_and_name_the_source_line() {
        let snapshot = snapshot();
        let (expected, _) =
            StatusTemplate::new(DEFAULT_TEMPLATE.into()).render(&snapshot, &[], 120);
        for template in [
            "{{ model }}",
            "{{ prompt }} {{ prompt }}",
            "inline {{ prompt }}",
            "{% filter priority(0) %}{{ prompt }}{% endfilter %}",
        ] {
            let (line, error) = StatusTemplate::new(template.into()).render(&snapshot, &[], 120);
            assert_eq!(line.text(), expected.text());
            assert!(
                error
                    .unwrap()
                    .contains("prompt must appear once on its own line")
            );
        }
        let (_, error) =
            StatusTemplate::new("first\n{{ ) }}\n{{ prompt }}".into()).render(&snapshot, &[], 120);
        assert!(error.unwrap().ends_with("(line 2)"));
    }

    // Catches stale goal objectives after a live goal replacement or stop.
    #[test]
    fn template_goal_variables_follow_live_projection() {
        let mut snapshot = snapshot();
        snapshot.set_goal(Some(("Ship template".into(), 2, Some(5))));
        let template = StatusTemplate::new("{{ goal.active }} {{ goal.objective }} {{ goal.turns_used }}/{{ goal.turns_max }}\n{{ prompt }}".into());
        assert_eq!(
            template.render(&snapshot, &[], 120).0.text(),
            "true Ship template 2/5"
        );
        snapshot.set_goal(None);
        let (line, error) = StatusTemplate::new("{{ goal.active }}\n{{ prompt }}".into()).render(
            &snapshot,
            &[],
            120,
        );
        assert_eq!(error, None);
        assert_eq!(line.text(), "false");
    }

    // Catches filters leaking ANSI controls into layout, or unbounded user loops.
    #[test]
    fn template_colors_preserve_plain_text_and_untrusted_input_cannot_inject_markers() {
        let (line, error) = StatusTemplate::new(
            "{{ 'meter'|threshold(85) }} {{ 0.661|money }}\n{{ prompt }}".into(),
        )
        .render(&snapshot(), &[], 120);
        assert_eq!(error, None);
        assert_eq!(line.text(), "meter $0.66");
        assert_eq!(
            line.template_rows.unwrap()[0].spans[0].style,
            Style::fg(Palette::ERROR)
        );
        assert_eq!(clean("evil\x1b[2J\u{e000}"), "evil[2J");
        let (_, error) =
            StatusTemplate::new("{% for i in range(100000) %}x{% endfor %}\n{{ prompt }}".into())
                .render(&snapshot(), &[], 120);
        assert!(error.is_some());
    }
}
