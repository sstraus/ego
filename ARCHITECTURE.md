# ego — Architecture

Date: 2026-08-19. Status: **ratified by maintainer** after adversarial review
(`docs/08-codex-critique.md`) and triage (`docs/10-triage.md`).
Resolves the eight open questions of `SPEC.md` §4. Evidence citations refer to
the dossier in `docs/`.

## 0. Product stance

ego is a **super-compact, scripting-first, pure-CLI harness**. Competitive with
fx and pi on capability, but its identity is Unix: prompt in, events out,
meaningful exit codes, composable with pipes. Every host surface — interactive
terminal, one-shot, JSONL stream, ACP — is a *view over the same event stream*.
That single sentence is the architecture; everything below is its consequence.

## 1. The event model (the load-bearing decision)

State is a **reduction over an append-only event log** (pi's reducer,
`docs/02-pi.md` §3, generalised):

```
PromptQueued | PromptDeliveryExpedited | UserPrompt | AssistantDelta |
AssistantMessage | ReasoningDelta | ReasoningState | ToolCallStarted |
ToolCallDelta | ToolCallCompleted | PermissionRequested | PermissionAmended |
PermissionDecided | QuestionRequested | QuestionSettled | GenerationStarted |
ExecutionObserved | AttemptSettled | UsageUpdated | UsageCorrected |
ContextWindowUpdated | TurnCompleted | Compacted | Forked |
CompactionSeeded | HookCatalogueAdmitted | HookInvocationStarted |
HookInvocationSettled | SubagentSpawned | SubagentReturned | Error
```

Thirty-one in the final M1 schema, and this list is exhaustive by contract.
The current `EventKind` (`crates/ego-core/src/event/domain.rs`) implements
twenty-eight. regression case adds the two question-lifecycle variants; story
regression_case adds the clean-session compaction seed. The enum and this list are
compared by hand at each milestone close.

- `SessionState::apply(&mut self, &Event)` is a **pure, sync** function.
- Legacy in-place compaction, clean-session compaction lineage, and forking are
  events *in* logs, not operations *on* them.
- Every event carries a monotonic sequence number, stable event/turn/item IDs,
  a causal parent ID, a timestamp and a producer identity — required the
  moment hooks, subagents and concurrent tool calls exist.
- Busy input is durable state, not terminal state. `PromptQueued { id, text }`
  records acceptance; `PromptDeliveryExpedited { through }` records monotonic
  `Esc` intent through one FIFO tail; and `UserPrompt.queued_prompt` is the sole
  consumption record. Recovery replays queued IDs without that correlation and
  preserves expedited intent, exactly once. FIFO comes from event sequence.
  Acceptance, expedited intent, and correlated consumption each reach stable
  storage before their UI, interruption, or next-request effect is exposed.

**One semantic model, four serialisations** (R3 of the review: schema
*identity* across surfaces does not survive contact with compatibility):

| Representation | Contract |
| --- | --- |
| `DomainEvent` | exhaustive internal reducer input; evolves freely with core |
| `StoreRecord` | durable envelope: session ID, sequence, causal IDs, codec version, integrity checksum, versioned payload |
| `PublicEvent` | privacy-filtered, stable JSONL DTO with its own compatibility policy (`CLI.md` §4.2) |
| ACP adapters | capability-aware lossy projections onto protocol v1 and v2 |

Each has golden fixtures and projection/migration tests; a new internal event
never becomes public automatically. The semantics stay single-sourced — that is
what keeps the harness compact — while storage, scripting API and ACP evolve
independently.

## 2. Crate layout (resolves §4.1)

The final layout has six crates in one workspace. The current workspace has
the five implemented crates; M2 adds `ego-mcp` when its real client/server
surface exists. Dependency arrows point down only. The diagram is the target
layout, not authority to scaffold an empty sixth crate.

```
ego          (bin)  CLI: interactive line UI, headless, subcommands, config
├── ego-acp         ACP server on agent-client-protocol 2.0
├── ego-mcp         current MCP client + `ego mcp-server` on a release-gated
│                   rmcp 3.x
├── ego-tools       builtin tools + execution backends (the OS lives here)
├── ego-ai          provider layer: wire formats, provider table, usage,
│                   delivery certainty, model catalogues
└── ego-core        the harness: loop, reducer, permissions, sessions,
                    subagents, skills, hooks, compaction, truncation —
                    and the OWNER of the conversation types + all ports
```

**Ports belong to the consumer** (R5 of the review — the original layout had
`ego-core` depending on `ego-ai` for its fundamental types, which breaks at the
first integration). `ego-core` owns the provider-neutral conversation model
(`GenRequest`, messages, tool descriptors, usage) and a narrow `Generator`
port; `ego-ai` *implements* that port and owns `Http`, wire formats,
credentials and provider tables. M2 replaces `Http`'s OS implementation with
the shared core-owned guarded byte-transport port while leaving provider wire
semantics in `ego-ai`. Every satellite crate depends on core; the
binary wires implementations in. The crate count is a consequence, not a
requirement.

Rules:

- **`ego-core` opens no sockets and no files.** It sees the world through the
  traits in §3. `serde`, `tokio` (sync/time only), `futures` — yes;
  `reqwest`, `rusqlite`, `rmcp` — no.
- `ego-ai` owns provider HTTP semantics behind the core-owned guarded
  byte-transport port (§3.1); it never exposes its provider client to a sibling.
- `ego-tools` is where real I/O lives: guarded HTTP, filesystem tools, duplex
  process spawning, and sandbox backends. It implements `ego-core`'s traits.
- `ego-mcp` adapts current rmcp tools into `ego-core::Tool` objects and its
  resources/prompts into the core-owned `ContextSource`. It owns what rmcp does not provide
  (`docs/04-crates.md` §1): immutable admitted snapshots, access policy,
  startup/reload admission, truthful health, input validation posture, and
  foreign-domain egress integration. It selects MCP 2026-07-28 explicitly and
  enters the workspace only after the accepted MCP ADR's SDK release gate.
- `ego-acp` is a thin projection: core events → `session/update`, ACP requests
  → core operations. Target: same order of magnitude as pi's `protocol` +
  `server` (~3.5k lines TS), not fx's hand-rolled 8 files.

This is pi's proven decomposition (`ai`/`agent`/`coding-agent` → `ego-ai`/
`ego-core`+`ego-tools`/`ego`) with the two integration surfaces (MCP, ACP) as
their own crates so their heavyweight dependencies stay out of the core.

## 3. Core traits

### 3.1 Provider layer (`ego-ai`)

pi's taxonomy (`docs/02-pi.md` §2): few **wire formats**, many **providers**.

```rust
/// One implementation per API shape, and a mapping is told nothing about
/// which provider it is serving.
///
/// The target is six providers end-to-end; the mappings are fewer, and how
/// many is decided by measurement rather than by a vendor's badge — see
/// `docs/decisions/2026-08-21-a-shared-shape-is-not-a-shared-contract.md` §9
/// and `crates/ego-ai/tests/conformance.rs`. Ollama is the worked example: it
/// speaks the openai-completions shape everywhere except the output-ceiling
/// field, fails conformance check 1 on that one difference, and therefore has
/// its own mapping — which shares the family's whole decoder rather than
/// copying it.
trait WireFormat: Send + Sync {
    /// A target, not a provider row: a base, opaque headers, and no id. §2 of
    /// that ADR is this signature — a mapping that could ask who it is
    /// serving is a mapping that will eventually branch on the answer.
    fn build_request(&self, target: &RequestTarget, req: &GenRequest)
        -> Result<HttpRequest, RequestRefused>;
    /// The whole response, including a non-200, because a refusal is part of
    /// the protocol and every mapping must say the same three things about
    /// one (`wire::failed`).
    fn decode(&self, response: Result<HttpResponse, HttpError>,
              cost: Option<Arc<dyn ProviderCost>>) -> BoxStream<'static, AiEvent>;
}

/// A provider is a table row, not an integration — and it selects rather
/// than holds.
struct ProviderRow {
    id: String,
    /// Scheme, host and any prefix; the *mapping* owns the path.
    base: Url,
    /// A variable name and a header name. Never a secret: the resolved
    /// credential exists once, beside the model and not inside it (§6).
    credential: CredentialStrategy,
    /// Which API shape. The thing itself, not a name — a name needs a
    /// registry, and a registry needs a configuration file this build does
    /// not have yet.
    wire: &'static dyn WireFormat,
    /// What turns one of this provider's model ids into a `ResolvedModel`:
    /// read from what an operator supplied, or asked of a daemon.
    resolver: &'static dyn ModelResolver,
    /// Whether this provider states a monetary figure on the wire, and how to
    /// read it. `None` is a claim, not a gap.
    cost: Option<Arc<dyn ProviderCost>>,
}

/// Transport seam: one method, reqwest impl. Also where `--offline` lives,
/// because a host string is not a socket and the check belongs to whatever
/// owns the socket.
trait Http: Send + Sync {
    fn execute(&self, req: HttpRequest)
        -> BoxFuture<'static, Result<HttpResponse, HttpError>>;
}
```

**What is deliberately *not* on the row**, each because it belongs to a model
rather than to a provider: a model catalogue (hosted facts are supplied per
model, local ones measured from the daemon), a thinking-level map and a cache
retention (both are `ModelCapabilities` fields, resolved per model), and a
pricing revision (one revision on a row would assert a single table priced
every model under it, which is false as soon as two models are priced from
different places).

A provider row is configuration, **not** proof that models sharing an HTTP
shape have equivalent conversation semantics (I5): a versioned
`ModelCapabilities` record — tool/schema limits, parallel tool calls,
reasoning-state carryover, context/output ceilings, cancel semantics, cache
behaviour, transport — is resolved per generation and persisted in the run
manifest; fallback chains check conversation compatibility and explicitly
transform or drop provider-private reasoning state before switching. Unknown
cost fails the budget closed.

Carried over from fx because it is cheap and correct (`docs/01-fx.md` §2.4):
**delivery certainty** — `DeliveryCertainty { DefinitelyUnsent, PossiblySent }`
tracked per attempt, deciding whether a retry is safe or might double-bill, with
explicit retry ownership (transport vs agent). In from day one, since scripting
mode retries unattended.

Per-generation usage and cost are first-class outputs of `parse_stream`, not an
afterthought — the CLI's `--budget` and ACP's `UsageUpdate` both feed off them.

**Local models are a first-class provider class, not a footnote.** Ollama,
LM Studio, vLLM and llama.cpp-server are all rows in the provider table on the
openai-completions shape, working from M0 with `auth: None`. Beyond the table:
`ego models` queries the local daemon's catalogue live (`/v1/models`,
`/api/tags`) instead of a generated file; `ego doctor` detects running local
daemons; `--offline` (pi's flag) asserts no non-loopback egress — enforced at
the `Http` seam, not by convention — which is also the air-gapped posture for
the server deployment (`docs/09-server-companion.md`). Cost accounting reports
zero-cost usage rather than pretending local tokens are billable.

### 3.2 Tools (`ego-core`, resolves §4.3)

**Dynamic dispatch.** MCP tools are dynamic by nature; a static registry macro
would buy nanoseconds against a network round-trip measured in seconds, and cost
a second registration mechanism. One trait, boxed — with **enforced** rather
than declared effects (R1 of the review: a declaration-only `intent()` lets
`run()` drift from what was approved):

```rust
trait Tool: Send + Sync {
    fn descriptor(&self) -> ToolDescriptor;      // name, description, JSON Schema
    fn prepare(&self, params: Value, ctx: &PrepareCtx)
        -> Result<PreparedCall, ToolError>;      // canonical ops, not prose
    fn execute(&self, call: PreparedCall, io: &dyn GuardedIo)
        -> BoxFuture<'_, Result<ToolResult, ToolError>>;
}
```



### 3.3 The environment seam (`ego-core`)

```rust
trait Env: Send + Sync {          // pi's harness/env, docs/02-pi.md §3
    fn fs(&self) -> &dyn Fs;      // read, write, glob, stat
    fn spawn(&self, cmd: Command, confinement: &Confinement) -> BoxFuture<Spawned>;
}
// Tools do NOT receive Env; they receive GuardedIo, a permit-checking facade
// over it (§3.2). Env is held by the loop and by GuardedIo's implementation.

trait Clock: Send + Sync {        // event/time.rs — the only reader of wall time
    fn now(&self) -> Timestamp;   // injectable for replay determinism
}
// A port of its own, and not `Env::clock() -> Instant` as this section said
// until M0b. An `Instant` is monotonic and opaque: it cannot produce the UTC
// millisecond an `Envelope.ts` is. The turn loop needs a clock and nothing
// else from the environment, and a port it can satisfy with one type is
// cheaper than an `Env` it would have to stub.
// Corrected by docs/decisions/2026-08-21-the-turn-loop.md §2.

trait SessionStore: Send + Sync {
    fn append(&self, id: &SessionId, ev: &Event) -> Result<()>;
    fn read(&self, id: &SessionId) -> Result<BoxStream<Event>>;
    fn list(&self) -> Result<Vec<SessionMeta>>;
}

trait Host: Send + Sync {         // the conceptual UI boundary
    fn request_permission(&self, req: PermissionRequest)
        -> BoxFuture<'_, PermissionDecision>;   // includes Amend(command)
    fn deliver(&self, batch: &[Event]);         // coalesced, docs/03 §2.3
    fn elicit(&self, req: Elicitation) -> BoxFuture<'_, ElicitationResponse>;
}
```

Event delivery to hosts is **batched/coalesced** in core (tuicommander's
`batched_conversation_stream` lesson) so no host reimplements it.
Implementations use narrow consumer-owned ports for these methods. Story
regression_case realizes `elicit` as the typed `Elicitor` port; it does not add a
second human-input boundary.

### 3.4 Permissions and sandbox (`ego-core` policy, `ego-tools` enforcement; resolves §4.4)



1. **Mode** — `plan` (read-only, ends in an approval gate) · `default`
   (reviews unresolved sensitive actions) · **`auto`** (judge model, below) ·
   `edits` (auto-approves workspace writes) · `yolo` (bypass).
2. **Rules** — persistent, scoped (project/user), with **stable IDs**,
   listable and revocable without the original context, storable without
   executing (`rules remember`). fx's contract verbatim (`docs/01-fx.md`
   §2.2). Rules are a **formal grammar with a canonical AST** (R9), matched
   against `PreparedCall` structure — argv, resolved paths, endpoints — never
   against rendered shell strings; `ego rules explain <tool> <args>` shows the
   canonical intent, the matching rule ID and any unenforced remainder without
   executing. Grammar published before persistent rules ship (M1 gate).
3. **Judge** — the `auto` backend: an unresolved sensitive action is evaluated
   by a configured judge model receiving the canonical `PreparedCall`;
   verdict allow / deny / escalate-to-human, with the reasoning recorded in
   the audit event. The judge is decoupled from the main model
   (`permissions.judge = "openai/gpt-5.6-luna"`, a GLM, or `ollama/qwen3`
   for private/air-gapped loops) — just another `Generator` call with its own
   budget line. Judge unavailable or uncertain ⇒ ask the human; headless ⇒
   exit 3.
4. **Execution backends** — OS confinement of tool subprocesses behind a
   trait sized for growth (I4):

```rust
trait ExecutionBackend: Send + Sync {
    fn capabilities(&self) -> ExecCaps;      // what it DEMONSTRABLY enforces
    fn prepare(&self, ws: &Workspace, policy: &Confinement) -> Prepared;
    fn spawn(&self, p: &Prepared, cmd: Command) -> BoxFuture<Spawned>;
}
```

Backends: `landlock` on Linux (runtime ABI feature-detection, explicit
handled-rights); **Seatbelt on macOS** via generated profile + `sandbox-exec`
(deprecated but functional — the same bet Codex makes); **null on Windows** in
v1 (honestly labelled: no confinement); rootless OCI containers post-v1.
Approval frequency keys on **per-operation demonstrated capability**, never on
a backend's name; a requested-but-unavailable guarantee fails closed (R2).

Confinement boundaries (R2): in-process tools are confined by the
permit-checking `GuardedIo`, subprocesses by the backend, and **egress policy
is enforced at all four exits** — provider HTTP, MCP transports, hooks, and
spawned commands — which is what makes `--offline` a guarantee. Spawned
commands receive the **real environment by default** (maintainer): credentials in
env are what make the agent useful; permits govern effects, not knowledge.
Env filtering is opt-in only. The narrow taint rule (secret source → network
sink) **asks** by default, never silently blocks; secret redaction applies to
exports, logs and telemetry, never to the working env.
Amend-before-approve is in the `PermissionDecision` enum from day one.

### 3.5 Subagents, skills, hooks (`ego-core`)

- **Subagents**: isolated sub-loops over the same core; the child's final
  message is a return value by contract. Parent/child communication is events
  in both logs (fx's stored-projection idea, simplified — full per-child
  authority registries only if v1 usage proves the need).
- **Skills**: strict `SKILL.md` instructions from system, user, or trusted
  project roots. Bounded model-visible descriptors are announced like admitted
  MCP metadata; bodies and rooted read-only resources load only on a typed
  user/model invocation. One immutable catalogue, source-tagged authority, the
  canonical permission engine, and durable rendered-content digests prevent a
  skill from self-granting or changing underneath resume. Current Agent Skills
  and Claude frontmatter is implemented or refused by field name; shell
  injection is never a skill capability. See `docs/20-agent-skills.md` and the
  accepted skills ADR. Executable agent files (`CLI.md` §5) reuse configuration
  and permission mechanisms but remain top-level run inputs, not skills.
- **Hooks**: one asynchronous `Hook` port owns the current stable
  Claude-compatible subset of lifecycle points that εgo actually produces.
  Configured `command`, `http`, `mcp_tool`, and `prompt` handlers are adapters
  over that port and reuse, respectively, `GuardedExec` plus confinement,
  guarded HTTP plus egress, the admitted MCP snapshot plus foreign-tool path,
  and the ordinary generator plus its accounting. A hook may veto, request the
  ordinary escalation path, or attach bounded source-tagged feedback; it cannot
  grant a permit or relax system rules, sandbox, offline, budget, judge, MCP
  admission, or egress. Unsupported lifecycle owners and deprecated or
  experimental fields and handlers fail admission explicitly. M1 finalizes
  stable identity, source/digest, and typed start/settlement evidence only; M4
  builds the strict catalogue, runner, four adapters, and inspection surfaces.
  See `docs/21-claude-hook-parity.md` and the accepted hook ADR.

### 3.6 Extensibility posture (plugins without a plugin system)

maintainer's constraint: plugins are desirable but must not complicate the core.
Resolution — v1 ships **four out-of-process extension surfaces** that already
exist in the design: MCP servers (tools are the tool plugin API; resources and
prompts are the external context API), configured hooks (lifecycle,
subtractive and able to block), skills (behaviour), and executable agent files
(`CLI.md` §5). In-process
extension is the **library API itself**: every port `ego-core` owns is a trait
an embedder implements (`Tool`, `Hook`, `Generator`, `Host`, `Env`,
`ExecutionBackend`; `WireFormat` and `ModelResolver` live in `ego-ai`), so
any Rust embedder supplies custom implementations without ego knowing — pi's
extension richness, minus the runtime.

**"Registry" was the wrong word and this corrects it.** A port is selected by
*value*: a row holds a `&'static dyn WireFormat` and a `&'static dyn
ModelResolver`, and an embedder that adds a protocol writes a row that names
its adapter. A registry — a name, a table, and a lookup that can fail — buys
exactly one thing that this does not: it lets a profile be written in a
*file*, because a file can hold a name and cannot hold a trait object. That
file does not exist yet, so the table would have one caller, be populated by
the same constructors it serves, and live in the same crate. It arrives with
the file, and nothing built now has to be undone for it
(`docs/decisions/2026-08-21-a-shared-shape-is-not-a-shared-contract.md` §3,
amended 2026-08-22).

A dynamic loader (WASM or dylib) is
**explicitly deferred**: it would add an ABI/runtime burden now, and the
dyn-registry design means it can be bolted on later as "a loader that fills the
same registries" — no redesign, so deferring costs nothing.

## 4. Async model (resolves §4.2)

**Tokio, multi-thread, Send futures — but all decision logic is pure and sync.**

- The ACP SDK 2.0 is Send-based on multi-thread tokio (verified,
  `docs/06-acp.md` §4); rmcp and reqwest are tokio-native. A sync core would
  bridge against the entire dependency ecosystem.
- The testability argument for a sync core is answered differently: the
  reducer, the permission classifier, truncation, compaction selection — the
  logic that needs dense unit tests — are pure sync functions. The async layer
  is thin orchestration, tested with `#[tokio::test]` against fake `Http`/
  `Env`/`Host`.
- Structured concurrency: the loop owns its tasks; no detached `tokio::spawn`
  in core. Cancellation is a `CancellationToken` surfaced as first-class
  operations (cancel now; pause/resume mid-turn as an ego extension, since ACP
  lacks it).

## 5. Session storage (resolves §4.5)

**JSONL event log + JSON manifest, per session directory. No SQLite in v1.**

```
~/.ego/sessions/<id>/
    manifest.json      # id, workspace_root, model, created, updated, title
    events.jsonl       # the log; first line is a versioned header record
    snapshot.json      # a replay checkpoint carrying its source sequence
    lease              # the writer lease: PID + heartbeat, advisory-locked
```

Prompted compaction never rewrites one of these directories. Its accounted,
tool-less generation is journalled in the source session; success creates a new
directory whose first semantic event records the source ID, source sequence,
prompt revision, and generated context checkpoint. The checkpoint is the sole
conversation artifact carried from the source. The source log remains
append-only and independently resumable.

Rationale: the state is *already* an event log (§1) — an append-only JSONL file
is its natural serialisation, portable and greppable, and replay is just
re-reading it. But JSONL is a **format choice, not a transaction model** (R4):
the store ships with an explicit durability protocol, not with optimism:

- one **writer lease** per session, with defined conflict behaviour (CLI and
  ACP attaching to the same session must not interleave);
- monotonic sequence numbers and per-record checksums (`StoreRecord`, §1);
- defined **tail repair** for a half-written final line on crash;
- atomic manifest replacement and a documented fsync policy;
- periodic **snapshots** carrying their source sequence, so resume does not
  replay hundreds of megabytes;
- **content-addressed blobs** for large tool results and attachments, with
  reference counts and GC;
- a **run manifest** per session (I6): ego version, codec version, effective
  config/policy digests, provider capability digest, loaded
  instruction/skill/hook/MCP descriptors, workspace and target identity,
  execution-backend capabilities — what makes an export reproducible and a
  sanitized export provable;
- fault-injection tests at every crash point. **This suite is the gate**: if
  JSONL cannot pass it cleanly, the engine becomes SQLite behind the same
  `SessionStore` trait — decided by evidence, not taste.

Migrations: the header record carries the codec version; a migration is a
streaming rewrite that preserves (or explicitly refuses) unknown records. The
catalogue is a directory scan over manifests with an mtime cache; if that is
ever slow, the fix is an index file. fx's manifest-acceptance rules (recent +
`id` and `workspace_root` match, `docs/03-tuicommander.md` §2.5) are adopted.

## 6. Terminal approach (resolves §4.6)

**Primary-buffer line renderer + `reedline`. No alternate screen, mouse
capture, or ratatui in v1.** The terminal owns transcript scrolling, selection,
and copy. Output therefore behaves like shell output, survives copy-paste, and
works over ssh and in CI logs. Full capability is not traded for that
simplicity: the interactive surface keeps a live multiline composer, native
status line, keyboard menus, questions, model selection, a local LIFO draft
stash, durable FIFO busy-turn steering, and an ordinary FIFO only for
non-steerable or idle-transition cases.

The interactive `Host` owns the terminal as one surface. Reedline's external
printer accepts the renderer's bounded output batches while the editor remains
live, then repaints the exact composer and ephemeral chrome below the appended
transcript. The renderer never writes around it. Headless and protocol hosts
continue to consume the same events without terminal chrome. A future
full-screen front-end would still be a different `Host`, not a core change —
and tuicommander over ACP already *is* the GUI (`SPEC.md` §3).

The surface ships with an **ownership contract** (R12): exactly one component
writes terminal bytes; transcript output is append-only; status, menus, and the
composer are the only ephemeral rows; every redraw preserves the in-progress
buffer; input and rendered events cross bounded channels with explicit
backpressure; every interactive state has a plain non-TTY representation; and
golden-transcript tests cover narrow widths, resize, wide graphemes, and
interleaved input/output. `docs/decisions/2026-08-24-the-surface-keeps-the-seat-live.md`
supersedes the earlier sequential-seat implementation decision.

## 7. ACP integration (resolves §4.7)

Verified against the schema crates, full record in `docs/06-acp.md`. Summary:
ACP covers everything tuicommander needs — session catalogue (`session/list`),
switching/resume/fork/delete, model selection (`providers/*`), config
(`session/set_config_option`), usage *and cost* (`UsageUpdate`), approvals,
streaming — natively or behind `unstable_*` crate features. The single gap is
mid-turn pause/resume → ego extension method. `ego-acp` implements the agent
side of protocol v1 and v2 through the runtime crate's version negotiation.
No filesystem archaeology remains on tuicommander's side.

Coverage is **negotiated per connection, not assumed** (R11): pinning crates
controls ego's build, not each host's capability set. Every ACP operation is
modelled as a negotiated capability with a defined fallback when absent;
unstable DTOs stay inside the `ego-acp` adapter; projection fixtures and a
compatibility matrix run against at least Zed and tuicommander. ego extensions
are namespaced and optional.

## 8. Sequencing (resolves §4.8)

Each milestone ends demonstrable. Resequenced after the review (R6): the
contracts most likely to reshape everything land **before** the first write
tool, and an ACP projection spike runs early so public types are exercised by
a second consumer before they ossify. The seven review gates (docs/08, final
section) are M0a's exit criteria.

| M | Content | Demo / exit criteria |
| --- | --- | --- |
| **M0a — contracts** | The four event representations with golden fixtures (§1); stable ID/ordering scheme; single-writer durable store passing fault-injection; crate DAG with `Generator` port and fakes for `Generator`/`GuardedIo`/`Host` (also the eval seam, U3); `ModelCapabilities` record; strict trusted-config loading; `tracing` spans throughout. | The seven docs/08 gates pass; `cargo test` proves replay, projection and crash-recovery on synthetic logs. |
| **M0b — vertical slice** | `ego-ai`: openai-completions (**anthropic-messages deferred** — plan Open Question 4, answered 2026-08-21: one wire format proves the seam, and no Anthropic credential exists to verify a second end to end), streaming, usage, delivery certainty. Loop + reducer + resume. **Read-only tools** (read, grep, glob) via `GuardedIo`. AGENTS.md discovery (trust-gated, I2). Line UI + headless `--output json/jsonl` + exit codes. **ACP spike**: one prompt + updates driven from a host. | `ego "explain this failing test"` interactively; `ego run --output json … \| jq` in a script; kill → `ego resume`; the same session driven over ACP. |
| **M1 — safety, writes, judge, interactive control** | prepare/permit/execute in anger: write/edit/bash tools; permission grammar + canonical AST + stable-ID rules + `rules explain`; modes incl. `auto` (judge model) and plan; landlock + Seatbelt backends with per-operation capability accounting; egress policy at all four exits; truncation; **minimum viable compaction**; narrow taint (ask-only). Finalize durable hook identity, admitted source/digest, typed invocation start/settlement, feedback reference, and crash uncertainty without building a partial loader or handler. Complete the final keyboard interaction contract directly: live composer over streaming output, status line, slash/action menus, model picker, `AskQuestion`, draft stash, durable pending steers, and phase-safe interruption. Bring forward the shared current-model catalogue: supplied hosted facts plus authoritative provider/local discovery feed resolution, `ego models`, `/model`, and `Option-P` through one projection. | Fix-the-failing-test end to end while the operator edits and queues multiple follow-ups: hook evidence replays with exact identity and uncertain external settlement fails according to its pre-effect or observer boundary; near-zero prompts under workspace sandbox with the judge on; `ego models --json` and the picker expose the same selectable rows; model and effort change from `Option-P`; busy input is delivered FIFO after the complete durable sibling tool batch, a no-tool final forces its follow-up, and `Esc` remains truthful through every active phase; same task on the null backend shows escalation; `--offline` verified at every exit. |
| **M2 — MCP, both directions** | Outbound MCP 2026-07-28 through a release-gated rmcp 3.x: stdio + stateless Streamable HTTP over core-owned guarded transports, explicit discovery/per-request metadata/standard headers, current CIMD OAuth, tools plus `ContextSource` resources/prompts, cache hints, tool-list subscriptions, and MRTR; one immutable admitted tool capture per generation; snapshots refresh atomically from add/rm/login/reload/config/TTL/list changes; failed refresh retains last-good and truthful health; foreign direct and collapsed meta-tools require exact tool plus egress authority. Bring the final rooted `RunController` and leases forward from M4. **`ego mcp-server`** (I1): Claude-compatible MCP 2025-11-25 stdio with run/continue/cancel/status tools and sanitized resources, no second served revision, MCP Tasks, or server prompts. | A current static or collapsed MCP server reloads without changing an active generation, every selected inner operation crosses permission/egress, a failed refresh retains last-good, and Claude Code delegates a scoped run to εgo over 2025-11-25; exact-Luna proves cancellation before and after the first outbound HTTP event on a released SDK. |
| **M3 — sessions in full** | Full compaction (intent, latest results, blockers, verification state survive), fork, replay/export (`--sanitize`), catalogue polish, budgets incl. judge budget line, run-manifest completeness, `[profile.ci]` contract (U1). | `--budget`, `ego fork`, compaction across a context-overflow task; a CI job consumes the sanitized run bundle. |
| **M4 — subagents, skills, hooks** | Sub-loops with context isolation over M2's existing `RunController` IDs/leases (U2); strict Agent Skills/Claude-compatible `SKILL.md` frontmatter, system/user/project admission, progressive disclosure, typed user/model invocation, `/skills` rescan, durable digest/rendered content, and fail-closed fork/hook fields; one strict source-bound hook catalogue and deterministic runner over the `Hook` port; stable `command`, `http`, `mcp_tool`, and `prompt` adapters through their real safety owners; `/hooks` and read-only `ego hooks ls`; executable agent files remain top-level run inputs. | A model sees only admitted skill descriptors, invokes one through the typed Skill operation, and cannot widen tools/sandbox/offline/budget; user-only/model-only visibility, reload failure, resume/compaction, and fork ownership are proven; `/hooks` and `ego hooks ls --json` project the same immutable catalogue, unsupported/deprecated/experimental hook inputs fail by exact name, a pre-effect hook blocks a forbidden commit without granting authority, adapter settlement is durable, an executable `.ego` file runs as a shebang script, and parallel subagents hold distinct leases. |
| **M5 — ACP full + remaining shapes** | `ego-acp` complete (v1+v2, negotiated capabilities), tuicommander + Zed as hosts; openai-responses + google-generative-ai shapes. New shapes register their final resolution sources in M1's existing model catalogue rather than introducing another catalogue. | ego driven from Zed and tuicommander with session list, model switch, live cost. |

Order rationale: contracts before code that would freeze them; safety before
the first write lands in anger; permissions before MCP because the permit
model must exist when foreign tools arrive; MCP before subagents so MCP tools
are first-class inside sub-loops; ACP *spike* early (host neutrality is
tested, not assumed) but ACP *completeness* last. A per-milestone CLI
availability table in `CLI.md` keeps documented flags honest — no flag ships
before its contract.

## 9. Risks

1. **Seatbelt deprecation** — `sandbox-exec` may break in a future macOS.
   Mitigation: it is one `Sandbox` impl; the null backend + classifier is the
   documented fallback. Codex shares this exposure.
2. **ACP `unstable_*` churn** — the features ego needs are unstable upstream.
   Mitigation: pin versions; `ego-acp` is the only crate exposed.
3. **Scope of v1** — four blocks is a lot. The mitigation *is* §8: every
   milestone is a usable tool, so scope pressure cuts tail milestones, not
   quality.
4. **fx attribution** — any verbatim reuse of fx's system prompt sections
   requires Apache-2.0 attribution in `NOTICE`. Decide at M0 when the prompt
   is written.
