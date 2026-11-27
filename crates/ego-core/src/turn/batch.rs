//! `mcp_batch`: several independent MCP lookups behind one model tool call.
//!
//! The batch exists for providers that can emit only one tool call per
//! generation. It is not a second permission path and not a scheduler: every
//! member is prepared before any effect runs, then runs serially, in the
//! order the model wrote, through the same per-call admission, hooks and
//! journal as a lone call. The first member that does not complete cleanly
//! stops the batch and the rest are answered "not run".
//!
//! Overlapping members is a separate, unimplemented step: it needs an
//! operator-owned eligibility rule that this module does not invent.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};

use super::*;
use crate::state::{ItemKind, ItemState, SessionState};

pub(crate) const TOOL_NAME: &str = "mcp_batch";
const MIN_MEMBERS: usize = 2;
const MAX_MEMBERS: usize = 8;
/// Bound on the serialised member arguments, summed over the batch.
const MAX_ARGS_BYTES: usize = 64 * 1024;
/// Bound on the combined answer the model reads back.
const MAX_ANSWER_BYTES: usize = 64 * 1024;

/// The MCP effect tools of one captured catalogue. Nothing else is a member.
#[derive(Clone)]
pub struct BatchTool {
    members: Vec<Arc<dyn crate::permit::Tool>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    calls: Vec<RequestMember>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestMember {
    tool: String,
    #[serde(default)]
    arguments: Value,
}

/// A validated batch. Holding one proves the shape and bounds, not the
/// members' own arguments: those are checked by [`TurnLoop::run_batch`]
/// before the first effect.
pub(crate) struct BatchRequest {
    members: Vec<(Arc<dyn crate::permit::Tool>, String, String)>,
}

impl BatchTool {
    /// `None` when fewer than two members exist: one tool needs no batch.
    pub(crate) fn new(members: Vec<Arc<dyn crate::permit::Tool>>) -> Option<Self> {
        (members.len() >= MIN_MEMBERS).then_some(Self { members })
    }

    /// The same batch restricted to members `keep` admits.
    pub(crate) fn restricted(&self, keep: impl Fn(&str) -> bool) -> Option<Self> {
        Self::new(
            self.members
                .iter()
                .filter(|tool| keep(&tool.descriptor().name))
                .cloned()
                .collect(),
        )
    }

    pub(crate) fn descriptor(&self) -> crate::generator::types::ToolDescriptor {
        crate::generator::types::ToolDescriptor {
            name: TOOL_NAME.to_owned(),
            description: format!(
                "Run {MIN_MEMBERS} to {MAX_MEMBERS} independent MCP tool calls in one step and read all answers together. \
                 Use it only for lookups whose arguments do not depend on each other's results. Members run one after the other in the \
                 order given; each is permission-checked on its own, and the batch stops at the first member that fails, is refused or \
                 needs input or starts an MCP task, so later members are reported as not run. Only MCP tools are accepted; call anything else directly."
            ),
            parameters: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["calls"],
                "properties": {
                    "calls": {
                        "type": "array",
                        "minItems": MIN_MEMBERS,
                        "maxItems": MAX_MEMBERS,
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "required": ["tool", "arguments"],
                            "properties": {
                                "tool": { "type": "string", "description": "Exact name of an MCP tool from the tool list." },
                                "arguments": { "type": "object", "description": "The arguments that tool takes when called directly." }
                            }
                        }
                    }
                }
            }),
        }
    }

    pub(crate) fn parse(&self, params: Value) -> Result<BatchRequest, String> {
        let example = r#"{"calls":[{"tool":"<mcp tool>","arguments":{}},{"tool":"<mcp tool>","arguments":{}}]}"#;
        let request: Request = serde_json::from_value(params).map_err(|error| {
            format!(
                "{TOOL_NAME} arguments are invalid: {error}. Nothing ran. Retrying unchanged cannot help; use the shape {example}."
            )
        })?;
        if !(MIN_MEMBERS..=MAX_MEMBERS).contains(&request.calls.len()) {
            return Err(format!(
                "{TOOL_NAME} takes {MIN_MEMBERS} to {MAX_MEMBERS} calls and got {}. Nothing ran. Send a batch inside that range, or call the tool directly.",
                request.calls.len()
            ));
        }
        let mut total = 0_usize;
        let mut members = Vec::with_capacity(request.calls.len());
        for (index, member) in request.calls.into_iter().enumerate() {
            let tool = self
                .members
                .iter()
                .find(|tool| tool.descriptor().name == member.tool)
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "{TOOL_NAME} call {index}: `{}` is not an MCP tool in this batch's catalogue (nesting, built-ins and unknown names are refused). Nothing ran. Fix the name or call that tool directly.",
                        member.tool
                    )
                })?;
            let args = serde_json::to_string(&member.arguments).expect("a JSON value serialises");
            total = total.saturating_add(args.len());
            if total > MAX_ARGS_BYTES {
                return Err(format!(
                    "{TOOL_NAME} arguments exceed {MAX_ARGS_BYTES} bytes in total. Nothing ran. Split the batch."
                ));
            }
            members.push((tool, args, member.tool));
        }
        Ok(BatchRequest { members })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Ok,
    TaskStarted,
    Error,
    NotRun,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::TaskStarted => "task_started",
            Self::Error => "error",
            Self::NotRun => "not_run",
        }
    }
}

struct Answer {
    tool: String,
    status: Status,
    output: String,
}

fn member_output(result: &ToolResult) -> String {
    let mut output = result.model_text();
    if !result.images().is_empty() {
        output.push_str("\n[image content omitted from the batch answer; the call already ran]");
    }
    output
}

fn render(answers: &[Answer]) -> String {
    let mut budget = MAX_ANSWER_BYTES;
    let results: Vec<Value> = answers
        .iter()
        .enumerate()
        .map(|(index, answer)| {
            let mut output = answer.output.as_str();
            let mut truncated = false;
            if output.len() > budget {
                let mut end = budget;
                while !output.is_char_boundary(end) {
                    end -= 1;
                }
                output = &output[..end];
                truncated = true;
            }
            budget = budget.saturating_sub(output.len());
            let mut entry = json!({
                "index": index,
                "tool": answer.tool,
                "status": answer.status.label(),
                "output": output,
            });
            if truncated {
                entry["truncated"] = json!("answer budget reached; do not repeat a completed call solely to retrieve omitted output");
            }
            entry
        })
        .collect();
    json!({ "results": results }).to_string()
}

/// The combined answer of a batch whose member suspended for input and then
/// settled, rebuilt from durable member items only. `None` unless every
/// started member is complete and one of them was suspended: a plain crash cut
/// has no such proof and stays torn. Members that never started are reported
/// as not started, never as run.
fn settled_suspension(
    state: &SessionState,
    outer: ItemId,
    item: &ItemState,
) -> Option<(bool, String)> {
    let request: Request = serde_json::from_str(&item.args).ok()?;
    let mut members: Vec<(ItemId, &ItemState)> = state
        .items
        .iter()
        .filter(|(_, member)| member.batch_member == Some(outer))
        .map(|(id, member)| (*id, member))
        .collect();
    members.sort_by_key(|(_, member)| member.seq);
    if members.is_empty() || members.len() > request.calls.len() {
        return None;
    }
    let mut suspended = None;
    let mut answers = Vec::with_capacity(request.calls.len());
    for (index, (call, (member_id, member))) in request.calls.iter().zip(&members).enumerate() {
        if !member.completed {
            return None;
        }
        if state.foreign_question_settled_for(*member_id) {
            suspended = Some(index);
        }
        let ok = member.ok == Some(true);
        answers.push(Answer {
            tool: call.tool.clone(),
            status: if ok { Status::Ok } else { Status::Error },
            output: member
                .tool_result
                .as_ref()
                .map_or_else(String::new, member_output),
        });
    }
    let suspended = suspended? + 1;
    let all_ok = answers.iter().all(|answer| answer.status == Status::Ok);
    let complete = answers.len() == request.calls.len();
    for call in &request.calls[answers.len()..] {
        answers.push(Answer {
            tool: call.tool.clone(),
            status: Status::NotRun,
            output: format!(
                "not started: member {suspended} was suspended for input and the batch stopped there; nothing was dispatched for this call. Call it directly if it is still needed."
            ),
        });
    }
    Some((all_ok && complete, render(&answers)))
}

/// The outer provider call must not receive a synthetic torn answer while
/// the original member still owns a live question.
pub(super) fn pending_outer_call_id(state: &SessionState) -> Option<&str> {
    let member = state.items.get(&state.pending_question()?.tool_call())?;
    let outer = state.items.get(&member.batch_member?)?;
    if outer.completed || outer.tool_name.as_deref() != Some(TOOL_NAME) {
        return None;
    }
    outer.call_id.as_deref()
}

impl<'s: 't, 't> TurnLoop<'s, 't> {
    /// Answer each `mcp_batch` whose member suspended for input and has since
    /// settled, once, with one combined result. Fresh input checks all turns.
    ///
    /// While the member's question is open the outer call stays unfinished and
    /// owned (`SessionState::torn_tool_errors` skips it), so recovery never
    /// answers it torn. Nothing here dispatches: the member settled through
    /// its own durable continuation, and later members are reported as not
    /// started because the registry and policy that admitted the batch are not
    /// recaptured on a reopened session.
    pub(super) fn settle_suspended_batches(
        &mut self,
        turn: Option<TurnId>,
    ) -> Result<(), StoreError> {
        let settled: Vec<_> = {
            let state = self.state();
            state
                .items
                .iter()
                .filter(|(_, item)| {
                    turn.is_none_or(|turn| item.turn == turn)
                        && item.kind == ItemKind::ToolCall
                        && !item.completed
                        && item.batch_member.is_none()
                        && item.tool_name.as_deref() == Some(TOOL_NAME)
                })
                .filter_map(|(id, item)| {
                    let started = item.tool_call_started?;
                    let (ok, text) = settled_suspension(&state, *id, item)?;
                    Some((item.turn, *id, started, ok, text))
                })
                .collect()
        };
        for (turn, item, started, ok, text) in settled {
            self.journal.append(
                turn,
                Some(item),
                Some(started),
                EventKind::ToolCallCompleted {
                    ok,
                    result: self.tools.masked_result(ToolResult::builtin(text)),
                },
            )?;
        }
        Ok(())
    }

    /// Run one validated batch. The outer call is answered with one combined
    /// result in member order; every member keeps its own journal item.
    pub(super) async fn run_batch(
        &mut self,
        policy: &PolicyEngine,
        policy_snapshot: &Arc<GenerationPolicySnapshot>,
        turn: TurnId,
        outer: &PendingCall,
        request: BatchRequest,
    ) -> Result<ToolOutcome, StoreError> {
        let refuse = |text: String| ToolOutcome::Answered {
            ok: false,
            result: ToolResult::builtin(text),
        };
        // Every member is prepared before the first effect, so a bad member
        // can never leave its predecessors run and the model unanswered.
        let mut prepared = Vec::with_capacity(request.members.len());
        for (index, (tool, args, name)) in request.members.iter().enumerate() {
            let outcome = self
                .tools
                .prepare_selected(RegisteredTool::Effect(Arc::clone(tool)), args);
            match outcome {
                Ok(PreparedToolCall::Effect {
                    tool,
                    call,
                    presentation,
                }) if call.mcp_envelope().is_some() => prepared.push((tool, *call, presentation)),
                Ok(_) => {
                    return Ok(refuse(format!(
                        "{TOOL_NAME} call {index}: `{name}` is not an MCP effect. Nothing ran. Call it directly."
                    )));
                }
                Err(error) => {
                    return Ok(refuse(format!(
                        "{TOOL_NAME} call {index} (`{name}`): {error} Nothing ran. Correct that member's arguments and resend the batch."
                    )));
                }
            }
        }

        let mut answers: Vec<Answer> = request
            .members
            .iter()
            .map(|(_, _, name)| Answer {
                tool: name.clone(),
                status: Status::NotRun,
                output: "not run: the batch stopped at an earlier member".to_owned(),
            })
            .collect();
        let mut stopped = false;
        for (index, (effect, (_, args, name))) in
            prepared.into_iter().zip(request.members.iter()).enumerate()
        {
            if let Some(reason) = self.must_stop() {
                return Ok(ToolOutcome::Ended(reason));
            }
            let item = ItemId::new();
            let started = self.journal.append(
                turn,
                Some(item),
                Some(outer.started),
                EventKind::BatchMemberStarted {
                    batch: outer.item,
                    index: u8::try_from(index).expect("a batch has at most eight members"),
                    name: name.clone(),
                    args: args.clone(),
                },
            )?;
            let mut member = PendingCall {
                timing: Default::default(),
                user_effect: None,
                user_shell: false,
                item,
                started,
                selected: None,
                args: args.clone(),
                _catalogue_generation: outer._catalogue_generation.clone(),
                capture_authority: outer.capture_authority.clone(),
            };
            let dispatch_started = Instant::now();
            let outcome = self
                .run_effect_tool(policy, policy_snapshot, turn, &mut member, effect)
                .await?;
            if member.timing.execution_ms.is_none() {
                let elapsed =
                    u64::try_from(dispatch_started.elapsed().as_millis()).unwrap_or(u64::MAX);
                member.timing.execution_ms =
                    Some(elapsed.saturating_sub(member.timing.approval_wait_ms));
            }
            self.journal.append(
                turn,
                Some(item),
                Some(started),
                EventKind::ToolCallMeasured {
                    timing: member.timing,
                },
            )?;
            let (status, mut result, hook_call) = match outcome {
                ToolOutcome::Executed {
                    status,
                    result,
                    hook_call,
                } => (status, result, Some(hook_call)),
                ToolOutcome::Answered { ok, result } => {
                    (ExecutionStatus::from_ok(ok), result, None)
                }
                // A member the policy refused at a boundary is that member's
                // failed answer; any other refusal is a decision the turn
                // must act on, exactly as for a lone call.
                ToolOutcome::Refused { denial } if denial.authority() == Authority::Boundary => (
                    ExecutionStatus::Failed,
                    self.tools
                        .masked_result(ToolResult::builtin(denial.to_string())),
                    None,
                ),
                other @ (ToolOutcome::Refused { .. } | ToolOutcome::Unanswerable { .. }) => {
                    let denial = match &other {
                        ToolOutcome::Refused { denial } => refused_tool(denial),
                        ToolOutcome::Unanswerable { pending } => unanswerable_tool(pending),
                        _ => unreachable!("matched above"),
                    };
                    answers[index].status = Status::Error;
                    answers[index].output = denial.action.clone();
                    self.journal.append(
                        turn,
                        Some(item),
                        Some(started),
                        EventKind::ToolCallCompleted {
                            ok: false,
                            result: self
                                .tools
                                .masked_result(ToolResult::builtin(denial.action.clone())),
                        },
                    )?;
                    return Ok(ToolOutcome::BatchStopped {
                        denial: Some(denial),
                        result: ToolResult::builtin(render(&answers)),
                    });
                }
                // The member suspended for input and the question settled in
                // this process (or ended the turn): the member's own item is
                // already complete, so the outer call is answered once from
                // the durable member items. Later members never start.
                other @ (ToolOutcome::Recorded | ToolOutcome::Ended(_)) => {
                    self.settle_suspended_batches(Some(turn))?;
                    return Ok(other);
                }
                // Root readmission happens before dispatch. The turn driver
                // records the outer answer and re-admits; include prior
                // results so the model does not retry the whole batch.
                ToolOutcome::RootReadmission { result } => {
                    answers[index].output = result.model_text();
                    self.journal.append(
                        turn,
                        Some(item),
                        Some(started),
                        EventKind::ToolCallCompleted {
                            ok: false,
                            result: self.tools.masked_result(result),
                        },
                    )?;
                    return Ok(ToolOutcome::RootReadmission {
                        result: ToolResult::builtin(render(&answers)),
                    });
                }
                // The outer call stays unfinished and owned until its member
                // settles; see `settle_suspended_batches`.
                ToolOutcome::QuestionRequired => return Ok(ToolOutcome::QuestionRequired),
                ToolOutcome::BatchStopped { .. } => {
                    unreachable!("a member cannot dispatch a batch")
                }
            };
            let ok = status.ok();
            let waiting_for_user = status == ExecutionStatus::Completed
                && hook_call.is_some()
                && blocked_progress(args, &result);
            self.tools.finish_result(&mut result, self.invocation, item);
            let output = member_output(&result);
            self.journal.append(
                turn,
                Some(item),
                Some(started),
                EventKind::ToolCallCompleted { ok, result },
            )?;
            if let Some(hook_call) = hook_call {
                let lifecycle = if ok {
                    HookLifecycle::PostToolUse
                } else {
                    HookLifecycle::PostToolUseFailure
                };
                let _ = self
                    .dispatch_hook(
                        HookCausality::item(item, turn, started),
                        HookEvent::tool_call(lifecycle, HookSubject::Item { item }, hook_call),
                    )
                    .await;
            }
            answers[index] = Answer {
                tool: name.clone(),
                status: match status {
                    ExecutionStatus::Completed => Status::Ok,
                    ExecutionStatus::TaskStarted => Status::TaskStarted,
                    ExecutionStatus::Failed => Status::Error,
                },
                output,
            };
            if waiting_for_user {
                for later in &mut answers[index + 1..] {
                    later.output =
                        "not run: an earlier member reported that it is waiting for the user"
                            .to_owned();
                }
                return Ok(ToolOutcome::BatchStopped {
                    denial: None,
                    result: ToolResult::builtin(render(&answers)),
                });
            }
            if status != ExecutionStatus::Completed {
                if status == ExecutionStatus::TaskStarted {
                    for later in &mut answers[index + 1..] {
                        later.output = "not run: an earlier member started an MCP task. Call this member directly if still needed; do not restart the task.".to_owned();
                    }
                }
                stopped = true;
                break;
            }
        }
        Ok(ToolOutcome::Answered {
            ok: !stopped,
            result: ToolResult::builtin(render(&answers)),
        })
    }
}
