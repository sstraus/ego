# ego — Specification

This specification reflects the October 30, 2026 review for the tagged
[0.4.0 release](docs/release-notes/0.4.0.md). The milestone results below
record native testing and integrated checks from that period. They do not
validate the current checkout or a later release candidate.

Contract freeze: 2026-09-03 (M4 complete; M5 contracts frozen). Status: **architecture ratified** —
`ARCHITECTURE.md` and `CLI.md`, revised after adversarial review (`docs/08`,
triage `docs/10`). M0a, M0b, M1, M2, M3, and M4 are complete. M4's accepted
post-green review findings are corrected and its closure sources are
reconciled. `ego mcp-server` negotiates its explicit
2025-03-26, 2025-06-18, and 2025-11-25 handshake set while outbound HTTP defaults to stateless 2026-07-28. An explicitly selected
`legacy-sse` profile pins 2024-11-05; failure never selects that profile
automatically. See `crates/ego-mcp/src/operation.rs` and
[the MCP guide](docs/integrations/mcp.md). M3 completes ordered session
streams, lineage replay and fork, rebuildable run evidence, role-attributed
usage, semantic compaction, sanitized export, and explicit CI publication.
Exact Luna's final M4 gate proved 2,410 ordinary workspace tests (2,378 passed,
32 intentional ignores) plus 20 doctests (all passed), with zero failures, and
every aggregate safety gate after the accepted post-green review findings were
resolved.

M5 is one final-product milestone executed in dependency phases. Statements
below that name its target are requirements, not claims about the current
binary; current availability remains explicit in `CLI.md` and the dated
provider/ACP audits.

Current closure scope (Boss, 2026-10-09): Windows continues to refuse
workspace/read-only/offline confinement; implementing that backend is excluded
(story 111-4e26, wontfix). The private coordinator bridge is removed (714-cad3); future coordination uses
the TUIC API. Five-day observer and comparative token reports remain excluded
(170-072e and 183-3624, wontfix).
M5 remains open pending final source/story reconciliation and current-candidate
gates. Use the coordinator-owned `progress/final-product.md` and story 095 for
closure tracking. Historical gate results above do not prove the current checkout.

## 1. What ego is

A coding agent harness written in Rust: a core library plus a thin CLI, able to
drive any mainstream model provider, speak MCP as a first-class client, and be
embedded by other hosts (editors via ACP, tuicommander, scripts).

The product stance, in Boss's words (2026-08-19): a **super-compact, friendly,
pure-CLI harness with no frills**, built from the
best of the existing stack — modern, efficient, fast, and **particularly suited
to scripting** and to protocol contexts such as ACP. The command language —
commands, flags, output contracts — is a first-class deliverable expected to
beat the incumbents combined (`CLI.md`).

It is minimal, native and embeddable, closer to a Unix tool than to an
IDE-in-the-terminal. It is written in Rust and provider-agnostic by construction.

## 2. Decisions already taken

These were confirmed by Boss on 2026-08-19 and are not open for re-litigation
unless new evidence appears.

### 2.1 Relationship with tuicommander — *standalone, integrated over ACP*

New, clean repository with its own dependencies. tuicommander's Rust code is
read as reference and rewritten where useful. Extracting crates from `src-tauri`
today would mean paying the cost of de-Tauri-ising `AppState` / `AppEvent` /
`app.emit()` while keeping tuicommander's tests green — that blocks ego on a
refactor of another project.

**Integration happens over ACP, not over shared crates.** ego is an ACP agent;
tuicommander becomes an ACP client. This is strictly better than the extraction
plan:

- Zero code coupling. Neither project constrains the other's internals, refactors
  or release cadence.
- The boundary is a versioned wire protocol with an existing Rust SDK on both
  sides (`agent-client-protocol` 2.0.0), not a hand-rolled API we would have to
  keep stable ourselves.
- tuicommander gets more than ego out of it: an ACP client can drive *any* ACP
  agent, not just ours.
- ego gets a second host for free, which forces the core to stay genuinely
  host-agnostic instead of quietly growing CLI assumptions.
- ego already needs an ACP server for editor integration. This makes it one
  surface serving two consumers rather than two integrations.

The M5 contract audit resolves the former coverage caveat. ego serves stable
ACP v1 only while v2 remains Draft, completes the selected durable session,
model/config, metadata, usage/cost, streaming, question, image, and permission
surface, and uses namespaced session-bound `_ego/pause` and `_ego/resume` for
the one required host control ACP does not standardize. Every capability is
negotiated per connection with an explicit fallback. Unrelated schema methods
are not product requirements. `docs/internals/06-acp.md` is the dated truth source.

Crate extraction from tuicommander is therefore **dropped**, not deferred.

### 2.2 Provider layer — *in-house client*

Distinct current provider contracts behind one in-house port, rather than
`genai` or `rig-core`.

Rationale: a coding agent lives or dies on exactly the details a generic client
abstracts away — prompt-cache breakpoints, reasoning effort, partial tool-call
streaming, per-generation usage and cost, and retry safety. `genai` is `0.7.0-beta`
and would sit on the critical path. `rig-core` brings its own agent loop that
would fight ours.

Use a small set of **API shapes** and a large table of **providers** that map
onto them. That turns "integrate N providers" into "implement a
handful of wire formats plus a config table". M5 adds public OpenAI Responses
without reusing the private subscription contract, and stable Gemini v1
Interactions without retaining `v1beta`. Published mappings become selectable
after deterministic conformance; live acceptance remains separately reported
evidence. The accepted amendment is
`docs/internals/decisions/2026-09-07-a-published-contract-is-enough-to-offer-a-row.md`,
which supersedes the live-recording selectability gate of 2026-09-03.

The provider catalogue validates trusted deployment instances before host
projection. Listings and selection use the instance's model ID and declared
facts. Setup checks presence in the same named credential source used at
admission. A captured session keeps its admitted instance definitions through
config saves and model switches. A settled session reopened in a fresh host
admits current configuration. If a host exits during a provider stream,
reloading settles the interrupted generation without sending it again. A
subsequent turn uses the newly admitted route and facts.

### 2.3 Product shape — *core library + thin CLI + ACP*

The core is an I/O-free crate with traits for transport, storage and UI. On top
of it: an interactive CLI, a headless one-shot mode, and one ACP server for
editors and the separate-repository TUICommander host.

Rationale: the core library makes the product embeddable without a WASM runtime.

### 2.4 v1 scope — all four capability blocks

1. **Permissions and sandbox** — modes, command classifier, persistent revocable
   rules, interactive approvals. Auto analysis binds complete executable and
   recognized interpreter/script identity, native arguments, captured cwd and
   environment, policy, measured confinement, trust and judge revision. Reuse
   only advisory opinions in a bounded private user-local cache; every call
   gets fresh policy checks and permits. High risk asks the human; unknown
   remains unknown after a model allow. Revalidate before spawn, refresh once
   on observed movement, and execute nothing after repeated movement.
2. **MCP client** — current MCP 2026-07-28: stdio and stateless Streamable HTTP,
   with an explicitly selected legacy SSE profile, never an automatic downgrade.
   Explicit discovery/per-request metadata/standard headers, OAuth with registered
   public-client, CIMD, or explicit issuer-advertised DCR registration, tools + resources + prompts, cache and tool-list subscriptions,
   MRTR human input, and atomically re-admitted snapshots for every command,
   configuration, TTL, and remote list change. Static and collapsed MCP
   toolsets are ordinary foreign tools, never an authority bypass. Every
   foreign call requires every exact opaque tool identity; its configured
   transport is sealed into the same permit, remains subject to explicit
   denies and runtime ceilings, and does not require a second allow. Each
   generation retains one immutable captured catalogue. Resources and prompts enter the
   final `ContextSource`, while the inbound stdio server adapts the rooted
   shared `RunController` for run/continue/cancel/status and sanitized
   resources. The exact released `rmcp` and injected-transport gate is in
   `docs/internals/19-current-mcp-and-collapsed-tools.md`.

   Registration, runtime credentials and refresh are code-landed in
   `crates/ego-cli/src/mcp.rs`, `crates/ego-mcp/src/oauth.rs`, and
   `crates/ego-tools/src/mcp_oauth.rs`. Story 476's operator login/restart check
   is complete by the recorded operator acceptance; local fixtures alone are
   not native-store live evidence.

   The inbound `ego mcp-server` stdio command negotiates the explicit MCP
   2025-03-26, 2025-06-18, and 2025-11-25 handshake revisions
   through one `rmcp`-owned handler and is available with optional
   non-zero `--max-concurrent-runs` (default 1), `--retained-requests`
   (default 16), and `--max-resource-bytes` (default 65536) deployment bounds.
   It admits the canonical `-C`/current root and the startup-effective
   selections, plus any reviewed `--allow-root`, `--allow-model` and
   `--allow-mode` alternatives, through the ordinary rooted run controller; status and sanitized resources remain connection
   scoped.
3. **Sessions** — on-disk store, versioned codec with one-way migrations,
   catalogue, resume and command replay; final M3 adds fork lineage and
   replay, physical run evidence admitted before effects, and a rebuildable
   manifest. No final invocation may use `--ephemeral`: it is refused before
   any effect because reproducible evidence must be durable.
4. **Subagents, skills, hooks** — isolated sub-loops; strict system/user/trusted
   project `SKILL.md` files with current compatible frontmatter, progressive
   model announcement and typed user/model invocation; the current stable
   subset of εgo-owned lifecycle hooks in the context shape that other agent CLIs share. Hooks are subtractive
   gates over one `Hook` port: command, HTTP, MCP-tool, and prompt adapters reuse
   their real execution, egress, foreign-tool, and generation owners, and
   unsupported, deprecated, or experimental names fail admission explicitly.
   Their admitted source/digest and typed start/settlement evidence are durable.
   Skills reuse the one permission/sandbox/budget path, never self-grant or
   execute dynamic shell context, and persist rendered content/digests for
   resume and compaction. See `docs/internals/20-agent-skills.md` and
   the hook compatibility analysis. External memory integrations use generic hooks; ego does not embed a vendor bridge.

This is a large v1. Sequencing does not defer any of the
four to "v2" as a matter of design — the architecture must have a
place for each from day one.

### 2.5 Interactive product contract — *complete, linear, keyboard-first*

The compact CLI is not a reduced CLI. The primary terminal buffer remains the
transcript and the terminal retains scrolling, selection, and copy; ego does
not enter the alternate screen or capture mouse events. Over that transcript,
one interactive surface owns a live multiline composer, a configurable native
status line, slash-command and action menus, model and reasoning selection,
structured questions, compact permission dialogs (command and risk in plain
words, detail behind a key), draft stash, queued prompts, and
safe steering between model interactions.

The same action backs every spelling: `Option-P`/`Alt-P` and `/model` open one
model picker, slash completion and direct shortcuts dispatch one registry, and
interactive questions map to explicit ACP and plain non-interactive forms. The
picker, `ego models`, and generation-time resolution consume one current-model
catalogue assembled from supplied hosted facts and authoritative provider or
local discovery; the UI owns no model list. Clean presentation may collapse
detail by terminal width; it may not remove a capability or create a second
implementation of it.

The composer remains usable while the harness generates and runs tools.
`Enter` on non-empty input during a busy regular turn durably accepts a FIFO
pending steer immediately. It is delivered only after provider sampling settles
and the complete sibling tool-call/result batch from that generation is durably
closed, before the next model request. A final response with no tool still
forces the follow-up needed to deliver pending input. `Esc` expedites the FIFO
prefix through a durable tail marker, requests phase-safe interruption, and
replays unacknowledged input exactly once as one fresh user turn at the earliest
safe durable boundary. It never splits a call from its result or reports a
non-cancellable side effect as interrupted before it truthfully finishes.

`Ctrl-S` keeps a session-local LIFO draft stash without entering conversation
history. Accepted pending steers are durable FIFO state for a busy regular turn;
ordinary durable queued prompts exist only at non-steerable or idle-transition
boundaries. One coordinator owns both accepted forms; an action registry never
has another queue or steering inbox.

`/compact` is a prompted model handoff, not transcript truncation. At a safe
idle boundary its versioned internal prompt reads the complete active context
and produces one self-contained checkpoint preserving everything material to
continuing the work. Success creates a new durable session whose only carried
conversation artifact is that checkpoint; ordinary system, project, runtime,
model, and tool context is resolved afresh. The old session remains complete
and resumable. No old turn, retained tail, queued prompt, or draft is silently
copied into the new session. The checkpoint-only target baseline must pass the
ordinary capability and wire-expressibility checks at compaction time. This is
operational admission, not an exact token-fit guarantee for hidden provider
transformations or the later first human prompt. Final validation exercises a
practical size sweep of canonical checkpoint baselines and preserves observed
provider limits as evidence rather than silently turning them into product
guarantees.

### 2.6 Final M5 closure — *one product, no reduced release*

M5 completes every ratified surface: the original 17 baseline commands, the original 24 selected
interactive actions, generated reference/completions from the ratified parser,
durable config/rules, session display metadata and one persistent-goal
lifecycle, a unified non-mutating redacted doctor, delivery-safe retry/fallback,
one zeroizing provider-credential lifetime, the two current public provider
contracts in §2.2, and the selected ACP v1 surface in §2.1. Its phases order
implementation and validation only.

`--retry N` permits at most N additional attempts on the same resolved model
after an eligible transient failure. Without an explicit `--retry`, interactive
and ACP sessions use a 24-hour monotonic recovery horizon starting at the first
eligible failure; headless runs default to no retries. Explicit `--retry N`,
including zero, selects a finite allowance instead. Recovery does not reset the
horizon when an operator forces a retry or the coordinator changes candidates.
Backoff honours `Retry-After`. Visible reasoning and unknown-effect provider
content stop retries; opaque reasoning with parsed tools proven unrun permits at
most one recovery per candidate generation within the same allowance. Interactive
and ACP seats also permit one narration-only recovery per candidate generation,
with no parsed tools. Opaque reasoning state may accompany that narration. The
partial text is marked interrupted and is not appended to the retry request.
Headless visible text still stops retries.
Ordered fallbacks re-resolve current facts and start a fresh attempt only when
tools, images, schemas, limits, and reasoning state remain expressible; any
possibly-sent attempt stops fallback. Cancellation, deadlines, and budget checks
remain authoritative. Each attempt remains separately durable and accounted.
See `CLI.md` §3 for delay bounds, force controls, and delivery-safety details.

Display names and goals are ordered session events. Their slash spellings and
metadata-only `ego resume` projections are fixed in
`docs/internals/decisions/2026-09-03-session-metadata-and-goals-are-durable-state.md`.
Native `/goal stop` and admitted model cancellation use a durable `cancelled`
state, distinct from achievement and blocking. Stop is serviced while generation
or goal checking is busy, without killing admitted tools, children or the session.
Model cancellation names the exact model-owned goal identity; operator goals remain
under operator authority. Goal-check results and continuation admission are
serialized against cancellation on the shared journal; stale work cannot reactivate
or continue a terminal goal. Objective, ownership, history and worklist are preserved.
Operator stop also pauses goal, worklist and report continuation and model goal
creation for the admitted request, in every cancellation window; a new user request
or an explicit operator goal releases the pause. Model cancellation does not pause.
Worklist execution state is admitted atomically: at most one direct action is
active, and extra active rows need actual host-owned live task bindings. Started
identities cannot return to pending through suspension, replacement, removal,
fork or compaction. Suspension retains its cause. Recorded history is folded
without re-admitting it, and interrupted process ownership never becomes a live
binding in a reopened session.
Goal completion is an explicit authority attestation; blocked status requires
three consecutive observations of the same condition and resets on progress.
Doctor and config-editor effects are fixed by the accepted 2026-09-03 doctor
ADR and the amended atomic-config ADR. None of these final requirements is
reported as currently available before its implementation story lands.

## 3. Product non-goals

- WASM / N-API embedding (not needed yet).
- Image generation. Image input through the shipped `--attach` path, and
  through the interactive seat's `Ctrl+V`/bracketed-path pasteboard staging
  onto that same durable path, remains in scope
  (`docs/internals/decisions/2026-09-29-interactive-image-paste.md`).
- A first-party GUI. The separate-repository TUICommander is a required M5 ACP
  host, not a shared-code frontend.
- Backward compatibility with other agents’ session files or config.

## 4. Open questions for the architecture phase

**All eight resolved on 2026-08-19** — decisions and rationale in
`ARCHITECTURE.md` (§2 crates, §4 async, §3.2 tools, §3.4 sandbox, §5 storage,
§6 terminal, §7 + `docs/internals/06-acp.md` ACP coverage, §8 sequencing). Kept below as
the original problem statement:

1. **Crate boundaries.** How many crates, and where the seams go.
2. **Async model.** Tokio throughout, or a sync core with async only at the
   edges. Affects how testable the core is and how easily tuicommander can host it.
3. **Tool trait design.** Static dispatch with a registry macro versus
   `Box<dyn Tool>`. Interacts with MCP tools, which are dynamic by nature.
4. **Sandbox strategy per platform.** macOS `sandbox-exec` (deprecated but
   working) versus `landlock` on Linux versus process-level restrictions only.
   Evaluate the implementation and maintenance cost.
5. **Session storage format.** JSON-lines versus SQLite (`rusqlite` is already a
   tuicommander dependency). Replay and migration requirements push toward the
   latter; portability and diffability push toward the former.
6. **TUI approach.** `ratatui` + `reedline`, or a line-oriented renderer closer
   to shell output rather than a full-screen interface.
7. **ACP coverage for the tuicommander host.** Which of tuicommander's needs —
   session catalogue and switching, model selection, config, usage and cost —
   are expressible in ACP v1, which need extension methods, and which stay
   outside the protocol. Requires reading the ACP schema, not the README.
8. **Sequencing.** Which of the four v1 blocks lands first, and what the first
   demonstrable milestone is.

## 5. Constraints

- Rust, stable toolchain.
- SSH-only git transport.
- Apache-2.0. Attribute any verbatim third-party material as required by its
  license in `NOTICE`.
- English in code and documents.

## 6. Grok as a provider

Added 2026-08-21 and superseded by the current-contract amendment completed in
story `037-db12`. The historical API-key investigation remains in
`docs/internals/11-grok-subscription.md` and
`docs/internals/decisions/2026-08-21-grok-as-a-provider.md`; the current wire audit is
`docs/internals/15-provider-contract-audit.md` §8.

### 6.1 Protocol — current xAI Responses, not legacy Chat Completions

The selectable row is `xai-oauth`. It sends streamed `POST /v1/responses`
through the dedicated `xai-responses` mapping. It is not an adaptation of
`openai-completions` or `openai-codex-responses`: request fields, reasoning
levels, image projection, event vocabulary, usage, and failure ownership come
from xAI's current public contract.

Story `087-5c3d` removed the API-key `ProviderRow::xai()`, its deprecated Chat
Completions path, credential strategy, published-rate resolver, and
xAI-specific negative-selection branch from product code. The dated audit
retains the investigation as archival evidence only. None of it is a fallback
for `xai-oauth`.

`xai-responses` sends `reasoning: { effort }` and accepts only the current
model's stated set. For the supplied `grok-4.6` contract that is `low`,
`medium`, `high`, and `xhigh`; every other `ThinkingLevel` refuses before the
request is built. Text, client function tools, JPEG/PNG image input, streaming,
usage, terminal errors, and in-band errors have their own deterministic
fixtures.

### 6.2 Authentication — the selected subscription row owns OAuth

`CredentialStrategy::OAuth { provider: "xai-oauth" }` reads the admitted token
from the ordinary subscription-login store. Login, refresh, revocation, and
tier failures use the shared credential lifecycle described in
`docs/internals/16-subscription-login.md`; provider-specific code does not read another
application's token file or add a legacy API-key fallback.

The final build has no `XAI_API_KEY` strategy or API-key xAI profile.

### 6.3 Metering — subscription truth, provider cost telemetry

The selected row resolves with subscription metering, so ego neither invents a
per-token subscription price nor interprets included quota as zero cost. The
current Responses decoder may retain xAI's `usage.cost_in_usd_ticks` telemetry,
but that observation does not replace the model's subscription metering or
authorize API-key pricing.

The dated audit retains archival published-rate findings from
`GET /v1/language-models`: per-key prices, long-context tiers, zero sentinels,
fingerprint, and version. Story `087-5c3d` removed their runtime resolver and
the dead `RateSource::ProviderPublished`; those facts cannot change the current
subscription row's billing contract. `XaiUsdTicks` remains solely as the
current Responses decoder for post-response cost telemetry.

## Conversation fork cache prefix

The accepted decision in
`docs/internals/decisions/2026-09-30-conversation-fork-cache-prefix.md` defines a durable
system-only prompt snapshot per epoch, inherited by resume and fork. Compaction
or explicit refresh opens a new epoch; tool/MCP admission stays current on each
generation. Volatile host data belongs before current user text and is never
persisted. Provider cache hits require usage evidence.

CLI `ego fork ID --at ITEM_ID` and REPL `/fork [message-id]` already select a
successful completed-turn boundary. The durable ACP adapter serves standard tip
fork and lifecycle operations, plus the negotiated `_meta.ego.atMessageId`
selector, lineage metadata, and inherited-message projection. Sources:
`crates/ego-acp/src/serve.rs`, `crates/ego-acp/src/project.rs`, and
`crates/ego-acp/tests/it/session_lifecycle.rs`. System snapshots are captured in existing generation events and inherited in
fork seeds. `/refresh` reloads instructions for the next admitted generation.
Subscription transport continuation is shipped in
the session transport; it does not prove provider cache parity
or latency improvement. See [providers](docs/guides/providers.md#subscription-transport).
