//! Ephemeral, depth-one children over the ordinary turn loop.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use futures::future::{BoxFuture, join_all};
use futures::stream::BoxStream;
use serde::Deserialize;
use serde_json::{Value, json};

use super::*;
use crate::event::domain::DomainEvent;
use crate::run::{RunAppend, RunRevision, RunTransaction};
use crate::sink::EventSink;
use crate::store::session_store::{SessionEntry, SessionMeta, SessionStore};

/// A trusted, immutable task definition. All authority fields are ceilings.
#[derive(Clone, Debug)]
pub struct NamedAgent {
    pub name: String,
    pub prompt: String,
    pub model: Option<ModelRef>,
    pub tools: Option<BTreeSet<String>>,
    pub mode: Option<Mode>,
    pub deny: Vec<crate::permit::rule::Rule>,
    pub sandbox: Option<crate::permit::Confinement>,
    /// When present, only the existing workspace root is admitted.
    pub roots: Option<Vec<crate::permit::CanonPath>>,
}

struct NamedModelSource {
    owner: Arc<dyn GenerationModelSource>,
    model: ModelRef,
}
impl GenerationModelSource for NamedModelSource {
    fn resolve<'a>(
        &'a self,
        request: GenerationModelRequest,
    ) -> BoxFuture<
        'a,
        Result<GenerationProfile, crate::generation_model_source::GenerationModelError>,
    > {
        self.owner.resolve(request.with_model(self.model.clone()))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskRequest {
    name: String,
    prompt: String,
    agent: Option<String>,
    plan_step: Option<String>,
    #[serde(default)]
    background: bool,
}

pub(super) fn descriptor() -> ToolDescriptor {
    ToolDescriptor {
        name: "task".into(),
        description: "Run an isolated child agent with your tools, permissions, sandbox, offline posture and shared budget. Children cannot delegate. Returns only its bounded final answer. Set background=true for durable standalone work that must outlive this parent turn; returns an ID immediately. ".into(),
        parameters: json!({"type":"object", "additionalProperties":false,
            "required":["name","prompt"], "properties": {
                "background":{"type":"boolean", "default":false, "description":"Launch a durable child and return its ID without waiting. Parent interruption does not cancel it."},
                "plan_step":{"type":"string", "description":"Existing durable plan step ID. Child execution marks it in_progress, then completed or blocked."},
                "name":{"type":"string", "minLength":1,"maxLength":80},
                "prompt":{"type":"string", "minLength":1,"maxLength":65536},
                "agent":{"type":"string", "minLength":1,"maxLength":80}
            }}),
    }
}

pub(super) fn parse(params: Value) -> Result<TaskRequest, String> {
    let request: TaskRequest = serde_json::from_value(params)
        .map_err(|error| format!("task was called with invalid arguments: {error}"))?;
    if request.name.trim().is_empty()
        || request.name.chars().count() > 80
        || request.name.chars().any(char::is_control)
        || request.name.contains('›')
        || request.prompt.trim().is_empty()
        || request.prompt.len() > crate::mcp::FOREIGN_VISIBLE_BYTES
    {
        return Err("task requires a printable name of 1–80 characters and a nonempty prompt of at most 64 KiB".into());
    }
    Ok(request)
}

/// A durable task crosses the same product-owned child boundary as forked skills.
#[derive(Clone, Debug)]
pub struct BackgroundTaskLaunch {
    pub invocation: crate::event::id::SubagentInvocationId,
    pub turn: TurnId,
    pub item: ItemId,
    pub started: EventId,
    pub prompt: String,
    pub model: Option<ModelRef>,
    pub mode: Mode,
    pub policy: GenerationPolicyDigest,
    pub tools: BTreeSet<String>,
}

pub trait BackgroundTaskPort: Send + Sync {
    /// Aggregate corrected usage across the durable family, including the parent.
    /// Hosts without a separate durable child ledger retain local accounting.
    fn family_usage(&self) -> Result<Option<Usage>, StoreError> {
        Ok(None)
    }

    fn launch_task(&self, task: BackgroundTaskLaunch) -> BoxFuture<'_, Result<(), String>>;
    /// Collect only settled children; never wait for a live child at a boundary.
    fn refresh_tasks(&self, state: SessionState) -> BoxFuture<'_, Result<(), String>>;
}

/// Host-selected policy text is shared by the prompt and tool catalogue.
pub fn delegation_guidance(tuic: bool) -> &'static str {
    if tuic {
        "Inside TUIC, prefer orchestrator_active for long delegated work. End your turn after launching managed peers; RESULT/BLOCKED resumes it. Use task only for short in-turn work."
    } else {
        "For standalone delegation, use task. Set background=true for long work; the durable child continues independently of parent-turn interruption. Use /tasks to inspect or cancel it."
    }
}

/// Only accounting crosses into the parent's durable log. Conversation and
/// tool details belong to the child's ephemeral store and reduction.
struct TaskStore<'s> {
    parent: SharedJournal<'s>,
    turn: TurnId,
    started: EventId,
    events: Mutex<Vec<DomainEvent>>,
}

impl SessionStore for TaskStore<'_> {
    fn append(&self, _: &SessionId, event: &DomainEvent) -> Result<(), StoreError> {
        let kind = match &event.kind {
            EventKind::GenerationStarted { attempt, .. } => Some(EventKind::GenerationStarted {
                attempt: *attempt,
                role: GenerationRole::Subagent,
                snapshot: None,
            }),
            kind @ (EventKind::UsageUpdated { .. }
            | EventKind::UsageCorrected { .. }
            | EventKind::AttemptSettled { .. }) => Some(kind.clone()),
            _ => None,
        };
        if let Some(kind) = kind {
            self.parent
                .append(self.turn, None, Some(self.started), kind)?;
        }
        self.events
            .lock()
            .map_err(|_| StoreError::Io(std::io::Error::other("child journal lock")))?
            .push(event.clone());
        Ok(())
    }

    fn read(
        &self,
        _: &SessionId,
    ) -> Result<BoxStream<'static, Result<SessionEntry, StoreError>>, StoreError> {
        let entries = self
            .events
            .lock()
            .map_err(|_| StoreError::Io(std::io::Error::other("child journal lock")))?
            .iter()
            .cloned()
            .map(|event| Ok(SessionEntry::Event(event)))
            .collect::<Vec<_>>();
        Ok(Box::pin(futures::stream::iter(entries)))
    }

    fn compare_and_append_run(
        &self,
        _: &SessionId,
        _: Option<RunRevision>,
        _: RunTransaction,
    ) -> Result<RunAppend, StoreError> {
        Err(StoreError::Io(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "task children cannot create durable runs",
        )))
    }

    fn list(&self) -> Result<Vec<SessionMeta>, StoreError> {
        Ok(Vec::new())
    }
}

/// The shared activity vocabulary for foreground and durable child projections.
/// Text is presentation only; consumers apply their own terminal and storage bounds.
pub fn child_activity(event: &PublicEvent) -> Option<String> {
    match event {
        PublicEvent::TurnStarted { .. } => Some("Working".to_owned()),
        PublicEvent::ItemDelta {
            kind: crate::public::ItemKind::Reasoning,
            ..
        } => Some("Thinking".to_owned()),
        PublicEvent::ItemDelta {
            kind: crate::public::ItemKind::AgentMessage,
            ..
        } => Some("Responding".to_owned()),
        PublicEvent::ItemStarted {
            kind: crate::public::ItemKind::Tool,
            name: Some(name),
            text,
            ..
        } => {
            let args: Value = text
                .as_deref()
                .and_then(|text| serde_json::from_str(text).ok())
                .unwrap_or(Value::Null);
            Some(match args.get("path").and_then(Value::as_str) {
                Some(path) => format!("{name} {path}"),
                None => name.clone(),
            })
        }
        PublicEvent::ItemStarted {
            kind: crate::public::ItemKind::Permission,
            ..
        } => Some("Waiting for permission".to_owned()),
        PublicEvent::ItemCompleted {
            kind: crate::public::ItemKind::Tool,
            ..
        }
        | PublicEvent::UsageUpdated { .. } => Some("Working".to_owned()),
        _ => None,
    }
}

/// The parent journal serializes publication to the existing terminal writer.
/// No child transcript is appended to the parent conversation.
struct TaskSink<'s> {
    parent: SharedJournal<'s>,
    turn: TurnId,
    name: String,
    item: ItemId,
}
impl EventSink for TaskSink<'_> {
    fn publish(&self, event: &PublicEvent, state: &SessionState) {
        // Keep progress on the same serialized publication path as nested tools.
        let activity = child_activity(event);
        if let Some(activity) = activity
            && let Err(error) = self.parent.publish(&PublicEvent::ChildProgress {
                turn: self.turn,
                item: self.item,
                activity,
                output_tokens: state.usage.output_tokens,
            })
        {
            tracing::error!(%error, "child activity publication failed");
        }
        let event = match event {
            PublicEvent::ItemStarted {
                item,
                kind: crate::public::ItemKind::Tool,
                name,
                text,
                ..
            } => PublicEvent::ItemStarted {
                turn: self.turn,
                item: *item,
                kind: crate::public::ItemKind::Tool,
                name: name.as_ref().map(|tool| format!("{} › {tool}", self.name)),
                text: text.clone(),
            },
            PublicEvent::ItemCompleted {
                item,
                kind: crate::public::ItemKind::Tool,
                ok,
                text,
                ..
            } => PublicEvent::ItemCompleted {
                turn: self.turn,
                item: *item,
                kind: crate::public::ItemKind::Tool,
                ok: *ok,
                text: text.clone(),
            },
            // ACP needs the child call id to attach its existing permission modal.
            PublicEvent::ItemStarted {
                item,
                kind: crate::public::ItemKind::Permission,
                name,
                text,
                ..
            } => PublicEvent::ItemStarted {
                turn: self.turn,
                item: *item,
                kind: crate::public::ItemKind::Permission,
                name: name.clone(),
                text: text
                    .as_ref()
                    .map(|text| format!("Child {}: {text}", self.name)),
            },
            PublicEvent::ItemCompleted {
                item,
                kind: crate::public::ItemKind::Permission,
                ok,
                text,
                ..
            } => PublicEvent::ItemCompleted {
                turn: self.turn,
                item: *item,
                kind: crate::public::ItemKind::Permission,
                ok: *ok,
                text: text.clone(),
            },
            _ => return,
        };
        // Publication has no persistence path. Accounting failures instead
        // propagate from TaskStore::append and abandon the child's turn.
        if let Err(error) = self.parent.publish(&event) {
            tracing::error!(%error, "child progress publication failed");
        }
    }
}

struct TaskPolicySource {
    owner: Arc<dyn GenerationPolicySource>,
    snapshot: Arc<GenerationPolicySnapshot>,
    definition: Option<NamedAgent>,
}
impl GenerationPolicySource for TaskPolicySource {
    fn current(&self) -> Result<Arc<PolicyEngine>, String> {
        Ok(self.capture()?.policy())
    }
    fn capture(&self) -> Result<GenerationPolicyCapture, String> {
        let capture = self.owner.restore(self.snapshot.clone())?;
        match &self.definition {
            Some(definition) => capture
                .task_ceiling(
                    definition.mode,
                    &definition.deny,
                    definition.roots.as_deref(),
                )
                .map_err(|error| error.to_string()),
            None => Ok(capture),
        }
    }
    fn restore(
        &self,
        snapshot: Arc<GenerationPolicySnapshot>,
    ) -> Result<GenerationPolicyCapture, String> {
        if snapshot.digest() != self.capture()?.snapshot().digest() {
            return Err("task cannot widen its captured parent policy".into());
        }
        self.capture()
    }
}

struct TaskExecution<'s, 't> {
    session: SessionId,
    parent: SharedJournal<'s>,
    background: Option<Arc<dyn BackgroundTaskPort + 't>>,
    turn: TurnId,
    evidence: &'s dyn EvidenceStore,
    clock: &'s dyn Clock,
    runtime: GenerationRuntime,
    tools: Tools<'t>,
    cancel: CancelToken,
    judge: Option<JudgeContext>,
    analysis_cache: Option<(
        Arc<dyn crate::permit::analysis::PermissionAnalysisCache>,
        crate::config::Trust,
    )>,
    analysis_observation: Option<ExecObservation>,
    effect_ceiling: Option<Arc<BTreeSet<String>>>,
    elicitor: Option<Arc<dyn Elicitor>>,
}

impl<'s: 't, 't> TaskExecution<'s, 't> {
    fn run(
        self,
        started: EventId,
        item: ItemId,
        request: TaskRequest,
    ) -> BoxFuture<'t, Result<(bool, ToolResult), StoreError>> {
        Box::pin(async move {
            let store = TaskStore {
                parent: self.parent.clone(),
                turn: self.turn,
                started,
                events: Mutex::new(Vec::new()),
            };
            let sink = TaskSink {
                parent: self.parent.clone(),
                turn: self.turn,
                name: request.name,
                item,
            };
            let mut child = TurnLoop::new(
                Session {
                    id: self.session,
                    state: SessionState::default(),
                    store: &store,
                    evidence: self.evidence,
                    clock: self.clock,
                    sink: &sink,
                    cancel: self.cancel.clone(),
                },
                self.runtime,
                self.tools,
            )
            .map_err(|error| StoreError::Io(std::io::Error::other(error.to_string())))?;
            let parent = self.parent.clone();
            child.task_usage = Some(Arc::new(move || {
                if let Some(port) = &self.background
                    && let Some(usage) = port.family_usage()?
                {
                    return Ok(usage);
                }
                parent.inspect(|state| state.usage).map_err(Into::into)
            }));
            child.generation_role = GenerationRole::Subagent;
            child.judge = self.judge;
            child.analysis_cache = self.analysis_cache;
            child.analysis_observation = self.analysis_observation;
            child.inherited_effect_tools = self.effect_ceiling;
            child.elicitor = self.elicitor;
            let outcome = child
                .run(&request.prompt)
                .await
                .map_err(|error| error.cause)?;
            let state = child.state();
            let final_text = state
                .items
                .values()
                .filter(|item| item.kind == crate::state::ItemKind::Assistant)
                .max_by_key(|item| item.seq)
                .map(|item| item.text.clone())
                .unwrap_or_default();
            let text = if outcome.ok {
                final_text
            } else {
                format!(
                    "Child ended: {:?}{}",
                    outcome.exit_reason,
                    if final_text.is_empty() {
                        String::new()
                    } else {
                        format!("\n{final_text}")
                    }
                )
            };
            Ok((
                outcome.ok,
                child
                    .tools
                    .masked_result(ToolResult::builtin(bounded_result(text))),
            ))
        })
    }
}

fn bounded_result(mut text: String) -> String {
    let limit = crate::mcp::FOREIGN_VISIBLE_BYTES;
    let marker = "\n… [task result truncated]";
    if text.len() > limit {
        let mut end = limit - marker.len();
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str(marker);
    }
    text
}

impl<'s: 't, 't> TurnLoop<'s, 't> {
    pub(super) async fn run_tasks(
        &mut self,
        turn: TurnId,
        snapshot: &Arc<GenerationPolicySnapshot>,
        pending: &mut [PendingCall],
    ) -> Result<BTreeSet<ItemId>, StoreError> {
        let mut completed = BTreeSet::new();
        let serial = Arc::new(futures::lock::Mutex::new(()));
        let mut work = Vec::new();
        let mut claimed_steps = BTreeSet::new();
        for call in pending {
            if !matches!(call.selected.as_ref(), Some(Ok(RegisteredTool::Task))) {
                continue;
            }
            call.selected.take();
            let request = self
                .tools
                .prepare_selected(RegisteredTool::Task, &call.args);
            let request = match request {
                Ok(PreparedToolCall::Task(request)) => request,
                Ok(_) => unreachable!("task registry entry prepares a task"),
                Err(error) => {
                    self.journal.append(
                        turn,
                        Some(call.item),
                        Some(call.started),
                        EventKind::ToolCallCompleted {
                            ok: false,
                            result: self.tools.masked_result(ToolResult::builtin(error)),
                        },
                    )?;
                    completed.insert(call.item);
                    continue;
                }
            };
            let definition = request
                .agent
                .as_ref()
                .map(|name| {
                    self.tools
                        .named_agents
                        .iter()
                        .find(|agent| &agent.name == name)
                        .cloned()
                        .ok_or_else(|| {
                            if self.tools.named_agents.is_empty() {
                                format!("unknown named agent `{name}`; no named agents are configured, omit agent for an ordinary task")
                            } else {
                                let available = self
                                    .tools
                                    .named_agents
                                    .iter()
                                    .map(|agent| agent.name.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ");
                                format!("unknown named agent `{name}`; available: {available}")
                            }
                        })
                        .and_then(|definition| {
                            if definition.sandbox.is_some_and(|sandbox| {
                                self.config
                                    .identity
                                    .as_ref()
                                    .is_none_or(|identity| sandbox != identity.sandbox)
                            }) {
                                return Err(
                                    "named agent sandbox must match the captured parent backend"
                                        .into(),
                                );
                            }
                            snapshot
                                .validate_task_roots(definition.roots.as_deref())
                                .map_err(str::to_owned)?;
                            Ok(definition)
                        })
                })
                .transpose();
            let definition = match definition {
                Ok(definition) => definition,
                Err(error) => {
                    self.journal.append(
                        turn,
                        Some(call.item),
                        Some(call.started),
                        EventKind::ToolCallCompleted {
                            ok: false,
                            result: self.tools.masked_result(ToolResult::builtin(error)),
                        },
                    )?;
                    completed.insert(call.item);
                    continue;
                }
            };
            if let Some(id) = &request.plan_step {
                let update = crate::plan::PlanUpdate::Step {
                    id: id.clone(),
                    status: crate::plan::StepStatus::InProgress,
                };
                let valid = if claimed_steps.insert(id.clone()) {
                    self.state().plan.apply(&update)
                } else {
                    Err(format!("plan step already assigned in this batch: {id}"))
                };
                if let Err(error) = valid {
                    self.journal.append(
                        turn,
                        Some(call.item),
                        Some(call.started),
                        EventKind::ToolCallCompleted {
                            ok: false,
                            result: self.tools.masked_result(ToolResult::builtin(error)),
                        },
                    )?;
                    completed.insert(call.item);
                    continue;
                }
            }
            if request.background {
                // Do not silently drop named-agent policy or plan ownership on a durable run.
                let launched = if request.agent.is_some() || request.plan_step.is_some() {
                    Err("background task does not accept agent or plan_step; use an explicit prompt".to_owned())
                } else if let Some(port) = &self.background_task_port {
                    let mut tools: BTreeSet<_> = self.tools.effect_names().into_iter().collect();
                    tools.remove("orchestrator_active");
                    if let Some(ceiling) = self.inherited_effect_tools.as_ref() {
                        tools.retain(|name| ceiling.contains(name));
                    }
                    if let Some(scope) = self.active_skill_scope.as_ref() {
                        let allowed = scope.effect_tools();
                        tools.retain(|name| allowed.contains(&name.as_str()));
                    }
                    let invocation = crate::event::id::SubagentInvocationId::from_uuid(
                        call.item.to_string().parse().expect("item UUID"),
                    )
                    .expect("item UUIDv7");
                    port.launch_task(BackgroundTaskLaunch {
                        invocation,
                        turn,
                        item: call.item,
                        started: call.started,
                        prompt: request.prompt,
                        model: self.main_model_identity.clone(),
                        mode: snapshot.mode(),
                        policy: snapshot
                            .analysis_cache_digest(self.tools.mcp_output_root())
                            .map_err(|error| {
                                StoreError::Io(std::io::Error::other(error.to_string()))
                            })?,
                        tools,
                    })
                    .await
                    .map(|()| format!("background agent {invocation}"))
                } else {
                    Err("durable background tasks are unavailable in this host".to_owned())
                };
                let (ok, text) = match launched {
                    Ok(text) => (true, text),
                    Err(error) => (false, error),
                };
                self.journal.append(
                    turn,
                    Some(call.item),
                    Some(call.started),
                    EventKind::ToolCallCompleted {
                        ok,
                        result: self.tools.masked_result(ToolResult::builtin(text)),
                    },
                )?;
                completed.insert(call.item);
                continue;
            }
            let mut request = request;
            if let Some(definition) = &definition {
                request.prompt = format!("{}\n\n{}", definition.prompt, request.prompt);
            }
            let source = match definition
                .as_ref()
                .and_then(|definition| definition.model.clone())
            {
                Some(model) => Arc::new(NamedModelSource {
                    owner: self.source.clone(),
                    model,
                }) as Arc<dyn GenerationModelSource>,
                None => self.source.clone(),
            };
            let task = TaskExecution {
                session: self.session_id,
                parent: self.journal.clone(),
                background: self.background_task_port.clone(),
                turn,
                evidence: self.evidence,
                clock: self.journal.clock(),
                runtime: GenerationRuntime {
                    source,
                    policy_source: Arc::new(TaskPolicySource {
                        owner: self.policy_source.clone(),
                        snapshot: snapshot.clone(),
                        definition: definition.clone(),
                    }),
                    gate: self.gate.clone(),
                    config: {
                        let mut config = self.config.clone();
                        if definition
                            .as_ref()
                            .is_some_and(|definition| definition.model.is_some())
                        {
                            config.fallbacks.clear();
                        }
                        config
                    },
                    admission: self.admission.clone(),
                    invocation: self.invocation,
                    retry_pause: self.retry_pause.clone(),
                },
                tools: self
                    .tools
                    .task_child(
                        request.name.clone(),
                        serial.clone(),
                        self.cancel.clone(),
                        definition
                            .as_ref()
                            .and_then(|definition| definition.tools.clone()),
                    )
                    .map_err(|error| StoreError::Io(std::io::Error::other(error.to_string())))?,
                cancel: self.cancel.clone(),
                judge: self.judge.clone(),
                analysis_cache: self.analysis_cache.clone(),
                analysis_observation: self.analysis_observation.clone(),
                elicitor: self.elicitor.clone(),
                effect_ceiling: self
                    .active_skill_scope
                    .as_ref()
                    .map(|scope| {
                        Arc::new(
                            scope
                                .effect_tools()
                                .into_iter()
                                .map(str::to_owned)
                                .collect(),
                        )
                    })
                    .or_else(|| self.inherited_effect_tools.clone()),
            };
            if let Some(id) = &request.plan_step {
                self.journal.append(
                    turn,
                    None,
                    Some(call.started),
                    EventKind::PlanUpdated {
                        update: crate::plan::PlanUpdate::Step {
                            id: id.clone(),
                            status: crate::plan::StepStatus::InProgress,
                        },
                    },
                )?;
            }
            let journal = self.journal.clone();
            let item = call.item;
            let started = call.started;
            work.push(async move {
                let step = request.plan_step.clone();
                let outcome = task.run(started, item, request).await;
                if let Some(id) = step {
                    let status = if matches!(&outcome, Ok((true, _))) {
                        crate::plan::StepStatus::Completed
                    } else {
                        crate::plan::StepStatus::Blocked
                    };
                    journal.append(
                        turn,
                        None,
                        Some(started),
                        EventKind::PlanUpdated {
                            update: crate::plan::PlanUpdate::Step { id, status },
                        },
                    )?;
                }
                let (ok, result) = outcome?;
                journal.append(
                    turn,
                    Some(item),
                    Some(started),
                    EventKind::ToolCallCompleted { ok, result },
                )?;
                Ok::<_, StoreError>(item)
            });
        }
        let results = await_controlled(
            &self.journal,
            &self.cancel,
            &mut self.control,
            turn,
            join_all(work),
            InterruptDisposition::AwaitEffect,
            None,
        )
        .await;
        match results {
            Controlled::Completed(results) => {
                for result in results {
                    completed.insert(result?);
                }
            }
            Controlled::Failed(cause) => return Err(cause),
            Controlled::Interrupted => unreachable!("task batch awaits child settlement"),
        }
        Ok(completed)
    }
}
