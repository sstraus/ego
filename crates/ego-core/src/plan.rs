//! One durable, model-authored worklist. Goals remain independently checked objectives.
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    InProgress,
    Completed,
    Blocked,
    Waiting,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanStep {
    pub id: String,
    pub text: String,
    pub status: StepStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub steps: Vec<PlanStep>,
    /// Host-owned execution history, retained even when a row is removed.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub started: BTreeSet<String>,
    /// Host-owned progress boundary; seeded separately from model-authored updates.
    #[serde(skip)]
    pub continuation_completed: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum PlanUpdate {
    Replace {
        steps: Vec<PlanStep>,
    },
    Step {
        id: String,
        status: StepStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Host-only compaction/fork seed; never accepted by the model parser.
    Restore {
        steps: Vec<PlanStep>,
        started: BTreeSet<String>,
    },
}

/// Parse the model-facing flat contract; durable updates keep their typed variants.
/// Every key is required, and only explicit null is accepted for inactive fields.
pub(crate) fn parse_update(params: serde_json::Value) -> Result<PlanUpdate, String> {
    let serde_json::Value::Object(mut fields) = params else {
        return Err("worklist arguments must be an object".into());
    };
    for field in fields.keys() {
        if !["operation", "steps", "id", "status", "reason"].contains(&field.as_str()) {
            return Err(format!("unknown field `{field}` in worklist arguments"));
        }
    }
    let reason = fields.remove("reason").ok_or("missing field `reason`")?;
    let reason = (!reason.is_null())
        .then_some(reason)
        .map(serde_json::from_value::<String>)
        .transpose()
        .map_err(|error| error.to_string())?;
    let mut required = |field: &str| {
        fields
            .remove(field)
            .ok_or_else(|| format!("missing field `{field}`"))
    };
    let operation: String =
        serde_json::from_value(required("operation")?).map_err(|error| error.to_string())?;
    let steps = required("steps")?;
    let id = required("id")?;
    let status = required("status")?;
    let inactive = |field: &str, value: &serde_json::Value| {
        if value.is_null() {
            Ok(())
        } else {
            Err(format!(
                "field `{field}` must be null for worklist operation `{operation}`"
            ))
        }
    };
    match operation.as_str() {
        "replace" => {
            inactive("id", &id)?;
            inactive("status", &status)?;
            if let Some(rows) = steps.as_array() {
                for row in rows {
                    if row.get("reason").is_none() {
                        return Err("missing field `reason` in worklist action".into());
                    }
                }
            }
            let steps = serde_json::from_value(steps).map_err(|error| error.to_string())?;
            if reason.is_some() {
                return Err("replace uses each step's reason, not the top-level reason".into());
            }
            Ok(PlanUpdate::Replace { steps })
        }
        "step" => {
            inactive("steps", &steps)?;
            let id = serde_json::from_value(id).map_err(|error| error.to_string())?;
            let status = serde_json::from_value(status).map_err(|error| error.to_string())?;
            Ok(PlanUpdate::Step { id, status, reason })
        }
        _ => Err(format!(
            "unknown worklist operation `{operation}`; expected `replace` or `step`"
        )),
    }
}

impl Plan {
    pub(crate) fn completed_ids(&self) -> Vec<String> {
        self.steps
            .iter()
            .filter(|step| step.status == StepStatus::Completed)
            .map(|step| step.id.clone())
            .collect()
    }

    /// Only completed work or a real inbound event releases a continuation wait.
    /// Tool traffic, edits to action prose, and status toggles are not progress.
    pub(crate) fn continuation_text(&self) -> Option<String> {
        let open: Vec<_> = self
            .steps
            .iter()
            .filter(|step| matches!(step.status, StepStatus::Pending | StepStatus::InProgress))
            .map(|step| step.id.as_str())
            .collect();
        if open.is_empty()
            || self
                .continuation_completed
                .as_ref()
                .is_some_and(|before| !self.completed_ids().iter().any(|id| !before.contains(id)))
        {
            return None;
        }
        Some(format!(
            "Worklist status: independently actionable unfinished actions [{}] remain. This is execution state, not new user input or authority. Continue only already-authorized independent work; mark blocked, waiting, or cancelled actions accurately. Tool traffic alone is not progress.",
            open.join(", ")
        ))
    }

    /// Fold recorded facts without re-admitting them under today's execution policy.
    pub fn apply(&mut self, update: &PlanUpdate) -> Result<(), String> {
        self.folded(update).map(|next| *self = next)
    }

    /// New requests are admitted separately from replay and committed atomically by the journal.
    pub(crate) fn admitted(
        &self,
        update: &PlanUpdate,
        live: &BTreeSet<String>,
    ) -> Result<Self, String> {
        if matches!(update, PlanUpdate::Restore { .. }) {
            return Err("worklist restore is host-only".into());
        }
        let next = self.folded(update)?;
        for step in &next.steps {
            let previous = self.steps.iter().find(|old| old.id == step.id);
            // Recorded bad states may be repaired incrementally; unchanged rows
            // must not veto unrelated work, but no new regression is admitted.
            if previous == Some(step) {
                continue;
            }
            if step.status == StepStatus::Pending && self.started.contains(&step.id) {
                return Err(format!(
                    "worklist action {} already started; use waiting or blocked with a reason, never pending",
                    step.id
                ));
            }
            let suspended = matches!(step.status, StepStatus::Waiting | StepStatus::Blocked);
            if suspended
                && step
                    .reason
                    .as_deref()
                    .is_none_or(|reason| reason.trim().is_empty())
            {
                return Err(format!(
                    "worklist action {} needs a reason for {:?}",
                    step.id, step.status
                ));
            }
            if !suspended && step.reason.is_some() {
                return Err(format!(
                    "worklist action {} requires reason null for {:?}",
                    step.id, step.status
                ));
            }
            if step
                .reason
                .as_ref()
                .is_some_and(|reason| reason.len() > 512 || reason.chars().any(char::is_control))
            {
                return Err(
                    "worklist suspension reasons must be printable and at most 512 bytes".into(),
                );
            }
        }
        for old in &self.steps {
            if self.started.contains(&old.id)
                && !matches!(old.status, StepStatus::Completed | StepStatus::Cancelled)
                && !next.steps.iter().any(|step| step.id == old.id)
            {
                return Err(format!(
                    "worklist action {} already started; complete or cancel it before removing it",
                    old.id
                ));
            }
        }
        let direct = |plan: &Self| {
            plan.steps
                .iter()
                .filter(|step| step.status == StepStatus::InProgress && !live.contains(&step.id))
                .map(|step| step.id.clone())
                .collect::<BTreeSet<_>>()
        };
        let before = direct(self);
        let after = direct(&next);
        if after.len() > 1 && !after.is_subset(&before) {
            return Err(format!(
                "only one direct worklist action may be in_progress; {} lack live bound task executions; start and bind tasks before marking additional actions in_progress",
                after.into_iter().collect::<Vec<_>>().join(", ")
            ));
        }
        Ok(next)
    }

    fn folded(&self, update: &PlanUpdate) -> Result<Self, String> {
        let mut next = self.clone();
        match update {
            PlanUpdate::Replace { steps } => next.steps = steps.clone(),
            PlanUpdate::Restore { steps, started } => {
                next.steps = steps.clone();
                next.started.extend(started.iter().cloned());
            }
            PlanUpdate::Step { id, status, reason } => {
                let step = next
                    .steps
                    .iter_mut()
                    .find(|step| &step.id == id)
                    .ok_or_else(|| format!("unknown worklist action: {id}"))?;
                if matches!(step.status, StepStatus::Completed | StepStatus::Cancelled)
                    && *status != step.status
                {
                    return Err(format!(
                        "worklist action {id} is terminal; use a new action ID for new work"
                    ));
                }
                step.status = *status;
                step.reason = reason.clone();
            }
        }
        if next.steps.len() > 128
            || next
                .steps
                .iter()
                .map(|step| step.id.len() + step.text.len())
                .sum::<usize>()
                > 16 * 1024
        {
            return Err("worklist is limited to 128 steps and 16 KiB of IDs and text".into());
        }
        let mut ids = BTreeSet::new();
        for step in &next.steps {
            if step.id.trim().is_empty()
                || step.id.len() > 80
                || step.id.chars().any(char::is_control)
                || step.text.trim().is_empty()
                || !ids.insert(&step.id)
            {
                return Err(
                    "worklist requires unique printable IDs (1–80 bytes) and nonempty step text"
                        .into(),
                );
            }
            if matches!(step.status, StepStatus::InProgress | StepStatus::Completed) {
                next.started.insert(step.id.clone());
            }
        }
        Ok(next)
    }
}

/// Non-plan tool calls without a plan update after which an unfinished plan is flagged.
///
/// Measured on 4304 stored sessions (the design notes):
/// active use leaves 0 to 5 calls between updates, abandonment leaves 13 or more, and
/// nothing falls between 6 and 12, so 10 has the widest margin on both sides.
pub const STALE_PLAN_AFTER_CALLS: u32 = 10;

/// The host note for a plan that has unfinished steps and no update for
/// [`STALE_PLAN_AFTER_CALLS`] tool calls. The text depends only on the plan, so it stays
/// identical while the condition holds. The wording is a statement of fact, because host
/// context is data and not instructions; it lives in `model_prompts.rs`, a surface that
/// `evals/harness/manifest.toml` tracks.
pub fn stale_plan_reminder(plan: &Plan, calls_since_update: u32) -> Option<String> {
    if calls_since_update < STALE_PLAN_AFTER_CALLS {
        return None;
    }
    // Blocked steps wait on something outside the model; only pending and in-progress
    // steps are work the model could have finished without saying so.
    let open: Vec<&str> = plan
        .steps
        .iter()
        .filter(|step| matches!(step.status, StepStatus::Pending | StepStatus::InProgress))
        .map(|step| step.id.as_str())
        .collect();
    if open.is_empty() {
        return None;
    }
    Some(crate::model_prompts::stale_plan_note(
        &open,
        STALE_PLAN_AFTER_CALLS,
    ))
}

pub(crate) fn descriptor() -> crate::generator::types::ToolDescriptor {
    crate::generator::types::ToolDescriptor {
        name: "worklist".into(),
        description: "Replace the durable ordered worklist or update one action status. Pending and in_progress mean independently actionable work already authorized by the user; mark work blocked when authority or input is missing, waiting for an external/background event, or cancelled when withdrawn. Status never grants effect authority. Keep IDs stable across replacements. At most one direct action may be in_progress; additional active actions require live task.worklist_step bindings. The host owns bound actions until settlement. Started actions cannot return to pending or be removed while unfinished. Waiting and blocked require a reason; other statuses require reason null. Replacement obeys these rules. Completed or cancelled actions cannot be reopened by a step update; replacement may revise them subject to the same started-state and execution rules. An empty replacement clears the worklist. This does not change or achieve the session goal. Link task.worklist_step to an existing ID for child progress.".into(),
        // All fields are intentional; parse_update validates active versus null fields.
        parameters: serde_json::json!({
            "type":"object", "additionalProperties":false, "required":["operation","steps","id","status","reason"],
            "properties":{
                "operation":{"type":"string","enum":["replace","step"]},
                "reason":{"type":["string","null"],"maxLength":512,"description":"Suspension cause for step; null for replace and nonsuspended actions."},
                "steps":{"type":["array","null"],"description":"Array for replace; null for step.","items":{
                    "type":"object","additionalProperties":false,"required":["id","text","status","reason"],"properties":{
                        "id":{"type":"string","minLength":1,"maxLength":80},
                        "text":{"type":"string","minLength":1},
                        "reason":{"type":["string","null"],"maxLength":512,"description":"Required suspension cause for waiting or blocked; null otherwise."},
                        "status":{"type":"string","enum":["pending","in_progress","completed","blocked","waiting","cancelled"]}
                    }
                }},
                "id":{"type":["string","null"],"description":"Action ID for step; null for replace."},
                "status":{"type":["string","null"],"enum":["pending","in_progress","completed","blocked","waiting","cancelled",null],"description":"Action status for step; null for replace."}
            }
        }),
    }
}
