# Architecture

εgo separates model generation, policy, external effects, and user interfaces.
The terminal, headless CLI, and protocol servers use the same durable session
and run machinery.

## 0. Crates

| Crate | Responsibility |
| --- | --- |
| `ego-core` | Events, configuration, permissions, budgets, turns, sessions, skills, hooks, and run control |
| `ego-ai` | Provider contracts, capabilities, pricing, wire formats, and authentication |
| `ego-tools` | Filesystem, process, and HTTP effects; storage, credentials, checkpoints, and orchestration adapters |
| `ego-cli` | Composition, command parsing, terminal interaction, output, and diagnostics |
| `ego-acp` | Agent Client Protocol projection over the durable core |
| `ego-mcp` | Inbound and outbound Model Context Protocol adapters |
| `ego-a2a` | Agent2Agent discovery |
| `ego-listen` | Shared endpoint admission and bounded listener transport |

## 1. Events and state

Session state is a reduction over an append-only event log.

```
SessionTitleChanged | SessionDeleted | HostContextAppended | SessionPauseRequested | SessionPaused |
SessionPauseObserved | SessionResumed | SessionSelectionsSelected | GoalSet | GoalTransitioned |
ProtocolFilterVariablesSaved |
ContextRequested | ContextRequestConsumed | ContextCooldown |
GoalAllowanceInherited | GoalCheckConfigured | GoalContinuation | GoalUpdated | GoalCheckStopped | PlanUpdated |
PromptQueued | PromptDeliveryExpedited | PromptRetracted | UserPrompt |
AssistantDelta | AssistantMessageStarted | AssistantMessage | ReasoningDelta | ReasoningState | ProviderContent |
ContextCommandStarted |
ToolCallStarted | ToolCallDelta | ToolCallCompleted | ToolCallMeasured |
DetachedExecStarted | DetachedExecOutput | DetachedExecSettled | QuestionRequested |
QuestionSettled | QuestionMemberSettled | PermissionRequested | PermissionAmended |
PermissionDecided | SessionRootGranted | PermanentRootGrantPending | PermanentRootGranted | RootRevoked | GenerationStarted | FallbackDecision | ExecutionObserved |
ExecutionMeasured | JudgeResponse | SandboxResidualDecided | SandboxOwnerGranted | AttemptSettled |
CostAnomaly | UsageUpdated | UsageCorrected | ContextWindowUpdated | ProviderLimitsObserved | TurnCompleted |
PinsChanged | CompactionSeeded | Compacted | Forked | HookCatalogueAdmitted |
HookLifecycleReached | HookInvocationStarted | HookInvocationSettled |
SubagentRequested | SubagentSpawned | SubagentSettled |
BackgroundTaskRequested | SubagentCompletionDelivered |
SkillCatalogueAdmitted | SkillInstructionAdmitted | SkillResourceAdmitted |
Error
```

79 variants in the current schema. The list is checked against `EventKind` in
`crates/ego-core/src/event/domain.rs`. `AssistantDelta` is decode-only: a turn
publishes text chunks live and writes one assembled `AssistantMessage` per item,
at its close. When the same generation opens another item first, a text-free
`AssistantMessageStarted` gives the message item its place in the log.

Events record prompts,
model output, tool calls and results, permissions, usage, questions, hooks,
subagents, and context transitions. Durable run records own run admission and
settlement. Protocol adapters project these facts; they do not create separate
policy authorities.

Session storage is private to the operator. Recovery, replay, and export preserve
session identity and ordering. Export applies redaction before producing public
artifacts. File checkpoints are separate Git recovery records, not replacements
for session history.

## 2. Generation

A resolved generation profile binds the provider, model, wire format, capabilities,
credentials, and budget. A request does not infer unsupported capabilities from
a provider name. Unavailable or unknown capabilities remain explicit.

Provider adapters own request encoding and response decoding. HTTP and WebSocket
connections use guarded transports. Continuation state is connection-local where
required; durable conversation state belongs to the core.

Wire adapters translate; a shared stage decides. A mapping turns its wire's
bytes into normalized events and states what the wire said. Decisions that do
not depend on the wire format, such as the handling of incoherent usage or of a
tool call without an id or a declared name, are made once outside the adapters.
`crates/ego-ai/src/wire/policy_contract.rs` runs one malformed answer through
every mapping and requires one outcome.

## 3. Tool authority

Tool input is parsed and prepared before policy approval. An approved operation
carries a permit bound to its target and policy. External-effect adapters consume
that authority to perform the operation. A model response alone cannot authorize
filesystem, process, or network access.

### 3.4 Permissions and sandbox

Permission mode and operating-system confinement are independent. Configuration
ceilings, credential floors, offline restrictions, and backend capabilities
remain applicable across user interfaces. An unavailable guarantee causes refusal,
not silent fallback to a weaker boundary.

Saved-session re-admission measures the current authority before effects or
session hooks. `ExecutionObserved` records admitted execution authority;
`ExecutionMeasured` records evidence only and does not replace saved authority.
Equal or provably narrower authority can proceed; potentially wider authority
requires human consent. Headless callers cannot approve that widening.
Confinement levels are `ro`, `workspace`, `adaptive`, and `off`; backend
capabilities determine which guarantees are available.

### 3.5 Interfaces

The interactive driver owns terminal input and rendering. Headless commands use
explicit stdout contracts, with diagnostics on stderr. ACP delegates user
interaction to a host while retaining session-scoped admission. MCP clients and
servers use their owning protocol adapters. A2A exposes discovery, not an alternate
run dispatcher.

## Further reading

- [Concepts](docs/concepts/README.md)
- [Permissions and confinement](docs/guides/permissions.md)
- [Command language](CLI.md)
- [Protocol integrations](docs/integrations/README.md)
- [Contribution guide](CONTRIBUTING.md)

## Optional workspace memory

Ego also has one opt-in built-in memory: with `memory = true` in the user configuration, or `--memory` for one run, each workspace has one user-readable Markdown file under `$EGO_HOME/memory/`, limited to 16 KiB, read at session start and at `/refresh` and appended to the system prompt. Only the user and the harness `memory` tool change it; plan mode refuses a write, every other mode applies it without a prompt, and the call is visible in the transcript and in `/context`. `session.started` lists the file (path, digest, size), never its text. Off by default; a project or local configuration file cannot turn it on; with it off, no memory file is read or written.
