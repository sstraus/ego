//! Read-only end-of-turn visibility for work owned by this Ego session.

use super::*;

pub(super) fn report(
    output: Output,
    human: bool,
    width: usize,
    state: &SessionState,
    process: &dyn ego_core::permit::ProcessExecutor,
    owner: &CliSkillInvocationOwner<'_>,
    diagnostics: &dyn Diagnostics,
) {
    if output != Output::Text || !human {
        return;
    }
    let secrets = ego_tools::secrets::from_env();
    let mut labels = Vec::new();
    {
        let tasks = owner.tasks.lock().expect("background tasks");
        for (id, item) in port_adapters::background_agent_items(state) {
            // Controller settlement can precede collection into the parent journal.
            if !tasks
                .get(&id)
                .is_some_and(|task| matches!(task.service.is_settled(id), Ok(false)))
            {
                continue;
            }
            let args: Value = serde_json::from_str(&item.args).unwrap_or_default();
            let name = args.get("name").and_then(Value::as_str).unwrap_or("task");
            labels.push(name.to_owned());
        }
    }
    for (id, settled) in &state.detached_execs {
        if settled.is_none()
            && process
                .background_status(*id)
                .is_ok_and(|snapshot| snapshot.outcome.is_none())
        {
            // Arguments and output can contain secrets; identify commands without them.
            labels.push(format!(
                "exec …{}",
                id.to_string().rsplit('-').next().unwrap_or_default()
            ));
        }
    }
    if let Some(line) = compact(&labels, width, &secrets) {
        diagnostics.notice_block(&line);
    }
}

pub(super) fn compact(
    labels: &[String],
    width: usize,
    secrets: &ego_core::secrets::SecretValues,
) -> Option<String> {
    if labels.is_empty() {
        return None;
    }
    let prefix_width = crate::render::Line::plain(format!("{}: ", crate::DISPLAY_NAME)).width();
    let width = width.min(100).saturating_sub(prefix_width + 1);
    let count = labels.len();
    let prefix = format!("↻ Background: {count} active");
    let labels: Vec<_> = labels
        .iter()
        .take(2)
        .map(|label| {
            let safe = ego_core::permit::terminal_safe(&secrets.mask(label))
                .replace(['\n', '\r', '\t'], " ");
            crate::render::Line::plain(safe)
                .truncate_to(24)
                .text()
                .to_owned()
        })
        .collect();
    for shown in (0..=labels.len()).rev() {
        let mut line = prefix.clone();
        for label in labels.iter().take(shown) {
            line.push_str(" · ");
            line.push_str(label);
        }
        // The count includes labels beyond the two-name display budget.
        if count > shown {
            line.push_str(&format!(" · +{}", count - shown));
        }
        if crate::render::Line::plain(&line).width() <= width || shown == 0 {
            return Some(
                crate::render::Line::plain(line)
                    .truncate_to(width)
                    .text()
                    .to_owned(),
            );
        }
    }
    None
}
