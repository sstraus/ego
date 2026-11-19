//! One durable, model-authored plan. Goals remain independently checked objectives.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    InProgress,
    Completed,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanStep {
    pub id: String,
    pub text: String,
    pub status: StepStatus,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub steps: Vec<PlanStep>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum PlanUpdate {
    Replace { steps: Vec<PlanStep> },
    Step { id: String, status: StepStatus },
}

impl Plan {
    /// Validate before publication and during replay. A rejected operation changes nothing.
    pub fn apply(&mut self, update: &PlanUpdate) -> Result<(), String> {
        match update {
            PlanUpdate::Replace { steps } => {
                if steps.len() > 128
                    || steps
                        .iter()
                        .map(|step| step.id.len() + step.text.len())
                        .sum::<usize>()
                        > 16 * 1024
                {
                    return Err("plan is limited to 128 steps and 16 KiB of IDs and text".into());
                }
                let mut ids = std::collections::BTreeSet::new();
                for step in steps {
                    if step.id.trim().is_empty()
                        || step.id.len() > 80
                        || step.id.chars().any(char::is_control)
                        || step.text.trim().is_empty()
                        || !ids.insert(&step.id)
                    {
                        return Err("plan requires unique printable IDs (1–80 bytes) and nonempty step text".into());
                    }
                }
                self.steps = steps.clone();
            }
            PlanUpdate::Step { id, status } => {
                let step = self
                    .steps
                    .iter_mut()
                    .find(|step| &step.id == id)
                    .ok_or_else(|| format!("unknown plan step: {id}"))?;
                if step.status == StepStatus::Completed && *status != StepStatus::Completed {
                    return Err(format!(
                        "plan step {id} is completed; explicitly replace the plan to reopen it"
                    ));
                }
                step.status = *status;
            }
        }
        Ok(())
    }
}

pub(crate) fn descriptor() -> crate::generator::types::ToolDescriptor {
    crate::generator::types::ToolDescriptor {
        name: "plan".into(),
        description: "Replace the durable ordered plan or update one step status. Keep IDs stable across replacements. Completed steps cannot be reopened by a step update; explicitly replace the plan to revise completed work. An empty replacement clears the plan. This does not change or achieve the session goal. Link task.plan_step to an existing ID for child progress.".into(),
        // Model-facing fields stay flat; PlanUpdate validates the selected variant.
        parameters: serde_json::json!({
            "type":"object", "additionalProperties":false, "required":["operation"],
            "properties":{
                "operation":{"type":"string","enum":["replace","step"]},
                "steps":{"type":"array","description":"Required for replace; omit for step.","items":{
                    "type":"object","additionalProperties":false,"required":["id","text","status"],"properties":{
                        "id":{"type":"string","minLength":1,"maxLength":80},
                        "text":{"type":"string","minLength":1},
                        "status":{"enum":["pending","in_progress","completed","blocked"]}
                    }
                }},
                "id":{"type":"string","description":"Required for step; omit for replace."},
                "status":{"enum":["pending","in_progress","completed","blocked"],"description":"Required for step; omit for replace."}
            }
        }),
    }
}
