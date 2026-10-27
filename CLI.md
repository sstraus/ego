# ego — Command language

Date: 2026-08-19; M5 contract freeze: 2026-09-03. Status: **§4 (output
contracts) ratified by maintainer 2026-08-21 — §4.1 through §4.5 in full, not a
subset.** regression case freezes the
M5 amendments in this document; unaffected previously proposed prose remains
proposed.



## 1. Design principles

1. **The common case costs zero ceremony.** `ego "fix the failing test"` — no
   subcommand, no flags.
2. **stdin/stdout are the API.** Prompt from argv, stdin, or file; events out as
   JSONL; meaningful exit codes. `git diff | ego run "review this" | jq` works.
3. **TTY-aware defaults, explicit overrides.** On a pipe: no colour, no spinner,
   quiet progress. Output *format* never changes implicitly — a script that
   wants JSON says so.
4. **One grammar everywhere.** The permission rule you pass as `--allow`, write
   in config, and see in `/permissions` is the same string. The event you see in
   `--output jsonl` is the same event in the session file and on ACP.
5. **Every interactive capability has a non-interactive spelling.** Nothing is
   locked behind the REPL (Codex's `exec` lesson; pi's `print`/`json` modes).

<!-- ego-cli-command-set:start -->
## 2. Command set

```
ego [PROMPT]                  interactive session (prompt submitted if given)
ego run [PROMPT]              headless one-shot; PROMPT from argv or stdin; -f FILE loads an agent file
ego continue [ID] [PROMPT]    alias of resume; same session selection and options
ego resume [ID] [PROMPT]      continue a session; no ID = interactive picker or headless latest;
                              --answer QUESTION_ID=JSON answers pending input;
                              --name and --goal-operation are metadata-only operations
ego ls                        list sessions (this workspace; --all for every one)
ego rm <ID> [--yes] [--force]  tombstone one session; confirm unless --yes;
                              --force acknowledges an uncertain run
ego fork <ID> [--at ITEM_ID]   branch after a completed turn
ego checkpoint [-C DIR] <ID> preview|restore COMMIT|close
                              inspect or selectively undo builtin file edits
ego compact <ID> [--request-id UUID] [--budget LIMIT] [--max-output-tokens N]
                              publish one idempotent checkpoint target
ego export <ID> [--sanitize] [--format md|json|jsonl|transcript] [--file PATH]
                [--follow] [--after SEQ]
                [-C DIR] [--session-dir DIR]
                              render a session; Markdown is the default
ego mcp add NAME --stdio [--cwd DIR] [--inherit-env NAME]... -- COMMAND [ARG]...
        add NAME --http URL [--profile current|legacy-sse] [--auth none|oauth] [--allow-insecure-loopback]
        rm NAME | ls [--json] | login NAME | reload [NAME]
                              shared configuration and live admission service;
                              login NAME --client-id ID | --cimd URL | --dcr
ego mcp-server --max-concurrent-runs N --retained-requests N
               --max-resource-bytes N [-C DIR] [--session-dir DIR]
                              serve ego AS an MCP server on stdio: scoped
                              run/continue/cancel/status tools, plus M3 compact
                              (delegation surface for Claude, Codex, CI) plus
                              sanitized resources; no MCP Tasks or prompts
ego auth   login|logout|status [PROVIDER]
ego models [--provider P] [--refresh] [--json]
ego hooks  ls [--json]                     read-only admitted hook catalogue
ego config capabilities|snapshot|batch|get|set|unset|ls|edit
ego roots  add|rm|ls                         user perimeter roots
ego rules  ls|allow|deny|revoke|remember|explain   (permission rules,
                              stable IDs; `explain <tool> <args>` shows the
                              canonical intent + matching rule, dry)
ego usage  [--period P] [--all-roles] [--json] cost and token report
ego acp [--tools LIST | --no-tools] [--allow RULE] [--deny RULE]
        [--profile NAME] [--append-system TEXT|@file] [--output-style NAME]
        [--instructions FILE]
        [--listen ENDPOINT [--allow-public-bind]
         [--max-connections N] [--connection-lifetime DURATION]
         [--max-ingress-bytes N]]
                              serve ACP on stdio by default, or on an explicit
                              endpoint. The seven session flags are §3's
                              general session shape and mean here exactly what
                              they mean for `ego run`; naming none of them is
                              the unshaped session. Listener controls default
                              to 32 connections, 24h, and 134217728 cumulative
                              ingress bytes per connection. TCP endpoints need
                              EGO_ACP_TOKEN; local endpoints use operating-
                              system peer identity and never read that token.
                              Public addresses additionally require
                              --allow-public-bind and cap `off` confinement at
                              `workspace`.
ego a2a --listen ENDPOINT [--allow-public-bind] [--sandbox LEVEL]
                              serve Agent2Agent discovery information.
                              ENDPOINT is a socket path —
                              authenticated by the operating system, the
                              preferred form — or an address, which requires a
                              credential in EGO_A2A_TOKEN. An address reachable
                              from outside this machine is refused by name
                              unless the operator asks for it
ego worktree show|cleanup RECEIPT_UUID   inspect or safely remove a retained local checkout
ego orchestrator brief [--root PATH]  read-only TUIC children and unread results
ego doctor [--json]           read-only redacted health snapshot
ego setup [PROMPT] [--profile NAME] [--directory PATH]
                             transient read-only setup inventory
```

Notes:

- `ego run` is the scripting workhorse. `ego resume <id> "next step"` chains
  scripted multi-turn conversations — a pipeline can carry a session through
  several stages. One session pool across interactive and headless runs
  (Codex split them and had to add `--include-non-interactive` to re-merge).
- For an installation-free IDE custom-agent setup over native ACP, see
  [IDE setup](docs/integrations/ide.md); no `--ide` auto-connect flag is required.
- For standard ACP client libraries and runnable isolated examples, see
  [ACP client libraries](docs/integrations/acp-clients.md).
- There is deliberately **no bespoke RPC mode**: embedders get `ego acp`.
  pi invented `rpc`/`json` modes; ACP is that, standardised.
  Headless multi-turn streaming uses this same ACP stdio connection, not an
  `--input-format stream-json` selector. See [the headless transcript](docs/integrations/acp.md#headless-multi-turn-stdio)
  for framing, attachments, cancellation, and permission answers.
- An ACP host may supply a stateless HTTP or explicit SSE MCP server in
  `session/new`. The ACP `sse` variant selects the legacy profile; `http`
  stays current-only. HTTPS
  remains the default. Plain HTTP is admitted only for the exact loopback
  hosts `localhost`, an IPv4 loopback literal, or an IPv6 loopback literal;
  other plaintext hosts are refused. The host does not send ego's
  configuration-only `allow_insecure_loopback` field: supplying the exact
  loopback URL is the explicit session-scoped request.
- Stdin contract (Codex's, the cleanest surveyed): prompt as argument; `-`
  reads the prompt from stdin; with *both* argv prompt and piped stdin, stdin
  is appended as a delimited context block.
- `ego export --sanitize` emits the producer-only version-1 structural bundle;
  unsanitized export is an explicit local transcript with no safety claim.
  `--format transcript` emits [local transcript v1](docs/reference/transcript.md),
  with stable event identity, lineage, goals, `--follow`, and `--after SEQ` replay.
  Transcript mode excludes `--sanitize` and `--file`.
  Canonical JSON ends in one newline, JSONL is the same single bundle line, and
  Markdown embeds that same bundle. `--file` suppresses stdout; JSONL with a
  file is a usage error. Relative files resolve from canonical `-C`. Existing
  identical bytes succeed without rewrite, different bytes conflict, and a
  published-but-durability-uncertain file remains visible and is not retried.
- Metadata-only session operations reuse `resume` and never start a model turn:
  `ego resume ID --name show|set=TEXT|clear [--output text|json|jsonl]` and
  `ego resume ID --goal-operation show|set=TEXT|achieve|block=REASON` with optional
  `--output text|json|jsonl`. They do not accept a prompt or `--answer` in the
  same invocation.

### Headless ACP stdio example

Launch `ego acp`. Each line below is a separate host-to-agent JSON-RPC frame,
not one batch to paste into a pipe. Wait for initialization and session creation;
replace `SESSION` with the returned `sessionId`. Wait for prompt 3 to settle
before sending prompt 4. While prompt 4 runs, the host can answer an incoming
permission request and send cancellation:

```jsonl
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/workspace/repo","mcpServers":[]}}
{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":"SESSION","prompt":[{"type":"text","text":"Summarize this repository"}]}}
{"jsonrpc":"2.0","id":4,"method":"session/prompt","params":{"sessionId":"SESSION","prompt":[{"type":"text","text":"Check the summary"}]}}
{"jsonrpc":"2.0","id":"PERMISSION_ID","result":{"outcome":{"outcome":"selected","optionId":"allow-once"}}}
{"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"SESSION"}}
```

The permission response is conditional: copy the exact ID (including its JSON
type) from the agent's `session/request_permission`, and select only an offered
option. Do not send the placeholder literally. Cancellation is a notification;
if it cancels the active run, the pending prompt response reports
`stopReason: "cancelled"` (a run that already finished may return `end_turn`).
Read stdout continuously for responses, `session/update` notifications, and agent requests.
Close stdin after pending work settles; EOF ends the connection, not a user
turn. `ego run --output jsonl` remains one-shot event output, not this input
protocol. The [integration guide](docs/integrations/acp.md#headless-multi-turn-stdio)
shows both directions, attachment blocks, and flow control.

`-f` / `--file` loads a strict version-1 agent file, including its invocation
settings and declared template variables. It is not a plain-text prompt-file
reader. For example, save this as `reviewer.md`:

```markdown
---
version: 1
---
Review the current change for defects.
```

Then run `ego run --file reviewer.md`. Use `ego run -` to read a plain-text
prompt from stdin. An argv prompt cannot be combined with an agent file.

### 2.1 Availability — what ships today

The current command inventory comes from `BUILT` and `UNBUILT` in
`crates/ego-cli/src/cli.rs`. All commands in the current `BUILT` table are
implemented; `UNBUILT` is empty. Tests bind the command set below to those
tables. If an unavailable command is added, its refusal must name the command
and the subsystem it waits on. Availability does not remove a required
capability from the final product.

A word that is **not** on this list stays a prompt: §2's first line is
`ego [PROMPT]`, so `ego "explain this failing test"` is the interactive form
and opens a session with that as its first turn. `ego -- resume` likewise
prompts the word rather than running the command.

**The reason in each cell below is the string the binary prints**, not a
paraphrase of it, and a test asserts that character for character (modulo
whitespace). This column drifted from the code within an hour of being written
— `ego rules` was still blaming the permission rule grammar after the grammar
shipped — because the first test bound only the command *name* to §2 and left
the reason free on both sides. Two prose surfaces that agree today, with
nothing that can say when they stop, are the same defect as a field that is
omitted when empty: two states, identical bytes.

| Command | Current | Status |
|---|---|---|
| `ego run` | **yes** | the scripting workhorse; the whole of §4 is about it |
| `ego [PROMPT]` | **yes** | asks once whether to trust project files before ambient reads, then runs the line-oriented session. Permission questions accept `allow`, `deny`, or `amend`; amended raw JSON parameters are prepared and classified again from the beginning |
| `ego continue` | **yes** | resumes a saved session through the same admission policy and options as `ego resume` |
| `ego resume` | **yes** | continues a session that already exists, never creating one. `[ID]` is optional: without it a human terminal shows the session picker across roots; headless execution takes the most recently updated session of this workspace. With terminal stdin and stdout, a positional prompt is submitted once to the interactive driver, which accepts further messages. Explicit `--output` (including `text`), stdin `-`, `--answer`, an agent file, or profile `ci` selects headless execution. Redirected stdin or stdout also selects headless execution |
| `ego ls` | **yes** | this workspace by default, `--all` for the whole store; `--output json\|jsonl` per §4.6 |
| `ego fork` | **yes** | creates a durable lineage branch at the idle tip or after the completed turn containing `--at ITEM_ID`, without copying source spend or run state |
| `ego compact` | **yes** | publishes one idempotent clean target from the idle compaction operation and supports later continuation; `--request-id` optionally reuses an existing ID, otherwise the parser generates one |
| `ego export` | **yes** | local transcript export plus the producer-only version-1 structural bundle under `--sanitize`; Markdown, JSON, JSONL, stdout, and atomic file publication follow §2.1 |
| `ego mcp` | **yes** | `add`, `rm`, `ls [--json]`, and `reload [NAME]` use one live admitted registry and truthful health projection. `login NAME --client-id ID`, `login NAME --cimd URL`, or `login NAME --dcr` selects an explicit public-client registration strategy, prints a copyable authorization URL, and waits up to 120 seconds on an exact loopback callback; `--callback-port PORT` selects a registered callback port |
| `ego mcp-server` | **yes** | serves scoped `run`/`continue`/`cancel`/`status`/`compact` tools and sanitized summary/output resources on stdio. `--max-concurrent-runs`, `--retained-requests`, and `--max-resource-bytes` are required non-zero deployment bounds; `-C`/the current directory is the sole canonical root and stdout remains protocol-only |
| `ego auth` | **yes** | `login\|logout\|status [PROVIDER]`; credentials live in one locked `$EGO_HOME/auth.json`, while TOML commands and discovered auth plugins can supply login tokens |
| `ego models` | **yes** | prints the shared current-model catalogue as deterministic text or versioned JSON; `--provider` filters both model and source rows, and `--refresh` updates authoritative discovered slices without promoting cached display facts into generation authority |
| `ego orchestrator` | **yes** | `brief [--root PATH]` inspects tracked TUIC children and unread results; requires TUIC_SOCKET; no generation or inbox reads |
| `ego hooks` | **yes** | `ls [--json]` reads the same immutable admitted hook catalogue used by execution; it neither reloads nor edits hook policy |
| `ego config` | **yes** | `get`, `set`, `unset`, `ls`, and whole-document `edit` use one strict atomic user-config transaction; stored profiles are explicit targets and management ignores ambient `EGO_PROFILE` |
| `ego rules` | **yes** | `ls`, `allow`, `deny`, `revoke`, `remember`, and `explain` share the canonical rule grammar, atomic user store, and effective policy service; management ignores ambient `EGO_PROFILE` and never executes the explained call |
| `ego usage` | **yes** | reports the versioned main/judge/compaction/total accounting DTO for this workspace; `--period` accepts `all` or a positive `s`/`m`/`h` duration and `--json` emits the same DTO. The text view is a compact `session model role in out cached cache_hit cost` table that hides role rows with no activity unless `--all-roles` is given; the flag is presentation only and never reaches the JSON report |
| `ego acp` | **yes** | the Agent Client Protocol on stdio, v1. A host's `session/new` mints an ordinary ego session and its `session/prompt` runs an ordinary turn, so what it opens is listed by `ego ls` and continued by `ego resume`. v1 and not the v2 draft: see `ARCHITECTURE.md` §6.2 |
| `ego doctor` | **yes** | one ordered, versioned, redacted `HealthSnapshot` over nine sections, rendered as text or `--json`. It reads existing facts only: it never logs in, refreshes a credential or a catalogue, reloads an extension, edits config, or calls a hosted provider. `/doctor` renders the same snapshot, and a fact ego cannot observe without mutating something is reported `not_checked` with its reason rather than omitted |

All four M0b demonstrations now have an entry point (`ARCHITECTURE.md` §8): an
interactive session, `ego run … | jq`, `kill → ego resume`, and the same
session over ACP. This is availability, not a release boundary; later
milestones remain dependencies of the final product.

### 2.2 Configuration and rule management grammar

The final root grammar is exact:

```text
ego config capabilities
ego config snapshot [--profile NAME]
ego config batch [--profile NAME] [--set KEY=TOML_VALUE]...
                 [--unset KEY]... [--if-revision REVISION]
ego config get   [--profile NAME] [--json] KEY
ego config set   [--profile NAME] KEY=TOML_VALUE
ego config unset [--profile NAME] KEY
ego config ls    [--profile NAME] [--effective] [--json]
ego config edit

ego rules ls       [--profile NAME] [--json]
ego rules allow    [--profile NAME] RULE
ego rules deny     [--profile NAME] RULE
ego rules revoke   [--profile NAME] RULE_ID
ego rules remember [--profile NAME] [-C DIR] [--yes] allow|deny TOOL ARGS_JSON
ego rules explain  [--profile NAME] [-C DIR] [--json] TOOL ARGS_JSON
```

These management commands never infer a target from `EGO_PROFILE`. No profile
means the base user document; explicit `--profile NAME` means exactly that
user profile. Mutations never write system, project, project-local,
environment, agent-file, or flags authority. `rules ls` and `rules explain`
read system plus the selected user authority in bare posture; they do not
ambiently trust the current directory.

`NAME` is one to 64 UTF-8 bytes, contains no ASCII control character, and has
no leading or trailing Unicode whitespace. For `config`, a missing profile
lists empty, `get` reports absence, `set` creates it, and `unset` is unchanged
success. For `rules`, the selected user contribution is empty but effective
list/explain still includes applicable system rules.

`config set` accepts one assignment and parses its value as TOML. The closed
managed key set and writable scopes are listed below. `PROVIDER` stands for a
provider-qualified authentication key, not a literal provider name. The table
is checked against `ManagedConfigKey`; it is not a separate runtime allowlist.

<!-- ego-managed-config-keys:start -->
| Key | User base | User profile | ci profile |
|---|---|---|---|
| `model` | yes | yes | yes |
| `thinking` | yes | yes | yes |
| `mode` | yes | yes | yes |
| `sandbox` | yes | yes | yes |
| `roots` | yes | yes | yes |
| `exec_paths` | yes | yes | yes |
| `exec.env` | yes | yes | yes |
| `exec.env_allowlist` | yes | yes | yes |
| `network` | yes | yes | yes |
| `budget` | yes | yes | yes |
| `bare` | yes | no | no |
| `permissions.judge` | yes | yes | yes |
| `ui.statusline.template` | yes | no | no |
| `ui.keybindings` | yes | no | no |
| `ui.vim` | yes | no | no |
| `session_dir` | no | no | yes |
| `artifact_dir` | no | no | yes |
| `auth.PROVIDER.headers` | yes | no | no |
| `auth.PROVIDER.credential_command` | yes | no | no |
<!-- ego-managed-config-keys:end -->

Permission arrays, interactive authentication login, MCP, and hooks retain their
dedicated commands. `unset` is the only null operation and prunes empty managed
tables. There are no table fragments, list indices, aliases, or arbitrary dotted
keys. `config capabilities` reports transaction support; `snapshot` reads a
managed document revision and `batch` applies its bounded edits atomically,
optionally conditional on `--if-revision`. These commands do not change which
authority owns a key.

The standalone runner also accepts `exec_paths = ["/absolute/tools/root"]`
in `~/.ego/config.toml` (or `$EGO_HOME/config.toml`), selected user/system
profiles, and explicit `-c` overrides. Use `ego config set exec_paths='["/absolute/tools/root"]'`
for a managed user edit, or edit the TOML with `ego config edit`. Higher layers replace the whole list;
an explicit empty list clears configured roots. Project and project-local
configuration cannot add exec roots. Paths must be absolute and must exist;
the runner canonicalizes symlinks before creating the confinement policy.
On macOS and Linux these roots add read and execute access, never write access.
On macOS the inherited operator PATH remains unchanged. The sandbox permits
metadata lookup in those PATH directories and for their captured symlink targets,
so a tool outside the exec perimeter is found but its execution is denied. This
metadata grant does not permit file contents, directory listing, writes, or execution.
A declared MCP server does not add exec authority. If its executable is outside
the perimeter, startup names its canonical path and asks you to add that exact path
to `exec_paths`, then exit and resume. If a read-only root denies execution, startup
names that root instead: change or remove its read-only restriction, then exit and
resume. Adding `exec_paths` cannot override a read-only root, including under
adaptive confinement. MCP diagnostics do not expose child stderr,
arguments, or environment values.
On macOS an installed `/opt/homebrew` is also a default runtime root, including
its `bin` symlinks, Cellar targets and libraries. Other installation prefixes
must be listed explicitly. Git can also read existing `~/.gitconfig` and
`$XDG_CONFIG_HOME/git/config` (default `~/.config/git/config`) on macOS; this
does not grant access to the rest of the home directory. Writes remain limited by `--sandbox`: workspace
and private scratch for `workspace`, no writes for `ro`.

`config get|ls --json` emits
`{"v":1,"scope":"user","profile":string|null,"values":{...}}`; JSON
values preserve their types and keys use canonical dotted spelling. Text uses
canonical TOML values: `get` prints only value plus newline and `ls` prints
escaped `KEY\tVALUE` rows in closed-schema order. A missing `get` writes no
stdout, reports the key on stderr, and exits 1; an empty `ls` succeeds.
`set` and `unset` print one plain line: `Saved KEY = VALUE in TARGET.`,
`Removed KEY from TARGET.`, or `Unchanged: ...` when nothing changed. Unknown
keys fail with a usage error that names the key. `edit` and `rules` keep their
tab-separated receipts, which carry the publication state. Text fields escape
backslash, tab, carriage return, and newline.

`rules allow|deny` stores the canonical AST in the existing user
`permissions` arrays. A same-effect duplicate is an unchanged success; the
opposite effect is a conflict. `revoke` removes the ID from both effects in the
selected user target; an absent ID is unchanged success, a future grammar ID
is exit 2, and a collision is exit 1. Copies in other authorities survive and
are reported. A permissions table is a whole-block override: creating one may
shadow that layer's base judge/lists or lower-layer allow/judge values; system
denies remain binding. Receipts report stored and effective change separately.
Credential reads have built-in `default` deny rules for `.env*`, `.ssh/**`,
`.aws/**`, `.netrc`, `Keychains/**`, `*.keychain`, and `*.keychain-db`, at any
depth in the workspace or additional roots. These rules remain when a
permissions table replaces lower layers. An effective explicit user allow,
for example `ego rules allow 'read(/absolute/project/.env)'`, lifts the preset
for matching reads only. Project, local, agent-file, and flag allows do not
lift it; explicit denies from any file layer still win, including immutable
system denies. Revoking the user allow restores the default denial. The
existing path grammar is case-sensitive and matches canonical targets.
These are `read` permission defaults: they do not mask re-allowed content,
filter `grep` results, or confine subprocess reads. Key-only projection is
separate work.

For `allow`, `deny`, and `revoke`, the rule ADR fixes the one-line receipt to
eight escaped tab-separated fields:
stored change, effective change, effect, ID, canonical rule, target, surviving
or shadowing authorities, and publication state.

`rules ls --json` emits
`{"v":1,"scope":"effective","profile":string|null,"rules":[...]}`.
Each row has `id`, `effect`, canonical `rule`, `from`, and `also`; text is
`ID EFFECT RULE FROM ALSO`, tab-separated. `rules explain` prepares and
matches the call but returns the exact version-1 schema and deterministic line
order in the rule ADR: redacted ordered operations, outcome
`allowed|denied|uncovered`, matching rule authorities, uncovered operation
indices, `settlement: not_run`, and `would_execute: false`. Every match names
its operation index and complete `from`/`also` origin set; deny returns the
first matching deny, otherwise each covered operation returns its first allow.
It never invokes
discovery, a mode, approver, judge, human, settlement, or executor.

Standalone rule preparation accepts exactly one JSON object and the static
effect built-ins `read`, `glob`, `grep`, `write`, `edit`, `exec`, and
`read_tool_result`; the last is explainable but not rememberable. It never
discovers skills or foreign tools.

`rules remember` derives the narrowest rule the existing grammar can express,
not an exact invocation. Its preview names every loss: list/search omit
pattern/query; write omits its change payload; spawn omits
cwd/timeout/environment; ordinary connect omits request digest; opaque omits
parameter digest/transport. Protocol-connect retains its exact authority.
Without `--yes`, it previews and publishes nothing. `--yes` accepts that
widening and performs the atomic mutation; `/permissions` uses the same preview
with modal confirmation.
Session-evidence reads require no rule and secret egress is not persistible.
The rule ADR fixes its output as ordered `DERIVED`/`OMITTED` rows followed by
one `RESULT` row containing stored change, effective change, target, and
publication state; this replaces the singular-rule receipt for this command.

<!-- ego-cli-command-set:end -->
## 3. Flags (the argument grammar)

Grouped; all long flags, short forms only for the hot four
(`-m -q -C -f`). `-v`/`--verbose` has no ratified semantics and is not part of
the final grammar.

**Model and generation**

```
-m, --model P/M         provider/model slug: anthropic/claude-opus-5,
                        openrouter/z-ai/glm-4.7, ollama/qwen3 …
                        local daemons (ollama, lmstudio, vllm, llamacpp) are
                        first-class providers, catalogued live from the daemon
    --offline           no non-loopback egress from any owned exit:
                        loopback addresses and `localhost` only, no DNS, no
                        proxy, and every redirect hop re-checked. Provider HTTP,
                        MCP, hooks, and spawned commands use their guarded
                        transport or demonstrated no-IP confinement. It cannot
                        stop an admitted local daemon from relaying upstream
                        itself
    --thinking L        off|minimal|low|medium|high|xhigh|max   (pi's scale)
    --system TEXT|@file          replace system prompt
    --append-system TEXT|@file   append to it
    --output-style NAME         select a trusted prompt-only response style
```

**Permissions and confinement** (one axis pair, Codex-style)

```
    --mode M            plan|default|auto|accept-edits|yolo   shortcuts: --plan, --yolo
                        trusted interactive sessions default to auto when no
                        mode is configured; deterministic low/bounded calls
                        need no judge, while unknown calls may use the
                        configured judge and otherwise escalate
    --sandbox S         ro|workspace|off             (off = classifier only)
    --allow RULE        repeatable   e.g. --allow 'exec(git *)' --allow 'net(api.github.com)'
    --deny RULE         repeatable; deny beats allow
```

Auto analysis reuses an opinion only for the same executable content and
identity, interpreter/script observations, exact arguments, captured cwd and
environment, workspace trust/boundary, generation policy, confinement and
measured backend capabilities, and resolved judge/prompt revision. Risk is
recomputed on every call: deterministic low/bounded native operations settle
without a paid judge; high requires human judgment; unknown stays unknown
even when the judge allows it. Dynamic shell/import/package behavior is never
safe merely because of its executable name. The judge receives argument
lengths/digests rather than source or environment values.

Analysis lives outside the workspace and transcript at
`$EGO_HOME/permission-analysis/v1.json` (private 0700 directory, 0600 file on
Unix; at most 1024 entries and 4 MiB). Hits do not write or generate a new judge
opinion. Cache failures are misses with diagnostics; an opinion never grants
a persistent rule or bypasses a new deny, plan mode or the workspace boundary.
Deleting this advisory document only causes future analysis to run again.
Incomplete identity cannot reuse allow. The captured target and environment
are used for execution; observed movement triggers one fresh preparation and
policy pass. Repeated movement executes nothing. A pathname race after the
final check remains possible; ego does not copy executable files.

A rule's head names an **effect** and never a tool: `exec(git *)`, not
`bash(git *)`. A tool-named rule's coverage is a function of the tool
inventory, so every new tool that shells out is a silent hole in every rule
ever written, and the hole is invisible because the rule still reads
correctly. `bash(...)` is a parse error whose message names `exec` rather than
an alias for it. The heads are `read`, `write`, `list`, `search`, `exec`,
`net` and `tool` — grammar in
`ARCHITECTURE.md`, which
corrects the example this line used to carry.

`tool(server/name)` is the one user-facing authority for an opaque foreign
tool. Its captured HTTP route or stdio process remains sealed into the permit,
rechecked at use, and subject to matching denies and hard ceilings, but does
not need a second `net(...)` or `exec(...)` allow. The rule delegates the
tool's downstream effects; Ego does not insert approval prompts for browser
clicks, navigation, or other work performed behind that boundary. Running the
same program through native shell remains an `exec(...)` operation and gains
no tool authority by its name.

`tool(server/*)` covers every tool of that one server and never another
server's. It is accepted only in the system and user layers (including their
profiles); a project, local, agent-file, environment or flags layer is rejected
at load, so a repository cannot grant itself every tool of a server. A matching
deny still wins over the wildcard. A wildcard grant applies only when the
resolved server is defined in user config or a system/user profile. A project
or local redefinition of the server disables that grant, including profile
redefinitions. Missing servers and tombstones grant no wildcard authority;
exact tool rules remain unchanged. Wildcard denies remain restrictive.

Rules follow a published formal grammar matched against the *canonical* call
(resolved paths, argv, endpoints — never rendered shell strings); the
mode × sandbox matrix is published and contradictory combinations are
rejected, not guessed. Every control is individually disableable; hard
ceilings come only from an administrator's `/etc/ego`.

**Launcher precondition:** a launcher of ego, or of an application that hosts
`ego-tools`, must not leak inheritable file descriptors numbered 3 or higher.
Close those descriptors or set close-on-exec (`FD_CLOEXEC`) before launching
the host. Ego does not sweep or close launcher-inherited descriptors and adds
no unsafe code to do so. With `--sandbox off`, leaked descriptors can reach
tool children. On Linux, the Landlock helper instead refuses confined execs
with `unexpected inherited file descriptor N`; `ro` and `workspace` execution
can therefore fail before the command starts. In the 2026-09-29 `rb-run`
incident (regression case), the launcher leaked lock descriptors 8 and 9 without
close-on-exec: sandbox-off children inherited them and Landlock refused
confined execs. The launcher was subsequently fixed.

**Session**

```
    --session NAME      named session (create or attach)
    --session-id UUID   caller-SUPPLIED id — no capture-then-resume dance
    --ephemeral         recognized only to refuse by name: exit 2 before a
                        session is opened or created, or any effect occurs;
                        use the durable default, --session, or --session-id
    --session-dir DIR   override store location
    --artifact-dir DIR  override the sanitized artifact root; valid only for
                        the explicit `ci` profile
    --answer ID=JSON    repeatable; answer a pending AskQuestion while resuming
    --name OP           resume/continue only; show|set=TEXT|clear, metadata-only
    --goal-operation OP resume/continue only; show|set=TEXT|achieve|block=REASON,
                        metadata-only
```

### Managed worktree launch

`ego --worktree BRANCH [PROMPT]` and `ego run --worktree BRANCH [PROMPT]`
create a new Git branch and linked worktree before session admission. The exact
branch argument is the operator's approval; ego never invents a branch name.
This option conflicts with resume, named sessions and caller-supplied session IDs.
`-C` selects the source repository, including an existing linked worktree or a
submodule. Tracked files come from HEAD, not from uncommitted source changes.
Submodules are not initialized or fetched automatically.

Standalone launches retain worktrees on every exit, including startup failure.
Versioned receipts live under `$EGO_HOME/worktrees` (default `~/.ego/worktrees`),
and identify the Git common directory, exact checkout, branch, source ref and
creation state. Standalone processes hold a checkout lease until shutdown; local
cleanup refuses an active lease. The receipt is durable before Git creation; an interrupted launch
can be inspected without starting a model:

```
ego worktree show RECEIPT_UUID
ego worktree cleanup RECEIPT_UUID
```

Cleanup is explicit, receipt-targeted and never forced. It refuses dirty state,
changed repository/branch identity, and commits not reachable from the recorded
source branch (or the initial commit when the source was detached). It also refuses new commits without upstream reachability evidence from the
recorded source branch. It retains the branch, never discards unpushed commits,
and records successful removal. Ignored files also prevent local cleanup.
Pending receipts survive failures for inspection and recovery. A repeated cleanup
of a removed receipt is unchanged success. No background or exit cleanup runs.
Do not edit a checkout concurrently with cleanup; Git performs the final
non-forced removal check. A cleanup refusal never grants force authority.

With `TUIC_SOCKET` set, launch uses TUIC `repo.worktree_create` and its receipt,
not a second local ownership record. No fallback to local Git occurs on host
failure. Hosted cleanup stays with TUIC `repo.worktree_remove` and its lifecycle
checks; the local receipt commands refuse under TUIC. Launch receipts and retained
paths are written to stderr, never to protocol or headless result stdout.
ACP hosts keep their existing explicit workspace ownership; `ego acp` does not
accept `--worktree` or create a hidden second checkout.

**Scripting contract**

```
    --output F          text|json|jsonl              (default text)
    --schema S          JSON Schema (@file or inline): the final answer MUST
                        validate; ego re-prompts on mismatch, fails code 1 after
                        --schema-retries N (default 2)
    --budget B          hard cap: '2.5$' or '400k' tokens → exit 5 when hit
    --max-turns N       cap agent turns
    --timeout DUR       wall clock: '90s', '10m' → exit 124
    --retry N           provider retries, delivery-certainty-aware: a
                        transient failure that was unsent or showed no
                        output retries, with backoff (default 0; ACP 6)
    --fallback P/M      ordered model degradation chain (repeatable)
-f, --file FILE         strict version-1 agent file with invocation settings (§5)
    --var K=V           template variable for agent files (repeatable)
-C DIR                  run as if started in DIR (git-style)
-c, --set K=V           dotted-path config override, value parsed as TOML
                        (Codex's mechanism: -c 'sandbox.mode="workspace"')
    --profile NAME      layer [profile.NAME] from config (§7)
    --bare              no ambient discovery even in a trusted workspace
    --trust-workspace   explicitly trust this launch's workspace, without asking
-q / --no-color / --plain
```

**Native web tools**

`web_fetch` retrieves public HTTP(S) UTF-8 HTML, text, Markdown, or JSON through
the same network permission rules as other connections. It sends no credentials,
cookies, or proxy authorization. It rejects private and special-use resolved
addresses before connect. Each call has a 30-second deadline, a 1 MiB response
limit, and a 32 KiB rendered-content limit. Results identify their source and
mark web content as untrusted. Redirects return a destination for a new
`web_fetch` call; they do not transfer the original network permission.

`web_search` is present only when `EXA_API_KEY` is nonempty in the process
environment. It sends queries to Exa, uses account credits, and can incur paid
charges. Each call requires `allow_paid: true` and normal network permission.
There is no fallback backend. Results contain at most five cited URLs, titles,
and highlights. Search never forwards its key through a redirect. An explicit
`--tools web_search` without a key gives a configuration error. Provider-native
hosted search is not enabled by this change.

**Attachments and tools**

```
    --attach IMAGE_PATH|HTTPS_IMAGE_URL
                        repeatable; exact image bytes admitted as a durable
                        content block before provider projection
    --tools LIST        restrict builtin tools;  --no-tools for pure generation
    --mcp NAME|@cfg.json  select an effective server by name (even one marked
                          enabled = false) or attach a strict explicitly named
                          JSON MCP server for this invocation (repeatable);
                          enabled effective servers attach without it
```

**The general session shape**

Seven of the flags above are not about *running a prompt*: they are about the
*session* a prompt runs in — what the model may use, what it is allowed to do,
and what it is told about where it is working.

```
    --tools LIST / --no-tools    what the model may call
    --allow RULE / --deny RULE   what it is allowed to do
    --profile NAME               which stored configuration layer applies
    --append-system TEXT|@file   what it is told, appended to the prompt
    --output-style NAME         trusted prompt-only response style
    --instructions FILE          which instruction file, named not discovered
```

Every surface that opens a session takes them, from one declaration: `ego run`,
`ego resume`, and `ego acp`. They are declared once (`Shape` in
`crates/ego-cli/src/cli/commands.rs`) and converted once
(`SessionShape`), so the served surface cannot drift into a `--tools` that
means something else.

The builtins `history.search` (`{"query":"acp trust"}`) and `history.open`
(`{"session":"<id>"}`) read prior conversations across the admitted session
store's workspace roots. Search returns at most 1 KiB of JSON with session ids,
titles, UTC updated dates and conversational snippets in `hits`. Its `unreadable`
list names skipped session ids and read errors; `omitted_unreadable` counts warnings
that do not fit the bound. Each skipped session is also logged. Unreadable logs
are not decoded with an older schema or changed. Open returns at most
8 KiB of user/assistant text, with an omission notice when bounded. Tool results
and streamed deltas are excluded. `--tools` and `--no-tools` control their
availability like the other builtins. SQLite FTS5 lives in a derived
`history.sqlite` cache beside the JSONL sessions; a missing or corrupt cache is
rebuilt from those logs. First use may build the index; later searches update
changed sessions. The cache is never needed to resume a session.


On `ego acp` this is how a caller shapes the sessions the served process will
open, before any client connects. Naming none of them is the unshaped session
and is byte-for-byte the behaviour of an `ego acp` that had no such flags.
`--no-tools` there is worth stating on its own: a session with no tool can
never ask for a permission seat, because a permission request originates from
a tool call and nowhere else. `--instructions` still names a file rather than
discovering one — a served session is untrusted (§4.4), and an untrusted
posture does no ambient discovery, so an explicit file is the only way to tell
a served session anything about its workspace.

Final retry semantics are owned above provider mappings. `--retry N` permits
at most N additional attempts on the same resolved model after a `Transient`
failure that emitted no content. An attempt with only opaque reasoning state
and parsed tools proven unrun may retry once per candidate generation, within
that same allowance. Pending tool items close as not run before the retry.
Visible text, visible reasoning, and provider content of unknown effect never
retry. Explicit `--retry 0` also forbids opaque/unrun recovery.
Each retry uses exponential backoff of 1, 2, 4 … seconds (at most 120),
with up to 25% downward jitter, or the provider's `Retry-After` without jitter.
`Retry-After` accepts delta seconds and HTTP-date; `retry-after-ms` takes precedence.
A provider wait above 120 seconds stops a finite plan rather than retrying early.
Without an explicit `--retry`, interactive and ACP seats use a 24-hour monotonic
recovery horizon. It starts at the first eligible failure and does not reset
when an operator forces a retry. Provider waits can use the remaining horizon.
Headless runs default to no retries. Explicit `--retry N`, including zero,
selects a finite allowance instead of interactive recovery.

Recovery status shows attempt, next delay, remaining horizon, and a force hint.
`/retry` or `Meta-r` ends only the current backoff; it does not cancel an active
request or replay output. ACP/TUIC clients can use `_ego/retry` version 1 with
`sessionId`. Cancellation and budget checks remain authoritative. Silent HTTP
is bounded by the existing 1800-second absolute call deadline, not an idle
timeout; a recovery attempt also ends at its recovery deadline.
Each repeatable `--fallback P/M` is an ordered
candidate that is re-resolved from current facts and attempted only after
capability and request-expressibility checks preserve tools, images, schemas,
limits, and reasoning state. A possibly-sent attempt that showed visible or
unknown-effect output stops automatic retry, and any possibly-sent attempt stops fallback. Every attempt has a fresh ID, reservation, request digest, settlement,
usage, and cost; output totals fold them without hiding failed or charged work.
See the accepted attempt-ownership ADR.

### 3.1 Availability — what ships today

§4.5 requires this table: *no flag documented before its contract is
implemented*. The grammar above is the **whole** design; this says which of it
you can actually run. Checked against the clap definitions in
`crates/ego-cli/src/cli.rs`, not against intent, on 2026-09-04.

A flag marked **no** is **refused by name**, with the subsystem it waits on —
not swallowed, and not reported as an unknown argument. That mechanism is
`UNAVAILABLE` in `crates/ego-cli/src/cli.rs`, checked before clap sees the
tokens, and it covers every "no" below. This table is the document half of it;
the code is the source of truth, and the two were compared flag by flag against
the built binary to write this.

A row marked **partly** is a flag that ships and a *value* of it that does not.
Those cannot use `UNAVAILABLE`, which matches spellings and would have to
inspect arguments to see a value — and would still miss the config file, since
every mode and every rule reaches the same layers whether it was typed or
written down. They are refused where the layers are read
(`crates/ego-core/src/config/layer.rs`) instead, which is one refusal covering
the flag, the file, the profile table and `-c`, and which arrives with the file
and line that said it.

| Flag | Current | Note |
|---|---|---|
| `-m, --model` | **yes** | |
| `--offline` | **yes** | |
| `--thinking` | **yes** | §3 above spelled the second value `min` until 2026-08-21; the parser has only ever accepted `minimal`, so `--thinking min` was a documented value that errored. Corrected above |
| `--system`, `--append-system` | **yes** | `--append-system` is part of §3's general session shape, so `ego acp` takes it too; `--system`, which replaces the prompt outright, is not |
| `--instructions` | **yes** | part of §3's general session shape, so `ego acp` takes it too. It was **missing from §3's grammar** until 2026-09-20 and documented only in §4.5's prose |
| `--bare` | **yes** | |
| `--trust-workspace` | **yes** | terminal and headless launches; conflicts with `--bare`; independent of `--yolo` |
| `--session`, `--session-id` | **yes** | durable session choices; the parser places them in the §4.5 mutually exclusive session group |
| `--ephemeral` | no | recognized only to refuse by name with usage error exit 2 before opening or creating a session or causing an effect; use the durable default, `--session`, or `--session-id` |
| `--session-dir` | **yes** | |
| `--artifact-dir` | **yes** | `ego run`/`ego resume` only, and only with the exact selected profile name `ci`; flags override `[profile.ci].artifact_dir` |
| `--answer` | **yes** | `ego resume --answer QUESTION_ID=JSON` validates and durably settles pending questions before continuing the session |
| `--name`, `--goal-operation` | **yes** | metadata-only `ego resume ID` / `ego continue ID` operations from §2; they cannot be combined with a prompt, answer, or run-only override |
| `--output` | **yes** | `text\|json\|jsonl`. On `ego ls` the two machine forms are §4.6's, not §4.1/§4.2's — a listing is not a run |
| `--all` | **yes** | `ego ls` only, and documented in §2 rather than here: §3 is the argument grammar of a *run*, and this widens a catalogue query |
| `--schema`, `--schema-retries` | **yes** | `--schema-retries` ships as its own flag; §3 mentions it only inside `--schema`'s description |
| `--budget` | **yes** | binds only on a currency-metered row — the run says so on `metering.selected` |
| `--max-turns`, `--timeout` | **yes** | |
| `--tools`, `--no-tools` | **yes** | part of §3's general session shape, so `ego acp` takes them too. `--no-tools` there is the session that can never ask for a permission seat |
| `-C`, `-c/--set`, `--profile` | **yes** | `--profile` is part of §3's general session shape, so `ego acp` takes it too. `-C` it already shared; `-c/--set` it does not take |
| `-q`, `--no-color` | **yes** | `--no-color` was marked **yes** here on 2026-08-21 while being parsed and read by nothing — the same silently vacuous flag §4.4 refuses elsewhere. It decides for real from 2026-08-22: it is the highest-precedence input to the one capability gate (`crates/ego-cli/src/render/caps.rs`), above `NO_COLOR`, above `TERM`/`COLORTERM`, above `isatty`. See `ARCHITECTURE.md` §1 |
| `--mode`, `--plan`, `--yolo`, `--allow`, `--deny` | **yes** | `--allow`/`--deny` are part of §3's general session shape, so `ego acp` takes them too; `--mode` and its two spellings are not part of it. `--plan`/`--yolo` are spellings of `--mode`; two at once is a usage error. All five modes enforce. `auto` resolves a separate configured judge and routes its closed verdict before any human escalation. All seven rule heads enforce: `read`, `write`, `list`, `search`, `exec`, `net`, and `tool` — `net` matches `Op::Connect`/`Op::ProtocolConnect` and `tool` matches `Op::Opaque`, both of which this build now produces. This sentence is derived from `parse.rs`'s `OPERATIONS` table by `the_documents_name_the_rule_heads_the_parser_actually_enforces`, not maintained beside it. The seven builtins are read, glob, grep, write, edit, exec, and read_tool_result. `edit` prepares both `Read` and `Write`; a matching `write(pattern)` allow covers its same-file replacement read without a separate `read(pattern)` allow. The glob still limits the target; explicit read/write denies, workspace boundaries, and plan mode still apply. A standalone read still needs read authorization; `read_tool_result` uses its session-evidence capability rather than a user-authored rule head. |
| `--sandbox` | **yes** | `ro\|workspace\|adaptive\|off`; fresh-config default `adaptive` on macOS and `workspace` elsewhere, independent of permission mode; effective configuration can override it. The measured platform backend enforces the selected level before a prepared process can launch; an unavailable required capability fails closed |
| `-f`, `--file`, `--var` | **yes** | strict version-1 agent files load through the existing bounded YAML implementation, apply their configuration layer below flags, and substitute declared body variables. M4 extracts the shared envelope for skills and still owns the direct executable shebang end-to-end proof, not these flags' availability |
| `--attach` | **yes** | repeatable local-path or HTTPS image input; bytes enter the durable session-blob/content path before model-specific projection and unsupported model/profile combinations refuse before acquisition |
| `--mcp` | **yes** | repeatable ordered selection of effective `NAME` entries and strict invocation-scoped `@FILE` entries; the selected live admission is shared by dispatch, `/tools`, `/doctor`, and deterministic `mcp_errors` |
| `--retry`, `--fallback` | **yes** | The shared turn coordinator owns delivery-safe retries and ordered, capability-checked fallback attempts; explicit retry limits and default interactive/ACP recovery follow §3 above |
| `-v`, `--verbose` | excluded | the binary refuses this legacy placeholder by name, and the exclusion regression case recorded stands. What it once waited on now exists and is not a flag: `EGO_LOG` (an `EnvFilter` string) installs a subscriber for the `tracing` events this build emits and `EGO_LOG_FILE` collects them to a file, because a diagnostic level is not an argument of a run — see §4.8. regression case was expected to retire the spelling and did not; it stays in the unavailable table rather than the retired one, because retiring a flag requires naming its replacement and there is deliberately no replacement flag to name |

The model can also call `task` with a printable `name` and a `prompt` to run a depth-one, in-process child. The child inherits the parent mode, sandbox, offline posture and workspace ceilings, shares its budget and cancellation scope, and has no `task` tool. Task-only provider batches run concurrently. A batch that mixes `task` with other tool calls is refused with failed tool feedback for every call; no child or sibling tool runs. Child tool rows are nested under their names; permission prompts identify the child through the existing human seat. Only the final answer, bounded to the existing 64 KiB tool-result limit, enters the parent context. Child usage is booked once to the parent; child conversation state is held in memory and is not resumable.

Set `task.background` to `true` to launch a durable standalone child without waiting.
The tool returns `background agent ID`. The child has its own journal and run lineage,
shares the parent budget and authority ceilings, cannot delegate, and emits no child
tool rows in the parent. Parent-turn interruption (including Ctrl+Enter) does not
cancel this child. `/tasks` lists typed exec and agent entries; `/tasks ID` inspects
one entry and `/tasks cancel ID` cancels that entry independently.
A completed child enters parent context once at the next generation boundary, not
in the middle of another generation. Recovery uses the durable task ID, original
parent run and captured tool ceiling. An uncertain interrupted child is not rerun.
Closing a session waits for its admitted children to settle. A new background launch
refuses if live permission policy differs from the admitted session epoch; start a
new session to inherit that changed authority. Background tasks do
not accept `agent` or `plan_step`; use an explicit prompt instead.
Inside TUIC (`TUIC_SESSION` is set), use `orchestrator_active` for long delegated
work and `task` for short in-turn work. Managed peer results resume the conversation.
Live task conversion with Ctrl+B and child transcript navigation are not included.

`task` also accepts `agent`, the filename stem of a named definition. Ego discovers
`*.md` files in the trusted workspace's `.ego/agents`, then `$EGO_HOME/agents`
(`~/.ego/agents` by default), then `/etc/ego/agents`. An untrusted workspace does
not contribute project definitions. The first definition of a name wins; files
are read-only inputs, not installed commands. The task descriptor lists available
names in both terminal and ACP runs. Symlinked definitions and catalogue roots
are refused.

Named definitions use the versioned agent-file envelope and prompt body. Optional
`model` selects the child's model without changing the parent. Optional `tools`
is an exact tool-name ceiling, intersected with the inherited registry; `[]`
disables all tools. `mode` is clamped to the parent mode and `deny` adds permission
rules. `allow` and output `schema` are refused for named tasks. Variables must be
fully resolved without task-time input. `sandbox`, when set, must match the
parent's captured backend: an in-process child cannot replace its sandbox.
Optional `roots` must contain exactly the absolute, canonical parent workspace;
a conflicting root or relocation is refused before child generation. If the parent
has extra profile roots, this ceiling is also refused: the inherited process
executor cannot narrow its confinement for a child. Omit `roots` to inherit the
parent perimeter unchanged. The existing sandbox, offline posture,
shared budget, cancellation, result bounds and depth-one limit remain in force.
`tools` and `roots` are named-task ceilings, not supported by explicit `--file`.

The `plan` tool owns one durable ordered plan, separate from the checked goal.
Call `{"operation":"replace","steps":[{"id":"inspect","text":"Inspect the source","status":"pending"}]}`
to replace it, or `{"operation":"step","id":"inspect","status":"in_progress"}`
to update an existing ID. Statuses are `pending`, `in_progress`, `completed`, and
`blocked`. Keep IDs stable when reordering or replacing steps. IDs are unique,
printable, nonempty strings of at most 80 bytes; step text must not be empty.
The plan is limited to 128 steps and 16 KiB of combined IDs and text. An empty
replacement clears it. Updates are journal events (`plan_updated` in JSONL),
survive replay, fork, and compaction, and appear as terminal progress rows and
ACP plan replacements. Plan completion does not achieve the goal.

An optional `task.plan_step` names an existing plan step. The parent records
`in_progress` before the child runs, then `completed` on success or `blocked`
on failure. An unknown ID or a second child assigned the same ID in one batch
is refused before that child runs. Child progress changes only its linked step,
not the objective or sibling steps. Child-authored plans stay local to the child.

The table distinguishes implemented flags, intentional refusals, frozen M5
target spellings, and the excluded verbose placeholder. The defect would be §3
reading, to someone who has not run the binary, as though all of it worked.

Milestones in bold above are from `ARCHITECTURE.md` §8's table, which owns
them; the flags with none are waiting on a subsystem that table does not place.

## 4. Output contracts

<!-- ego-cli-json-output:start -->
### 4.1 `--output json` (one object on stdout at completion)

```json
{
  "v": 17,
  "session": "s_9f2k…",
  "result": "…final assistant text or --schema-validated object…",
  "exit_reason": "done|budget|max_turns|timeout|permission|question|paused|error",
  "turns": 4,
  "usage": { "uncached_input_tokens": 8110, "cache_read_tokens": 40100,
             "cache_write_tokens": 1200, "output_tokens": 3120 },
  "context": { "used_tokens": 51410, "provenance": "generation_boundary" },
  "cost": { "state": "known",
            "total": { "amount": "0.083", "currency": "USD" } }
}
```

`v` is **`PUBLIC_EVENT_VERSION`**, the same number §4.2's events carry, and not
a number of its own. This object is a projection of the same public model —
`usage`, `cost` and `context` are that vocabulary's types, so a change to them
changes both surfaces at once, and a second number would let one of the two
move without the other. §4.6's catalogue is versioned separately for the
opposite reason: it publishes no part of the event model.

When the session has a goal, the summary also contains `goal`:
`{"status":"achieved|stopped|exhausted|active","reason":"…"}`.
The field is absent for sessions without a goal. `achieved` means the durable
goal was achieved; it has no `reason`. `stopped` carries the goal-check stop
reason (including bounded evaluator failure diagnostics), or the blocker reason.
`exhausted` carries the goal continuation or goal-check token budget reason.
`active` means the goal remains open without a goal-check stop, for example
when the invocation needs human input. Its last blocker reason is included if
present. `ok` and `exit_reason` still describe the invocation, not goal success:
a completed invocation can have `ok:true`, `exit_reason:"done"`, and a stopped
or exhausted goal. Consumers that require goal success must inspect `goal.status`.

`usage` is the sum of the per-generation deltas, split by the categories that
are *priced differently*; it is a flow and only grows. `context` is the
occupancy *level* of the live context window and goes **down** after a
compaction, so it is reported separately and never derived from `usage`.
The last measured occupancy is not clamped to the model window. A delivered
final answer completes its turn even if usage reaches or exceeds the resolved
window. Before the next provider request, a known-full context is refused with
`context_overflow`, asking for `/compact` or a larger model window. Unknown occupancy and history added since
the measurement are not an exact preflight token count; a provider-reported
context overflow remains a separate typed failure.
`cost.amount` is an exact decimal **string**, not a JSON number — binary floats
cannot represent a price. `cost.state` is `unstarted` before the first priced
generation, and `{"state":"unknown","reason":…}` once any generation could not
be priced: an unpriceable generation makes the session total unknown rather
than silently under-reported.

On exit 3, exactly one human-input payload is present. A permission boundary
uses the existing `denied`/`denied_by` fields. An `AskQuestion` boundary uses a
`question` object containing its stable ID, prompt, choice mode, options, and
whether free text is accepted. A script answers it with
`ego resume ID --answer QUESTION_ID=JSON`; a mismatched ID or value is exit 2,
not an answer guessed against a different question.

`paused` is the one reason that carries **no** payload and asks nothing. The
session stopped because a host called `_ego/pause` (§ `ARCHITECTURE.md`), and
the only thing that moves it again is that host calling `_ego/resume`. It is
exit 6 and deliberately not exit 3 for exactly that reason: a script that reads
3 goes looking for a `denied`/`denied_by` pair or a `question` object, and a
pause has neither.

<!-- ego-cli-json-output:end -->
### 4.2 `--output jsonl` (one event per line, live)



- `session.started` carries a **`capabilities` string array** (feature
  detection by name, never version parsing) and error arrays
  (`mcp_errors`, `hook_errors`) **present only when non-empty** — a CI gate
  is a presence check.
- The **final result rides the terminal event** (`turn.completed` of the last
  turn) — no `--output-last-message` temp-file dance.
- **One error field.** `ok: bool` plus `exit_reason`; no second status field
  that can contradict it (Claude's `subtype:"success"` + `is_error:true`).
- JSONL mode is a strict superset of text-mode resilience: any error text
  mode survives, jsonl survives as an event.
- The schema is versioned in the header and published; same schema as the
  session file and the ACP projection — learn it once.

The live `child_progress` event carries the parent task `turn` and `item`, the
current `activity`, and the child's provider-reported cumulative `output_tokens`.
It uses the same serialized publication as nested task tool rows. It does not
append a child transcript or change parent usage accounting. Interactive seats
show one cyan row per running in-turn task, with a clipped name and right-aligned
elapsed time and received tokens. Counts update when the provider reports usage;
zero before the first report is not a streaming token estimate. No-color seats
show the same child text without SGR. Task completion, failure, or cancellation
removes its row. Durable background tasks retain their existing `/tasks` view.

### 4.3 Exit codes

```
0 done        1 task failed (agent reports failure / schema never validated)
2 usage error 3 human input needed in non-interactive mode
4 provider/network error   5 budget or turn limit
6 paused at a host-requested boundary   124 timeout
130 SIGINT    143 SIGTERM
```

Windows ConPTY reports a double idle `Ctrl+C` exit with the native
`STATUS_CONTROL_C_EXIT` (`0xC000013A`) status rather than the POSIX-derived
`130`; both identify an operator interrupt.

Only Gemini ever documented an exit-code table; Codex and Claude collapse
almost everything into 0/1. Scripting needs the distinctions (retry on 4,
escalate on 3, alert on 5, resume on 6). The shutdown contract is explicit (Claude's, which
is right): on SIGTERM abort the turn, kill the tool process tree, run
session-end hooks, flush the session file, exit 143. Flags are never removed
silently: a retired flag errors **by name** with its replacement.

For `config` and `rules`, exit 0 includes an idempotent unchanged result; exit
1 covers missing `config get`, opposite-effect or ID-collision conflicts,
strict candidate validation, changed edit bases, and pre-publication store
failure; exit 2 covers grammar, type, unknown key/tool, non-persistible
operation, and future rule-ID generation errors. A durability-uncertain
publication has already changed authority, prints its successful receipt,
warns on stderr, and exits 0. No management command uses exits 3–5.

For `rules remember|explain`, a non-object or tool-schema-invalid `ARGS_JSON`,
unknown/non-persistible tool, or invalid/nonexistent `-C` is exit 2. A
schema-valid tool argument whose named path is absent or cannot be
canonicalized is exit 1. These pre-mutation failures write no stdout and one
actionable diagnostic to stderr.

### 4.4 Non-interactive posture: untrusted by default

Headless runs never hang on a prompt. An unresolvable permission is exit 3 with
the denied action in the JSON result; scripts opt in with `--allow` rules, a
stronger `--sandbox`, or `--yolo` rather than answering permission prompts. An
`AskQuestion` is also exit 3, but carries the `question` object from §4.1 and
continues only through `ego resume ID --answer QUESTION_ID=JSON`.

Exit 3 has more than one cause, and the three opt-ins above are not
interchangeable, so §4.1's object carries **who refused** beside what was
refused: `denied` is the action, and `denied_by` names the authority — a rule
(with the layer that owns it, which is what says whether it can be edited at
all), the mode, or the workspace boundary. `denied_by` is **absent when nobody
decided**: every party in the evaluation order was consulted and the question
is still open, which is the case `--allow` answers and the only one where the
absence of an authority is itself the fact. Present-or-absent, per §4.2's rule
for this format. See
`ARCHITECTURE.md`.

The three remedies are alternatives because they act on different axes, and
one of them is not a permission at all: **the workspace boundary belongs to
`ARCHITECTURE.md` §3.4 axis 4 — the execution backend — and no mode moves it.**
A mode says who approves; a backend says what is reachable. So an operation
outside the workspace is refused rather than asked about, `--yolo` does not
lift it, and none of the three remedies above resolves it — because it was
never an unresolvable *permission* for them to resolve. Widening what ego may
reach is `--sandbox`'s business, which is why that clause is listed beside
`--yolo` rather than subsumed by it.

A tool call the boundary refuses is an exception to exit 3: it changed
nothing, so the refusal is returned to the model as a failed tool result and
the turn continues. This includes reads, writes and execs, including writes
to read-only roots (regression case and regression_case). A user-decided denial, a
deny rule, or an unanswered permission question still ends the turn.

### Host PID-to-session index

While ego holds a session writer lease, it atomically publishes
`$EGO_HOME/pids/<pid>.json` (default `~/.ego/pids/<pid>.json`):

```json
{"pid":12345,"session_id":"01900000-0000-7000-8000-000000000001","cwd":"/absolute/workspace"}
```

`pid` is the process ID, `session_id` is the leased session UUID, and `cwd` is
its absolute workspace path. Publication writes a temporary file beside the
index, then renames it into place. Opening the initial session successfully
publishes its binding. A replacement session replaces the binding only after
it becomes active; opening or rejecting a candidate does not change it. Releasing the last owner of that lease removes its
binding; releasing an older session does not remove a newer binding. Normal
exit releases these owners. A crash or forced termination can leave an index.
The index is only a lookup hint, not a liveness or ownership guarantee: hosts
must verify that the selected session's `lease` records the same PID and that
the process is live. The session lease remains the source of truth. A custom
`--session-dir` does not move this index out of `$EGO_HOME/pids`.

### 4.5 Parse and lifecycle contract (normative before release)

The grammar ships with: a command × flag **applicability matrix** (no flag
documented before its contract is implemented — per-milestone availability
table); a normative **input-precedence algorithm** over argv prompt, `-`,
piped stdin, `-f`, `--attach` and agent-file body, with `--` reserved for
prompt disambiguation (`ego -- run` prompts the word "run"); explicit
mutually-exclusive groups (`--session`/`--session-id`/`--ephemeral`, mode ×
sandbox); a defined winner when limits collide (budget beats schema-retry:
exit 5); **stdout is protocol-clean** in json/jsonl/acp modes, all diagnostics
on stderr; agent files are a versioned format. CLI reference and completions
are generated from the same final clap definitions and snapshot-tested. M5
regression case regenerates both artifacts only after all 18 commands and their
applicability rules are installed; hand-maintained parallel command lists are
not release evidence.

**`--help` and `--version` are answers, not errors**: stdout, exit 0, and a
usage line naming the command that was asked about — `ego ls --help` says
`Usage: ego ls`, because `Usage: ego` describes an invocation that does not
work. Spelled out because it was wrong in all three respects at once until
2026-08-21: clap reports both through its error type alongside the genuine
failures, and one conversion sent help to stderr at exit 2, which §4.3
defines as a usage error. Asking what a command does is not misusing it, and
a script that pipes `--help` must not get an empty stdout and a failure.

**Ambient discovery is trust-gated.** Project-local config, `AGENTS.md`,
hooks, skills and MCP servers load only after an explicit terminal trust
answer or `--trust-workspace` admits the canonical workspace. Declining, Ctrl-C or EOF
continues bare. Headless runs without a dialog: an explicit `--trust-workspace` grant or a valid
remembered answer admits project content; otherwise it runs untrusted and bare. The trust offer names absolute file paths
and explains that yes loads project instructions, configuration, skills, hooks
and MCP servers, while no continues without project files. `--bare` forces this and skips the trust
question. The question is asked only where there is something to answer about:
a workspace holding no project config, skill, or discoverable instruction file
starts untrusted in silence, and the `trust` status segment says so. For a
workspace under `~/Gits`, instruction discovery starts at `~/Gits/AGENTS.md`
and descends through every ancestor to the working directory; outside that
tree it starts at the workspace root. Scratch work under `~/Gits/.tmp` does not
inherit shared instructions. `AGENTS.md` is canonical; a relative `CLAUDE.md`
symlink to it adds no duplicate prompt text.

For an explicitly authorized managed PTY, launch:

```sh
ego --trust-workspace --thinking medium --sandbox off --yolo --model openai-codex/gpt-6.1-sol 'TASK PROMPT'
```

`--trust-workspace` supplies a fresh `yes` for the current canonical workspace
and its current executable surface, without reading a terminal answer. It
admits ambient instructions, project configuration, hooks, skills and MCP
declarations, and remembers the answer in `$EGO_HOME/trust.json`, replacing
an earlier refusal for that root. Executable changes require a new answer on
later launches without the flag. This grants no tool permission or sandbox
exception; `--yolo` alone never grants workspace trust. Without the flag,
the remembered answer or interactive question still applies. `--bare`
conflicts with the flag. Headless launches never ask or record a refusal when no
valid answer exists. An empty workspace
still has nothing to admit and stays untrusted.

A standalone `@relative/path` line outside fenced or indented code loads an
instruction file relative to the containing file. `@~/path` expands the launch
home; absolute imports are also supported. External targets outside the discovery
tree require workspace consent bound to each exact target and SHA-256 content.
The trust question lists these targets; headless runs require `--trust-workspace`
or a matching remembered answer. Explicit-file imports outside its directory
require the same target to be covered by workspace consent. No tool read root is
widened. Duplicate files load once. Cycles, missing or malformed imports,
scope escapes, symlinks, non-text files, and size refusals are reported rather
than silently ignored. One run loads at most eight files, 64 KiB per file and
256 KiB in total, and attempts at most eight distinct imports. An answer —
`yes` or `no` — is remembered in
`$EGO_HOME/trust.json` against the canonical root and the parsed executable
declarations, *plus every program inside the workspace they name* —
`[[hooks]].handler.command`, `[mcp.servers.*].command` and
`[auth.*].credential_command` — so replacing a trusted hook script asks again
even when the config that names it is byte-identical. In-tree instruction and skill
edits do not invalidate the answer. External instruction edits do invalidate it. Terminal and ACP seats share this ledger.
Invalid config is never skipped silently: unknown keys and malformed MCP
entries are hard errors (Codex's `--strict-config`, as the only behaviour).

`--instructions FILE` is the headless opt-in and is **not** ambient: it names
a file rather than discovering one, so it survives `Posture::Bare` and
*replaces* the discovered set rather than adding to it. Its bounded imports
are relative to `FILE` and cannot leave that file's directory. `--bare` together with
`--instructions` is a conflict for the applicability matrix above, not for the
loader — from a resolved posture alone, "the user demanded bare" and "the
workspace is untrusted" are the same value and want opposite answers. See
`ARCHITECTURE.md`.

### 4.6 `ego ls --output json|jsonl` (the catalogue)

§4.1 and §4.2 are shapes *of a run*: they carry a `session`, a `result` and a
`usage`. A listing has none of those, so it is its own contract rather than
those two bent to fit. Same `--output` flag and the same three values.

`--output json` writes one object:

```json
{
  "v": 2,
  "sessions": [
    {
      "id": "0192f3a1-…-8c41",
      "workspace_root": "/Users/…/ego",
      "model": null,
      "created": 1787206085352,
      "updated": 1787292481007,
      "title": null
    }
  ]
}
```

`--output jsonl` writes one of those session objects per line, with no
envelope — the unit of a catalogue is a session, as the unit of a run is an
event.

- **`v` is this listing's own version**, not `PUBLIC_EVENT_VERSION`. A new
  field on a run's event stream must not bump a contract that did not move.
- **An object with a `sessions` key, not a bare array.** A top-level array has
  nowhere to put a version, and a consumer that cannot see the version is one
  that cannot be told the shape changed.
- **`sessions` is ordered: most recently updated first**, and the id breaks a
  tie. It is the order `ego resume` selects "latest" by, so the first row is
  always the session `ego resume` with no ID would continue.
- **`model` and `title` are `null` today** and are present anyway. An omitted
  key and a key nobody has set are two states with identical bytes. In M5,
  `title` projects the durable display name and remains `null` when unset.
- `created` and `updated` are UTC milliseconds since the epoch, the same
  `Timestamp` the session log carries.
- The empty store writes `{"v":1,"sessions":[]}` and exits 0. A script that
  gets no output cannot tell an empty store from a command that failed.
- Lineage changes the listing envelope version once; a session row has no
  independent catalogue version.

Default `--output text` is a table for the person who typed it: id, updated,
model, title, and a `WORKSPACE` column only under `--all` — without `--all`
every row would carry the same path.

### 4.7 `--output text` (the default): two renderings, one gate

§4.1, §4.2 and §4.6 are contracts with a *program*. This one is a contract with
a **screen or a file**, and they are not the same contract with escapes
removed. Full argument in
`ARCHITECTURE.md`.

**Redirected — a pipe, a file, `TERM=dumb`, `--output json|jsonl`.** stdout
carries the model's own markdown, byte for byte: nothing wrapped, nothing
re-flowed, no escape sequence of any kind, in-band or out. `ego run "…" >
answer.md` produces a markdown file, and it is the *same* file from any
terminal — the wrap column of a redirected run comes from a constant, never
from the window that happened to launch it. Everything that is *about* the run
rather than *of* it stays on stderr, as before.

**On a screen.** The markdown is rendered: headings, emphasis, inline and
fenced code, lists, block quotes, links, and tables drawn in box-drawing
characters and measured in display cells. Rendering is **block-wise as blocks
close** — a paragraph is drawn when the blank line after it arrives, a table
when its last row does — so the output is live without a half-built table ever
reaching the screen. Colour depth is whatever the gate found: 24-bit, 256, 16,
or none.

Top-level prose paragraphs stay on one logical line. The terminal or narrower
viewer owns their soft wrapping; ego adds no continuation indentation. Explicit
Markdown hard breaks and paragraph boundaries remain. Lists, block quotes,
fenced code, and tables keep their structured layout.

**One gate decides, once** (`crates/ego-cli/src/render/caps.rs`). In precedence
order: `--no-color`, then `NO_COLOR` (presence, not value), then whether stdout
is a terminal, then `TERM`, then `COLORTERM`, then `TERM_PROGRAM`. No other
code in the workspace reads one of those variables or formats an escape by
hand. `--no-color` on a real terminal removes the *colour*, not the layout: the
person asked for no colour, not for a file.

`--plain` overrides the gate with unstyled, append-only output for CI. Non-TTY
output and `TERM=dumb` also append retry notices. Interactive retries otherwise
replace one live row with the cause, attempt, countdown and retry hint, then
leave one settled cause receipt when the turn recovers or fails. `-q` suppresses
ordinary diagnostics, not recovery controls; `--output json` and `jsonl` keep
their machine-readable contracts.

**Out of band**, on a screen only: OSC 133 C/D around the run (so a host
segments the output into a command block instead of pattern-matching tool
headers), OSC 9;4 for the progress indicator — cleared on every exit path,
including failure — the window title, a desktop notification for a run that
took longer than a minute (suppressed by `-q`), and OSC 7770 `state`/`intent`
under TUICommander, which is proven by `TUIC_SOCKET` and not by `TERM_PROGRAM`.
Provider retry waits also emit OSC 7770 `ego-recovery` on this same terminal-only
path: compact JSON with exactly `phase`, `next_attempt_ms`, and `attempt`.
`waiting` carries the next attempt's Unix epoch milliseconds; `retrying` carries
null and is emitted at actual dispatch, never inferred from a countdown.
`attempt` counts retries from 1. Output, turn completion (including cancellation
or give-up), and process exit emit `ego-recovery=clear` while recovery is active.
The payload stays below 256 UTF-8 bytes. `/retry` or Meta-r releases the active
provider wait through the same ingress control used by ACP.

The human-facing product name is `εgo`, and every window title begins with the
exact prefix `ɛ । `. Every payload is stripped of control characters before it
goes into a sequence: a tool name, an error message and an intent line all come
from somewhere that is not ego.

On-screen text output also reports prompt-cache hits from provider usage, with
compact token counts. For Anthropic Messages requests with an explicit cache
boundary, a hit followed by a miss on the same marked input produces a qualified
warning: the provider may have expired or changed its cached entries. A changed
session, provider, model, or marked prefix starts a new comparison. Missing input
usage is unknown, not a miss. These observations do not change public JSON events
or durable accounting and are suppressed by `-q`.

When a turn ends, on-screen text output prints elapsed time, completion status,
and local time. Successful builtin `edit` calls contribute measured added and
removed lines to a `tracked edits +N -M` receipt. These counts are not a Git diff:
repeated edits are accumulated, and unmeasured tools do not contribute. Colour
follows the terminal gate. Quiet and redirected output omit these receipts.

### 4.8 Diagnostics (`EGO_LOG`, `EGO_LOG_FILE`)

The `tracing` events this build emits reach nobody by default. Two environment
variables — not flags, see below — decide otherwise:

| Variable | Meaning |
| --- | --- |
| `EGO_LOG` | A `tracing` `EnvFilter` string: a level (`warn`, `debug`) or target directives (`ego::listen=info,ego_ai=debug`). Unset or empty installs **nothing** — not a silent subscriber, nothing — so every stream is byte-for-byte unchanged |
| `EGO_LOG_FILE` | An absolute path to append to instead of stderr. Required for an interactive session, which paints its own prompt on the terminal stderr would land on; without it such a session is refused by name rather than having its display corrupted |

Diagnostics never touch stdout, which is protocol-clean in `json`, `jsonl` and
`acp` modes. A malformed `EGO_LOG` is a usage error before any run work, because
a typo that silently disables diagnostics is the one failure an operator cannot
detect by looking. A relative `EGO_LOG_FILE` is refused: it would resolve
against the working directory of whatever started the process, which under a
host is not the operator's.

**One target is special.** `ego::body` carries whole upstream response bodies —
the ones removed from operator-visible errors because those errors reach an ACP
host — and a failing endpoint or an intercepting proxy may echo a credential
back inside one. It is **off in every filter unless named**, so `EGO_LOG=trace`
does not print it and `EGO_LOG=ego::body=debug` does. The rule that generalises:
a level chosen to watch one subsystem never discloses another's secrets.

**Why not a flag.** §3 is the argument grammar of a *run*, and a diagnostic
level changes nothing a run does, is recorded in none of its evidence, and
belongs in neither the generated reference nor the completions. It is also the
shape a host can use: something spawning `ego acp` sets variables for its child
and cannot inject arguments into a command line it did not compose. See
`ARCHITECTURE.md`.

## 5. Agent files — the scripting language

An **executable agent file** is YAML frontmatter + prompt body. Same format
family as skills; `--var` fills `{{placeholders}}`; shebang makes it a program:

```
#!/usr/bin/env -S ego run -f
---
version: 1
model: anthropic/claude-sonnet-5
mode: accept-edits
sandbox: workspace
allow: ["exec(cargo *)"]
schema: "@review-schema.json"
vars: [severity]
---
Review the staged diff. Report only findings of severity {{severity}} or
higher. Output must match the schema.
```

```bash
chmod +x review.ego
./review.ego --var severity=major | jq '.result.findings'
```

Version 1 executable files accept `version`, `model`, `mode`, `sandbox`,
`allow`, `deny`, `schema`, and `vars`; unknown fields and versions refuse.
Named task definitions also accept the `tools` and `roots` ceilings described
in section 3.1; executable `--file` input refuses those two fields. Those operational
keys mirror their CLI flags one-to-one — the file *is* a saved command line
plus a prompt, not an untyped map of every present or future flag. The one
strict bounded YAML envelope is shared with skills; the complete format,
limits, variable equality, relative-path, and failure contract is
`ARCHITECTURE.md`.
Nothing else examined has this as a first-class, executable unit (Claude
Code's slash commands are REPL-only; pi's prompt templates are not
executables).

### 5.1 Skills are in-session capabilities, not agent files

A skill is a strict `<name>/SKILL.md`. Roots, in highest-precedence order:
trusted project `.ego/skills`, `.agents/skills`, `.claude/skills` at each directory
from the workspace up to the nearest repository root (nearest directory first);
user `$EGO_HOME/skills` (default `~/.ego/skills`), `~/.agents/skills`,
`~/.claude/skills`; then `/etc/ego/skills`. Outside a repository, only the
workspace directory contributes project roots. Project roots are not inspected
without workspace trust. Bare headless runs do not discover ambient skills.
The first valid definition of a name wins. Canonical file aliases are read once;
fixed actions and the bundled `context-management` name remain reserved.
The skill listing shows the selected file path. Current Agent Skills frontmatter
and supported Claude extensions are documented in `ARCHITECTURE.md`.

`/skills`, `/skills list`, and `/skills diagnose` inspect the one admitted
catalogue; `/skills rescan` prepares and atomically publishes a complete
candidate; `/skills invoke <name> [arguments]` and `/<skill-name> [arguments]`
invoke a user-visible skill through the same typed operation. A model-visible
skill is
announced by bounded name/description/trigger metadata like an admitted MCP
capability. Its full body and rooted read-only resources enter context only
after invocation. A user-only skill is never announced to the model, and a
model-only skill is hidden from slash completion and refuses direct user
invocation. `/hooks` accepts no arguments and inspects the active immutable hook
catalogue without reloading or editing it.

Skill frontmatter may narrow tools, model, effort, paths, and invocation
visibility. It cannot relax system rules, trust, permission, sandbox, offline,
budget, judge, or egress. `context: fork`, `agent`, `background`, and `hooks`
use the final child-run and source-bound hook owners; `shell`, dynamic
`!command` context, and Claude runtime variables are refused permanently.
Invocation stores the rendered instruction and source digest durably so resume
and compaction never silently reread a changed file.

## 6. Interactive surface (complete, linear, keyboard-first)

The surface stays in the terminal's primary buffer. The terminal owns
transcript scrollback, selection, and copy; ego never enables alternate-screen
mode or mouse capture. One interactive owner appends rendered transcript
batches, then repaints only the ephemeral status, menu, and composer rows.
History, completion, multiline editing, bracketed paste, and concurrent output
use reedline. A smaller presentation may collapse detail by width, never remove
a capability.

Interactive startup prints the application name and version before configuration,
trust, provider, or MCP notices. It reports the selected model, effective posture,
and key hints after admission. Startup notices remain visible; `--quiet` keeps
its existing announcement policy. The name does not require a resolved model.

The live seat enters raw mode before startup discovery, so lines typed before
the composer appears retain their Enter boundaries without cooked terminal
echo. The first starts a turn; further submitted lines enter the busy FIFO.

Admitted MCP prompts also appear as `/mcp__SERVER__PROMPT` commands. Supply
prompt arguments as a JSON object of strings, for example
`/mcp__docs__review {"target":"src"}`. Required arguments and allowed keys
come from the captured prompt descriptor. Admitted resources appear as
`/mcp_resource__SERVER__URI` and take no arguments. Completion shows the
captured server and resource names. In command names, bytes other than ASCII
letters, digits and hyphens use `_hh` hexadecimal escapes (including underscores).
These commands attach the captured MCP result to the conversation without a
model call. They use the existing effect permission path and refuse a command
from a generation that is no longer admitted. ACP announces the same commands
and their JSON argument schemas when the admitted runtime opens a prompt.

Slash commands: `/help /model /thinking /fast on|off /mode /plan /yolo /permissions /mcp
/sessions /history QUERY /resume [ID] /continue [ID] /fork [message-id]
/compact /refresh /clear /usage /statusline /export /config /setup /doctor /quit /new
/name /goal /tasks /tools /expand /skills /hooks /memory /init /retry /version`. These commands and aliases form the
maintainer-selected surface; a missing implementation remains required work rather
than an implicit de-selection. Typing `/` at the beginning of the composer
opens a filterable menu; the menu, shortcut bindings, help, and enabled-state
checks are projections of one action registry rather than parallel command
implementations. An exact registered action executes with one `Enter`; a
partial completion remains inert until explicitly submitted, and pasted slash
text remains ordinary prompt input. A submitted unknown slash command is refused
locally with the nearest command names and never starts a model generation.
To send literal text that starts with `/` (for example `/tmp/x`), add a leading
space. Bracketed paste also remains literal prompt input.

That paragraph is the final contract. The current implementation has the shared
action registry: a typed complete slash is classified as an action,
while bracketed paste and non-command text remain ordinary prompts. Completion,
menu, help, dispatch, and live availability read that one registry. Actions
whose final owner has not landed remain visible with their explicit disabled
reason rather than becoming model input or silently disappearing.

### Reviewed persistent memory and project initialization

`/memory [project|user] [list | show ID | set ID TITLE CONTENT | disable ID | delete ID]`
uses the native MDKB index in the workspace or ego user directory. The selected
scope must already have a `.mdkb` index; initialize it explicitly with `mdkb init`.
Quote a title or content that contains spaces. `set` creates or replaces the
record as an operator-authored `topic` with `user_statement` provenance and the
`ego-reviewed` tag. `list` and `show` expose native provenance and lifecycle
metadata. Every mutation requires review of the exact proposed content and
scope. Disable preserves the record and source provenance, adds `ego-disabled`,
and sets native TTL to zero so ordinary MDKB retrieval excludes it. Native
MDKB expiry and archive rules still apply. Delete uses native MDKB removal;
native archive and history may retain content. It is not secure erasure.

These actions require workspace trust. Plan mode, read-only confinement, root
limits, and explicit write denies remain effective. No model response, tool
result, or conversation is automatically saved. Review must exclude secrets and
untrusted tool instructions. Memory bodies are not automatically promoted to
system instructions. This surface adds no second memory store.

`/init` builds a deterministic, local `AGENTS.md` proposal and shows the diff
before confirmation. It creates `CLAUDE.md` only as the relative sibling symlink
`AGENTS.md`. Existing `AGENTS.md` content is preserved. An independent
`CLAUDE.md`, an unexpected symlink, denied write, or changed reviewed state is
refused. No model call is required. This explicit operator control-file workflow
does not grant general directory-write authority to tools or setup. Partial I/O
failures are reported and existing files are never overwritten. Use `/refresh`
after initialization to request the normal trust admission in a new prompt epoch.
Both commands are refused while a turn is running; a non-human seat cannot
confirm a mutation.

Arguments that do not belong to a selected local action are usage errors, not
model prompts. In particular, `/mcp` inspects the admitted runtime snapshot;
`/mcp add NAME --http URL` and `/mcp add NAME --stdio -- COMMAND ...` parse
the same add grammar as `ego mcp add` and persist configuration through the same
transaction and live-admission service. `/mcp toggle NAME` changes enablement
only for this session and does not edit stored configuration. Both are local
commands, not provider prompts; MCP mutations require an idle turn.

Read-only `/tasks [ID]`, `/config`, `/config perimeter`, and `/usage` answer
while a turn runs. They read the current session journal, admitted configuration,
and durable usage records without starting a generation. State-changing commands
remain on their existing host-command path; an inspection does not cancel,
restart, or complete the active turn.

`/help` renders task-oriented Discover, Model, Control, Session, Integrations
and Surface groups from that registry into transcript scrollback. `/doctor`
wraps the shared health snapshot at the live terminal width. Both preserve
every field at 120, 80, 60, and 40 columns instead of relying on terminal
reflow or attaching multiline output to the composer.

`/mode` opens the permission picker. `/mode <name>` selects exactly one of
`plan`, `default`, `auto`, `accept-edits`, and `yolo` (bypass permissions).
Unknown names and extra arguments are local usage errors, never model input.
`/plan`, `/yolo`, and `/permissions` still take no arguments.
Shift+Tab cycles in this order: plan → default → auto → accept-edits → yolo → plan.
It preserves the composer draft and attachments and does not submit them, in both
Emacs and Vim editing. While a picker, permission prompt, or other modal is open,
Shift+Tab does nothing. During an active turn it queues a local selection until
the turn ends; it does not change that turn's captured policy. Repeated selections
start from the last accepted staged mode. A receipt shows the selected mode and
its next-generation boundary. Fixed sandbox, system max_mode and profile ceilings
still apply. A refused selection leaves the prior selection intact and reports why.
Auto selection validates its configured judge without a provider effect.
`/plan` stages plan idempotently. `/yolo` and picker selection stage yolo directly;
they do not ask for a second confirmation.

Default keeps its existing policy: reads are admitted without a human prompt,
uncovered writes ask, explicit allow rules are honored, and coordinator bounded
calls retain their deterministic admission. Explicit denies and hard boundaries
still win. This selection feature does not add a new approval mechanism or change
existing ACP/session permission answers.

Configuration, CLI flags, ACP controls and saved selections use the same public
names. `edits` remains an input alias for `accept-edits`; saved config emits
`accept-edits`. The private content-addressed snapshot v1 wire retains `edits` to
preserve saved authority digests. Missing mode still falls back to default except
for the existing trusted-interactive auto default. Session resume keeps its
recorded policy and confinement; selection never widens them implicitly.

`/permissions`
exposes the same list, allow, deny, revoke, remember, and explain services and
DTOs as the root `rules` command. List/explain use the session's
already-admitted ruleset; mutations always target base user config, never the
active profile or `EGO_PROFILE`. Remember and explain select an admitted tool,
then edit its schema-derived fields with exact values prefilled; nested shapes
remain in one prefilled JSON fallback instead of requiring a whole call to be
typed as JSON.

An unresolved permission modal shows the call as a readable command, its risk
in plain words, and the keys `[y] yes`, `[a] yes, do not ask
again for <rules>`, `[n] no, say what to do instead`, `[e] edit`, `[d]
details`. The complete rule list wraps to the terminal width before `a` can
save it. The risk row never shows an internal name; a limited risk (changes
inside the workspace) and an unknown one (a program ego cannot inspect) read
differently. `d` shows the rest: the reason, the canonical operations, the exact
parameters, and what `a` would store with every widening. `y`, `a`, `n`, `e`,
and `d` act at once. `e` edits the same prefilled schema-derived form and sends
the result through full preparation and policy classification again. `a`
applies the displayed allow-rule plan through the single atomic config owner and
then allows the current call; it never asks for the tool, JSON, or a second
confirmation. Its rules are the narrowest the grammar can express that still
match the next call: a program-plus-subcommand prefix for recognized `exec`
programs (`exec(git commit *)`, `exec(npm run *)`), the workspace glob
for writes (`write(<workspace>/**)`), and the exact form for everything else.
A static shell list or pipeline in the workspace can offer one rule per inner
program instead of one literal shell script. Every command must be a known
process program (or `pwd`), with literal words and no shell effects. A later
compound is covered only when every command has an allow rule and its initial
cwd is in the workspace. Inner deny rules still win. Redirects, expansion,
assignments, background jobs, shell state changes such as `cd`, unsupported
syntax, and unknown programs keep the exact shell form. `rtk git status` keeps
the wrapper and action (`exec(rtk git status *)`); `pnpm exec biome` keeps the
selected tool (`exec(pnpm exec biome *)`).
A command whose prefix would authorize code the operator never saw keeps its
exact form: an unsupported shell, interpreter, or launcher (`python3 -c`,
`env`, `sudo`, `xargs`, ...), a program given by path, or a name carrying shell
metacharacters or `*`. A bare program, a leading option or toolchain selector,
and programs without recognized subcommands also keep exact argv. Suggestions
never offer a generic `exec(program *)` rule. The named subcommand prefix
allows later arguments to that action, as displayed before storage; it does
not infer whether those arguments mutate configuration or run another program.
Deny rules, including system ones, are still evaluated first. No allow rule,
including an exact rule or one from user or project config, covers a canonical
filesystem path containing a `.git`, `.ego`, or `.claude` component at any depth
(ASCII case-insensitive on every platform),
including nested repositories. The remembered write glob stays workspace-wide;
control paths remain uncovered and follow the existing mode/judge/human approval
path. In default mode a control-path write still asks on each call; remembering
cannot suppress that question. Other modes keep their existing settlement
semantics. The receipt is one sentence naming each stored rule, committed to
the transcript. A write or
staging failure is shown there and denies the call. `n` asks `What should it do
instead?`: words typed there answer the call as a failed tool result, so the
model reads them and the run continues; an empty answer is a plain refusal,
which ends the run as before. The plain non-terminal prompt (`allow`, `deny`,
`amend`) is unchanged.

A mode or rule change takes effect only at the next generation boundary,
after the preceding provider attempt, complete sibling tool batch, permission
settlement, events, usage, and cost are durable. Each generation captures one
immutable `(Mode, Ruleset)` and constructs a fresh policy engine. None of these
actions can widen sandbox, offline posture, budget, workspace, system denies,
or child-agent ceilings; permit scope remains derived solely from the prepared
call. The registry remains metadata/availability/dispatch only, the
seat/controller owns modal state, and the generation coordinator owns staged
policy and the swap. After config publication it re-resolves the same frozen
admitted sources; a higher source may leave effective policy unchanged, which
the receipt reports.

The canonical metadata actions are `/name show`, `/name set TEXT`, and
`/name clear`, with bare `/name` equal to `show`; and `/goal show`, `/goal set
TEXT`, `/goal achieve`, and `/goal block REASON`, with bare `/goal` equal to
`show`. They invoke the same metadata-only services as §2's `ego resume`
spellings. A name never changes session identity. One goal may be active;
`achieved` and `blocked` are terminal and a later objective starts a new goal.
`achieve` is the operator's explicit completion attestation. `block REASON`
records one blocker observation; only the fifth consecutive observation of the
same condition makes the goal terminal, and progress or a changed condition
resets that audit.

**Tool rows.** Every tool call leaves one row in the transcript, committed
through the same interactive owner as the answer and never through the
ephemeral status row. The row is drawn when the call finishes, because it needs
the outcome and the duration: `✓ read calc.py · 2 lines · 75ms`, `✓ edit
calc.py +3 -1 · 12ms`, `✓ $ seq 1 20 · exit 0 · 0.4s`, `✗ $ make · exit 2 ·
1.2s`, `✗ read gone.py · …no such file or directory · 3ms`. `✓` and `✗` are the
outcome and a failed call never carries `✓`. An `exec` row is followed by the
last three lines of its output, and a line saying how many earlier lines were
left out. Execution (or preparation that fails before execution) and human
approval wait are measured separately with a monotonic clock and saved before
the completion event. The row shows the
execution duration and, when nonzero, `approval 15.0s`. Replay uses the saved
values. Calls without an execution measurement show `duration unavailable`.
A row is cut to the terminal width at the moment
it is drawn (a resize applies to later rows): the target gives way first, the
mark, verb, outcome and duration last. Text from a tool result is shown with
control characters replaced, never passed to the terminal.

A submitted prompt that exceeds three terminal rows shows its first three rows
and a count of hidden rows. Short prompts keep their normal echo. This changes
only the displayed receipt, not the prompt sent to the model or saved in history.
`/expand` shows the full prompt if it is the most recent expandable entry; a
later tool call replaces it as the expansion target.

`/expand`, also `Alt-O`, prints the full result of the most recent tool call
under its row: for `edit`, the line diff; for every other tool, the whole
result text the model received, up to 256 KiB. Only the 16 most recent tool calls
and collapsed prompts of the session are kept, and only the latest can be shown. It runs between turns,
like `/help`. Headless runs write each row to stderr, prefixed `εgo:`, and have
no expand key.

`/doctor` renders the whole health snapshot §7 describes, not the MCP slice it
once dispatched, and it is the same snapshot root `ego doctor` prints.

The permission prompt has a plain representation that works in every terminal
and scripted transcript: `allow`, `deny`, or `amend`. Amend reads a complete
JSON object of replacement tool parameters, then re-runs prepare and policy
from the beginning. Invalid answers re-prompt; Ctrl-C and EOF deny.

The permission details view shows pending `edit` changes as a unified diff of
replacement fragments with real line breaks. Pending `write` content appears
as additions; the prompt does not read previous file content and states that
an existing file is overwritten. Diffs show at most 30 lines, clipped to
terminal width, with an explicit omitted-line count. Colour follows terminal
capabilities, `--no-color`, and `NO_COLOR`; redirected output has no colour
escapes. Binary whole-file content shows its byte count instead of a text diff.

### 6.1 Paste

The input line runs in **bracketed paste** mode (`ESC[?2004h`, off on exit).
Every terminal in the compliance matrix implements it, and without it a pasted
carriage return submits the first line of a paste as a whole prompt — the failure that
makes pasting a stack trace impossible. A paste arrives as one edit, never as
keystrokes, so it is never interpreted as a slash command or a control
sequence.

`Ctrl+V` reads the OS pasteboard: macOS `public.png`/`public.tiff`, Linux
`wl-paste`/`xclip`, Windows `CF_DIB`. A pasteboard image, or an entire
bracketed paste that names an existing supported image file, stages a
removable chip and submits with the next turn through the same durable
`--attach` path (`ARCHITECTURE.md`).
Idle Escape removes the newest staged chip; busy Escape still interrupts
active work. Image paste is available while a turn is running. Text and
images remain one draft through stash, restore, clear, and queue recall.
Enter validates and queues the complete draft before clearing it; an
admission failure retains it. Pasteboard text, and a path that is
missing or not a supported image, remain ordinary editor text; an
unsupported or unknown model capability refuses staging before any bytes are
read; an oversized image is refused with its observed size. Image input also
uses the explicit, scriptable `--attach IMAGE_PATH|HTTPS_IMAGE_URL` grammar;
no OSC 52 read path is part of the product.

### 6.2 Availability — what ships today

Same discipline as §2.1 and §3.1. This table records the implementation that
ships today; §§6.3-6.5 are the final product contract and are not optional
scope. The current reader, trust question, permission answer path, and live
composer ship. `ego` with no subcommand opens a session whose single Reedline
owner stays active while rendered output, tools, and modal prompts cross
bounded channels.

| Surface | Current | Note |
|---|---|---|
| Input line, history, multiline | **yes** | reedline. Active `Ctrl+C` requests cancellation and reports it immediately. Idle `Ctrl+C` abandons the line; a second idle `Ctrl+C` within 500 ms exits. `Ctrl+D` ends it |
| Enter and newline | **yes** | In the terminal composer, Enter (`CR`) submits once. `LF` (`Ctrl+J`), CSI-u `Ctrl+Enter` (`ESC[13;5u`), and CSI-u `Shift+Enter` (`ESC[13;2u`) insert a newline. Non-terminal scripted input keeps LF as its line terminator |
| Bracketed paste | **yes** | on for every read, off on exit. A pasted stack trace is one prompt, not its first line |
| Ctrl+B, `/tasks` | **yes** | Ctrl+B retains the running admitted exec as a session-owned detached command without restarting it. `/tasks` lists bounded output and status; `/tasks ID` inspects one task, `/tasks cancel ID` requests cancellation. Settlements are announced once at the next generation boundary; restart reports unsettled tasks as Interrupted |
| Ctrl+V | **yes** | reads the OS pasteboard; an image stages a removable chip and submits with the next turn through the `--attach` path, text inserts at the cursor |
| Ctrl+Z | **yes** (Unix) | stops ego as a shell job with the terminal back in cooked mode; `fg` restores raw mode and repaints the composer or the open modal, which stays unanswered. A running `exec` keeps running in its own process group, and its timeout does not count the time ego was stopped. Windows has no job control: the key reports that it is unsupported |
| Permission dialog | **yes** | plain `allow`, `deny`, or `amend`; amend then reads replacement JSON parameters. Invalid answers re-prompt; Ctrl-C and EOF deny. The engine keeps the configured mode first and re-prepares/re-classifies every amendment |
| Slash commands | **yes** | one shared action registry drives typed-slash classification, completion, menu, grouped help, and local dispatch. Exact actions use one Enter; partial completion and pasted slash text do not execute accidentally |
| Live composer during a turn | **yes** | one Reedline owner accepts bounded external-printer output and preserves the exact draft through streaming, tools, permission waits, paste, and resize |
| Ctrl-S drafts, prompt queue, steering | **yes** | exact LIFO drafts, durable FIFO busy input, safe-boundary steering, expedited Esc delivery, replay, and exit accounting |
| Model/question menus and status line | **yes** | the shared model picker, typed question path, and width-aware status projection are live |
| `--attach` | **yes** | §3.1's durable image path; interactive `Ctrl+V` and bracketed image paths use the same admission and blob commit |
| OSC 133 A (prompt mark), OSC 7 (cwd) | excluded | not part of the final M5 terminal contract. `ego run` retains 133 C/D, OSC 9;4 and — under TUICommander — 7770 |

### 6.2.1 Keyboard preferences and Vim editing

The default composer uses Emacs editing. Start `ego --vim` for Vim editing,
or save `ego config set ui.vim=true` to select it for future interactive
processes. `--vim` overrides a saved `false` value. The composer starts in
insert mode; idle `Esc` selects normal mode, and `i` returns to insert mode.
The prompt shows `[INSERT]`, `[NORMAL]`, or `[VISUAL]`. Vim editing uses the
bundled Reedline editor, including its motions, operators, counts, and undo.
Headless `run` and prompted `resume` reject `--vim`; ACP has no terminal
composer and does not apply keyboard preferences.

Customize action shortcuts in the **user** `config.toml`:

```toml
[ui]
vim = true
keybindings = { model = ["Ctrl+P", "F2"], expand = [] }
```

The same map can be saved atomically:

```sh
ego config set 'ui.keybindings={model=["Ctrl+P","F2"],expand=[]}'
ego config get ui.keybindings
ego config unset ui.keybindings
```

Keys in the map are canonical slash action names from `/help`, without the
slash. Each array replaces that action's complete shortcut list; `[]` removes
its shortcuts. Unspecified actions keep their defaults: `Option+P` opens the
model picker, `Option+O` expands tool output, and `Option+R` retries provider
recovery. `/help` and the startup key hints show the effective shortcuts.
Unset `ui.keybindings` to restore all defaults.

Supported chords are `Ctrl+LETTER`, `Alt+LETTER`, and `F1` through `F12`.
Spellings are case-insensitive; `Option` and `Meta` are aliases for `Alt`.
Letter shortcuts require one ASCII letter. Bare text, mixed modifiers such as
AltGr, arbitrary escape sequences, and shifted chords cannot be configured.
This keeps committed Unicode/IME text and terminal paste outside shortcut
matching. Use a chord that the terminal can transmit; modified Enter reporting
is not required for configurable shortcuts. The macOS `π` spelling of
`Option+P` follows the effective binding, not the retired default.

Unknown action names, invalid chords, and duplicate effective chords are
refused before terminal activation. The check includes unchanged defaults;
change both actions in one map to swap their keys. Managed `config set` and
`config batch` also reject invalid maps before publication. Permission,
picker, paste, draft, suspend, and busy-input controls cannot be remapped;
terminal aliases such as `Ctrl+I`, `Ctrl+J`, `Ctrl+M`, and `Ctrl+H` are reserved.

Keyboard preferences are user/process-only. They are read before the terminal
owner starts, apply at the next process start, and remain stable when a session
or project root changes. Project, local, system, and profile settings do not
select the keyboard map; `-c` keyboard overrides are refused. These preferences
do not change the permission mode or confinement.

Vim runs **below** host input routing. Busy `Esc` still interrupts or expedites
pending input; it does not select normal mode. Modal `Esc` still cancels or
settles the current decision. Idle `Esc` removes a staged image chip before
changing Vim mode. Busy Enter queues, Ctrl+Enter steers, Up on an empty composer
recalls queued input, and Ctrl+S parks/restores drafts in either Vim mode.
Permission keys and modal text forms keep their existing editing behavior.
Bracketed paste inserts literal multiline text in both Vim modes; Ctrl+J
inserts a newline without submitting. Terminal/OS IME pre-edit is owned by the
terminal; the composer receives its committed Unicode characters.

### 6.3 Live composer, drafts, queue, and steering

The composer remains editable while a provider streams, a tool runs, or the
harness waits at an interactive boundary. Renderer output enters a seat-owned
output pump; only that pump may block on reedline's bounded external-printer
channel. Reedline appends rows above the composer and restores the exact buffer
and cursor. A finish barrier commits every accepted row before the next idle
prompt, so control input remains responsive without reordering the transcript.
In interactive mode reedline is therefore the terminal writer. In headless
modes the existing renderer remains the writer. There is never one of each
writing the same terminal.

The default interactive presentation uses one live row for consecutive successful
shell commands (`exec`) that produce no output in the same turn. Each such
completion replaces the live row. Before a failed command, command output,
another tool, agent prose, or the end of the turn, the last live command becomes
a transcript row. Failed commands and output previews remain visible. Nested
child-tool rows keep their existing grouping. `--plain`, `TERM=dumb`, and
headless output remain append-only; `--no-color` only removes colour. This
presentation does not change durable events, transcript replay/export, or the
bounded tool-detail history used by the expand key.

`Ctrl-S` with a non-empty composer pushes the exact multiline buffer onto a
session-local LIFO draft stash and clears the composer. `Ctrl-S` with an empty
composer restores the newest stash immediately. After the next ordinary prompt
is accepted, the newest parked draft returns to the composer automatically;
actions and refused submissions leave it parked. Stashes are not submitted,
do not enter model history, and are not silently overwritten; exiting with
stashes reports their count.

`Enter` on non-empty input during a busy regular turn immediately and durably
accepts a FIFO **PendingSteer**, opens a clean composer, and shows exactly:

```text
Messages to be submitted after next tool call (press esc to interrupt and send immediately)
```

Every additional busy submission joins the same FIFO. Normal delivery waits
until the current provider sampling has settled and the complete sibling
tool-call/result batch emitted by that assistant generation is durably closed,
then appends each accepted prompt as a correlated user-prompt item before the
next model request. Queue text is never inserted into a `ToolResult` and never
appears between a call and its result. If the generation emits no tool or ends
with a final response, pending input forces an immediate follow-up request
instead of being stranded.

`Esc` while pending steers exist first records an expedited prefix through the
current FIFO tail, then requests phase-safe interruption. Sampling may stop
cooperatively but its attempt still settles durably. A permission wait becomes
an explicit denial. A running non-cancellable side-effecting tool finishes and
its real outcome is recorded; every not-yet-run sibling call closes with an
explicit interrupted result. Only after the complete batch and interrupted
turn are durable are all unacknowledged steers replayed in FIFO order as one
fresh user turn. Repeated `Esc` with the same tail is idempotent; a later tail
can extend the durable expedited prefix. `Esc` with no pending input requests
ordinary cancellation immediately, including while output is under terminal
backpressure.

The three input states do not overlap. A **Draft** is a local LIFO editor stash.
A durable **PendingSteer** is FIFO input accepted during a busy regular turn. A
durable ordinary **Queued** FIFO is reserved for non-steerable or
idle-transition cases. One queue coordinator owns both durable forms and their
exactly-once consumption; there is no steering inbox or action-registry-local
queue. None of these paths bypasses budget, permission, sandbox, attempt
settlement, usage, cost, or tool-result accounting.

### 6.4 Menus, model selection, and questions

`Option-P` on macOS and `Alt-P` elsewhere, normalised as terminal `Meta-P`,
open the same action as `/model`. On macOS, a terminal that emits the composed
`π` character for Option-P opens that action too. `/model` remains the exact
keyboard-independent equivalent. The picker shows provider,
model, context capability, and reasoning effort. The cursor starts on the
current model, which carries a `✓ current` marker; the reasoning step marks the
current effort the same way. Arrow keys navigate, a digit selects by number
while no filter is typed, and typing narrows the list by substring (`Backspace`
edits; no match shows an empty state and `Enter` does nothing). `Enter` at the
reasoning step sets the default for new sessions, `s` there applies only to this
session, and `Esc` cancels. Interactive startup with a selected model whose
reasoning default is unknown opens the reasoning step directly. The operator
must choose a supported effort; `Enter` saves the model and effort, and `Esc`
cancels startup. Headless runs still require an explicit `--thinking` value.
A completed change writes a `Model: <slug>` line to
the transcript. A selection made while a request is in flight applies at the next
generation boundary, never by mutating an active request.

The picker owns no rows. regression case brings the final shared catalogue into
M1: supplied hosted rows and authoritative live provider/local discovery form
one ordered projection consumed by generation-time resolution, `ego models`,
`/model`, and `Meta-P`. `ego models --json` is its versioned non-interactive
form. Selection resolves the chosen slug again at the generation boundary, so
a stale listing cannot bypass changed credentials, capabilities, or
availability. Missing facts remain explicit and never become UI defaults.

Local control actions such as the model picker and status inspection execute
immediately while the harness is busy. The action registry only declares
whether an action is local or prompt-producing. Every prompt-producing spelling
dispatches through regression case's sole coordinator and §6.3 semantics; the
registry owns neither a queue nor an inbox. The same command therefore cannot
execute immediately from a shortcut and queue from its slash spelling.

`AskQuestion` uses the same keyboard selector for single choice, multiple
choice, and free text. A question temporarily owns `Enter` and `Esc`; the
underlying composer and queue remain intact and cannot accidentally submit.
ACP maps the question to its negotiated elicitation shape. A non-interactive
run emits the versioned `question` item and exits 3; `ego resume ID --answer
QUESTION_ID=JSON` is its explicit continuation. It never hangs or invents an
answer. On terminals without the cursor operations needed for the selector,
the same question degrades to a numbered plain prompt.

### 6.5 Native status line

The status line is owned by the interactive surface, not a shell subprocess or
model output. It renders a value-only MiniJinja template from an immutable
snapshot. Git and external providers update asynchronously and never block input.

The compact default reads as:

```text
gpt-6-sol · medium · yolo ▰▰▱▱▱▱▱▱▱▱ 58k/272k ≈$0.66 ~/G/p/mdkb ⎇ feat/configurable-embedder●
```

The ten context cells fill in proportion to measured occupancy. The meter is
green below 60%, yellow from 60%, and red from 85%. Missing occupancy reads
`▱▱▱▱▱▱▱▱▱▱ ?/272k` when the window is known. Colour is optional; the count
and window remain visible without it. `≈$` means subscription-equivalent cost
at standard API prices, not billed spend. `$` means billed cost. `(partial)`
marks an estimate that excludes mixed or unpriced usage; `unpriced` means no
complete price is available. `/statusline` explains these symbols in its setup
selector. Home is `~`, intermediate path components use their first letter,
and the final component stays complete. `⎇` precedes the branch; `●` means
uncommitted changes. These symbols are standard single-cell BMP characters,
not emoji or private font glyphs. Under width pressure, branch, cwd, and cost
drop in that order before model, reasoning, mode, and context. At widths too
small for that core, whole fields still drop rather than wrap or truncate.

Configuration stores a complete template:

```toml
[ui.statusline]
template = """
{% filter priority(20) %}{{ model }} · {{ effort }} · {{ mode }}{% endfilter %}{% filter priority(10) %} {{ ctx.percent|bar(10) }} {{ ctx.used|tokens }}/{{ ctx.window|tokens }}{% endfilter %}
{{ prompt }}
{% filter priority(0) %}{{ git.branch }}{% endfilter %}
"""
```

`{{ prompt }}` must render once on its own line. Text before it goes above the
editable buffer; text after it goes below. Filter blocks use `priority(integer)`:
lower priorities disappear first when a row is too wide. Blocks cannot nest or
contain the prompt. Unblocked rows are indivisible. Values are never truncated.
Active queue, draft and interrupt state is added even when the template omits it.
The queue field reads `steer N · next N`: `steer` counts messages that steer the
running turn; `next` counts messages queued for the next turn. Both counts are
shown when either is nonzero. When both are zero, the field is omitted.
Queue counts refresh when input is consumed, including during a running turn.
The interrupt indicator disappears when the interrupted turn settles; input
that still waits for delivery keeps its queue count, not an interrupt indicator.
At a width too small for one critical value, that value remains whole.

Variables: `model`, `effort`, `fast`, `mode`, `ctx.used`, `ctx.window`,
`ctx.percent`, `cost.amount`, `cost.equivalent`, `cwd`, `git.branch`, `git.dirty`,
`queue`, `drafts`, `interrupt`, `trust`, `goal.active`, `goal.objective`,
`goal.turns_used`, `goal.turns_max`, and `providers.NAME`. Unknown measured
numbers, inactive goal fields and an unavailable Git branch are `none`.
`queue` is pending steers plus queued prompts. `cost.amount` is the numeric
amount represented by the visible cost; `cost.equivalent` distinguishes a
subscription estimate from billed cost. `cwd` is the full working directory.

Filters: `tokens` abbreviates counts, `money` formats a dollar amount,
`short_path` abbreviates a path, `bar(n)` renders a percent with `n` cells
(maximum 256), and `color(name)` accepts `green`, `yellow` or `red`.
`text|threshold(ctx.percent)` applies the context thresholds to text. Colours
respect the terminal colour policy and add no display width.

With no template configured, `{{ compact }}\n{{ prompt }}` is the built-in
default. `compact` preserves the 524 layout, including its width priorities.
`/statusline` presets write templates. The Off preset stores `{{ prompt }}`;
critical operational state remains visible. The unreleased `segments` key is
removed, without compatibility parsing. An invalid template retains the compact
default and emits one diagnostic with the error and source line. The environment
has no filesystem loader, network functions or subprocess functions. Source is
limited to 65536 bytes, output to 65536 bytes, evaluation to 10000 fuel units,
and recursion to 32 levels.

The live Working row also shows an approximate output-token count from model
text (four characters per token, rounded up), not provider usage. Tool output is
excluded. Reasoning deltas start a thinking interval; answer text, tool starts,
and turn completion clear it. A new turn resets progress. The row is bounded to
the terminal width and omits the interruption hint when it does not fit.

### 6.6 Prompted compaction starts a clean session

`/compact` is an idle-only session transition. The final M3 contract rejects
`--ephemeral` before every effect, so every admitted compaction source is
durable and independently resumable. It sends the
complete active provider context to one tool-less generation with a versioned
εgo-owned prompt that asks for a self-contained continuation checkpoint. That
checkpoint keeps
the active objective, binding constraints, completed evidence, current work,
blockers, uncertainty, decisions, rationale, exact references, and ordered next
steps. An earlier checkpoint is part of the input, so repeated compaction must
preserve every still-relevant fact.

A successful response creates a new durable session and switches the
interactive surface to it. The receipt names both session IDs, reports the
source context reading, and gives the command to resume the source. The
successor occupancy remains unknown until its next generation; checkpoint
length does not measure its freshly resolved provider baseline. A tip `/fork`
prints the same session identities and preserves the measured source context.
The new session resolves ordinary system, project,
runtime, model, and tool context afresh. The only conversation material carried
from the source is one distinct context-checkpoint block containing the
compaction response: no old user, assistant, reasoning, tool-call, tool-result,
or retained-tail item is replayed. The checkpoint is context, not a
user-authored prompt or higher-authority instruction.

The source session remains complete and resumable. The target records the
source session and source sequence. Event parents stay within their session:
`PreCompact` belongs to the source, while `PostCompact` starts a root occurrence
in the clean target. Both hooks retain the compaction request identity; target
lineage records the source boundary, not an event-parent edge into its log.
This also applies after automatic goal continuations. Metadata operations and
cold resume validate the target without replaying the source conversation.
A tool-use or truncated response, empty or
invalid checkpoint, provider/budget/cancellation failure, target-baseline
capability or encoding failure, or pre-publication target-store failure creates
no usable target and leaves the operator in the source session. At compaction
time εgo resolves and locally validates the exact checkpoint-only baseline; it
does not claim token-level fit for that baseline or for a future human prompt.
A later provider context-limit error remains explicit. The source owns the
compaction attempt's usage and cost. The action is disabled while a generation,
permission, tool batch, or accepted prompt is active; saved drafts require
confirmation and remain with the source. The complete decision and prompt
contract are in
`ARCHITECTURE.md`.

#### Transition identity, output, and host parity

Both `/compact` and the headless spelling invoke one
`CompactionTransitionService`. They mint a fresh request ID by default. The
optional `--request-id` is an advanced retry override that reuses a known
idempotency identity. The exact headless grammar is:

```
ego compact ID [--request-id UUID] [--output text|json|jsonl]
            [-C DIR] [--session-dir DIR]
```

There is no alias. The target is deterministically derived from the request
ID; that same value is the
`successor_start_request_id` a later Main start uses to adopt the event-first
target. Repeating the same request after a lost response inspects and confirms
the matching target without another checkpoint generation or charge. Reusing
the ID for another source or target is a no-effect identity collision.

Text output is the target session ID followed by a newline, matching `ego fork`.
JSON and JSONL each emit exactly one version-1 receipt; JSONL terminates it with
one newline:

```json
{
  "v": 1,
  "source": "019...",
  "source_seq": 42,
  "target": "019...",
  "publication": { "kind": "published_durably" },
  "successor_start_request_id": "019..."
}
```

`publication.kind` is `published_durably` or
`published_durability_uncertain`; `diagnostic` is omitted when absent. Invalid
grammar is exit 2 and a provider failure is exit 4. Busy, collision, target
inspection, and `not_published` outcomes exit 1 without exposing a usable target.
A durability-uncertain target is already authoritative: the command emits the
receipt, exits successfully, writes the warning only to stderr, and neither
retries nor rolls back.

An exact provider context overflow is a distinct settled provider failure, not
ordinary output length or generic invalid input, and is never blindly retried.
The operator then invokes `/compact` or `ego compact`; M3 introduced no
automatic threshold.

ACP v1 advertises the optional `_ego/compact` extension in
`AgentCapabilities._meta`. The durable lifecycle adapter also advertises
standard load, list, tip fork, resume, delete, and close. Its request is
`{"v":1,"sessionId":"...","requestId":"..."}`; its response carries
`v`, `sourceSessionId`, `sourceSeq`, `targetSessionId`, `publication`, and
`successorStartRequestId`. A successful response registers the target so the
next standard `session/prompt` adopts it. The extension itself adds no standard
lifecycle operation; those are served separately by the durable M5 adapter.
Because ACP has no
standard context-overflow stop reason, the preceding `session/prompt` returns a
structured application error naming `_ego/compact` after the typed settlement
is durable; it does not report max tokens or refusal.

At M3 `ego mcp-server` exposes five tools: `run`, `continue`, `cancel`, `status`,
and `compact`. The compact input is exactly
`{"request_id":"...","run_id":"...","expected_revision":N}` and its
version-1 result carries `source_session_id`, `source_seq`,
`target_session_id`, `publication`, and `successor_start_request_id`.
Visibility and revision checks remain mandatory. Application failures return
MCP `isError: true`; published uncertainty is a successful non-retryable tool
result, and `status` projects the durable `context_overflow` settlement. The
source session owns every compaction generation's usage and cost,
and the target receives no copied ledger or run state.

ACP `session/load` also displays accepted prompts that survive only in durable
run receipts. A semantic user prompt takes precedence, so load does not display
it twice. Recorded interrupted or uncertain outcomes appear as historical error
cards. This display projection does not add semantic events, assistant replies,
or provider context. A session with no accepted prompt still has no transcript.

### 6.7 Conversation forks

`ego fork ID --at ITEM_ID` and `/fork [message-id]` branch after the successful
completed turn containing that durable user or assistant message. The full answer
is included. Unknown or foreign items, non-message items, failed or incomplete
turns, and compaction checkpoints are refused without creating a target. Messages
hidden behind a compaction checkpoint cannot be selected. An inherited message
branches its owning ancestor at that ancestor's completion sequence. Title and
active goal come from the selected boundary. An explicit completed boundary can
be read while the source writer runs; a tip fork requires an inactive source.

Standard ACP `session/fork` serves the tip when no selector is supplied.
Negotiated `_meta.ego.atMessageId` selects a completed-message boundary;
lineage metadata and inherited-message replay are also implemented. See
[ACP history metadata](docs/integrations/acp-extensions.md#session-and-history-metadata)
and the source owners `crates/ego-acp/src/serve.rs` and
`crates/ego-acp/src/project.rs`. The accepted cache design snapshots only the system prompt
per epoch; resume/fork inherit it, and compaction or explicit refresh replaces
it. Tool admission remains current. The snapshot is captured on the first admitted generation. `/refresh` reloads
instructions and opens a new epoch on the next admitted generation; it refuses
while source work is pending. Live runtime and host facts are transient data
before current user intent. Provider cache usage and Codex continuation are separate observations; see
[Codex transport](docs/guides/providers.md#codex-transport), with decisions in
`ARCHITECTURE.md`.

## 7. Configuration

```
/etc/ego/config.toml      system    (admin: fleet defaults + non-relaxable
                                     deny/budget ceilings — the only source
                                     of ceilings; ego itself imposes none)
~/.ego/config.toml        user      (EGO_HOME overrides ~/.ego)
.ego/config.toml          project   (committed; no secrets)
.ego/config.local.toml    project-local (gitignored)
```

TOML, flag-mirroring keys, `[profile.NAME]` sections selected with
`--profile NAME` (Codex's idea — one flag flips a whole posture). The `ci`
profile is a **contract**, not detection: explicit trusted config, headless
credential sources, isolated session/artifact dirs, no prompts, sanitized run
bundle. It activates only through `--profile ci` or `EGO_PROFILE=ci`; ambient
`CI` never activates it. Its exact root configuration is:

```toml
[profile.ci]
session_dir = "/absolute/precreated/session-root"
artifact_dir = "/absolute/precreated/artifact-root"
```

Both roots must be absolute, pre-existing real directories with non-symlink
final components. After canonicalization, the workspace, session root, and
artifact root must be pairwise disjoint, including containment. CI is accepted
only by headless `ego run`/`ego resume`, requires explicit `--instructions`, a
prompt supplied without an interactive path, and an already non-interactive
credential source. Every admitted terminal outcome publishes exactly one
canonical sanitized bundle while the session writer lease is held; a
pre-admission refusal publishes none. Publication is never retried, deleted,
or rolled back after uncertainty. JSONL stdout remains the ordinary parseable
live event stream and diagnostics remain on stderr; the separately published
bundle is the sanitized artifact.

Precedence: flags > env (`EGO_MODEL`, `EGO_PROFILE`, …) > local >
project > user > system, with system ceilings binding everyone. Credentials
resolve through a source chain (OS keyring, systemd credentials, env,
`credential_command` for Vault/SOPS, device-flow OAuth) via `ego auth` —
never plaintext in config. M5 resolves provider material into one non-cloneable
zeroizing owner: discovery borrows it, generation transfers it, and the guarded
transport owns only the shortest zeroizing header presentation. Events,
manifests, diagnostics, traces, and request digests carry status/source facts,
never secret bytes.

All config and rule mutations use the one atomic transaction from the accepted
config ADR. They write only `$EGO_HOME/config.toml`, falling back to
`~/.ego/config.toml`, and preserve unrelated keys, comments, and ordering.
Typed no-ops publish nothing; every changed result strictly reparses the full
document before publication. A now-empty managed table is pruned rather than
left to shadow lower authority.

`ego config edit` edits the whole user config document, including profiles. It writes a mode-0600
sibling candidate, prints that path to stderr, selects `$VISUAL` then
`$EDITOR`, and parses the value with the documented lexical quoting and
escaping rules. The path is appended as the final argv element. There is no
shell, expansion, globbing, or guessed editor. Editing occurs outside the
config lock. Publication reacquires the lock, compares the original digest,
strictly parses the complete candidate, and atomically replaces the file. CI
filesystem invariants and system ceilings remain run/resume admission and
resolution behavior; edit does not execute or rewrite them. Invalid content or a changed base preserves and reports the
candidate. Launch failure, non-zero exit, cancellation, or unchanged content
removes it. A post-rename durability failure has already published the new
authority: it reports a warning and recovery copy without retry or rollback.
Orphan candidates from a crashed parent are never silently removed.
The config transaction ADR's outcome matrix fixes exit, stdout, stderr, and
candidate retention for every editor result.

System policy can also set `max_mode="accept-edits"`, using the order
plan < default < accept-edits < auto < yolo. A selected mode above the maximum is
refused before run effects, including live ACP and REPL selections. A refused
selection preserves the current and pending mode. `max_mode` is system-only and does not change
the default mode; a plan maximum must be paired with `mode="plan"`.
A system `sandbox` value is a confinement floor: lower layers and selected
profiles may tighten it but cannot weaken it.

System `roots=[{path="/path",access="read"}]` bounds extra roots and
configured exec paths after canonicalization. `roots=[]` allows no extra
roots. The workspace remains unchanged. System profile roots also bound
that profile. Broader roots and write access under a read-only bound are
refused; multiple system declarations all bind. These ceilings do not
grant access by themselves.

Child process controls are opt-in. `ego config set 'exec.env="inherit"'`
keeps the full inherited environment (the default). `exec.env="core"` keeps
PATH, HOME, USER, LOGNAME, SHELL, TMPDIR, TMP, TEMP, LANG, LC_ALL, LC_CTYPE,
SYSTEMROOT, WINDIR, COMSPEC and PATHEXT if present. `exec.env="allowlist"`
keeps only names in `exec.env_allowlist`, for example
`ego config set 'exec.env_allowlist=["PATH","HOME"]'`; an empty list keeps none.
These controls apply to built-in exec children, including rebuilt tool registries.
They do not change provider credentials, MCP declarations or hook environments.
Base and profile fields use normal per-field precedence; unset restores inheritance.

`ego config set 'network="off"'` applies the existing `--offline` guarantees
and capability checks. `network="on"` is the default and never clears an
explicit `--offline` invocation.

`--read-only-dir DIR` grants reads of one session upload directory at launch.
It is available to interactive runs, `run`, `resume`, and served protocols.
DIR must be an existing canonical absolute directory: relative paths, `..`,
symlink aliases and home/administrator ancestors are refused. Nested directories
beneath a writable workspace or root follow the platform rules below. This launch grant does not make
configuration `roots` writable from flags. System and host profile ceilings
still bind; credential denials remain in force. The grant adds neither writes
nor executable uploads. It does not change the selected sandbox level. On Linux, confined read roots
require Landlock, since read-only bind mounts alone permit execution.
The session perimeter lists the read root, and startup reports the read-only
launch directory. ACP also reports `_meta.ego.readOnlyDirectory` on creation.

ACP `additionalDirectories` accepts at most one directory with the same read-only
contract on `session/new`, `session/load`, `session/resume`, and `session/fork`.
A process-wide `--read-only-dir` and the ACP directory must name the same path.
The directory is pinned for the session and retained in durable run selections;
a later request cannot replace it or add one after admission. Hosts must supply the launch grant again
when loading a session that has not yet started a run. Paths inside the grant
are still canonicalized by each built-in tool, so symlinks cannot open siblings.

Effective perimeter inspection warns when canonical roots overlap the workspace.
A read root beneath a writable root or the workspace is admitted. Built-in tools
keep that subtree read-only, even if a deeper write root exists. On macOS,
Seatbelt denies writes and execution in the subtree; later grants cannot reopen
it, and its ancestors cannot be renamed to bypass the restriction. On Linux,
Landlock grants are additive: the writable ancestor wins for exec children,
including execution in the read subtree. Ego reports that limitation without
exiting; built-in tools still enforce read-only access. With `sandbox=ro`, the
global native write denial remains in force. With `sandbox=off`, native writes
are not confined. A missing or unresolvable configured path is reported as a
warning and remains an admission error; it is never shown as an admitted root.

`/config` lists Model, Mode, Sandbox, Perimeter, and Budget from the session's
captured startup configuration. Each declaration names its origin layer and
any overriding layer, including the active profile. Independent money and token
ceilings remain visible; system clamps and ignored project layers are reported.
The admitted perimeter is printed beneath the settings.

`/config set KEY TOML_VALUE` uses the same strict parser and atomic transaction
as `ego config set KEY=TOML_VALUE`. For example, `/config set mode "plan"` or
`/config set budget "20k"`. It saves the user base layer, preserves unrelated
TOML and comments, and reports publication and durability. Saved defaults apply
to future sessions; current model, mode, budget, and confinement stay under their
existing session authorities. An active profile or higher layer can override a
saved default; the receipt names that layer. Use `/model`, `/thinking`, or `/mode`
for live selection, and `ego config set --profile NAME KEY=TOML_VALUE` for explicit
profile edits. Config edits are unavailable while a turn is running.

`ego config ls --effective` and `/config perimeter` show the same perimeter
block as doctor: canonical workspace, admitted profile roots and their access
and origin layer, sandbox selection and backend, capability evidence, and
network mode. The interactive command projects the session's existing
confinement and admitted roots; it does not reload configuration. Headless
inspection uses the current directory and untrusted configuration resolution.
Session inspection reuses the current backend observation and committed probe
transcript from the shared behavioral suite; it executes no additional probes.
In effective-perimeter JSON, `capabilities` is an array of measured guarantees:
`read_scoped`, `write_denied`, `write_scoped`, and `no_ip_network`. An empty array
means the probes completed without establishing a guarantee. `backend` names
the observed backend, including `bwrap` when selected on Linux. `probe_evidence`
is the existing session transcript reference (`<sha256>:<bytes>`).
Without a completed observation and probe transcript, `capabilities` is the
string `not_checked`, and `capabilities_reason` explains why. Measured views omit
`capabilities_reason`; unmeasured views omit `probe_evidence`. Headless
`config ls --effective --json` and doctor have no session measurement and remain
`not_checked`. A mechanism name or configured sandbox is not proof of enforcement.
Profile roots reach both permission policy and exec confinement; sandbox `ro` still denies all writes.
Doctor may name admitted roots in this block only; all other diagnostics keep
the existing path redaction policy.

`ego doctor [--json]` and `/doctor` are two renderings of one ordered,
versioned `HealthSnapshot`: config, credential status, session store,
sandbox/backend, provider/model catalogue, MCP, skills, hooks, and the ACP
surface this build advertises — the protocol version it serves and each `_ego`
extension with its version, which is the one fact about ego that only a second
implementation could previously discover. Doctor is
non-mutating and redacted. It may read existing facts and perform bounded local
or loopback checks; it never logs in, refreshes credentials or catalogues,
reloads extensions, edits config, probes a hosted provider, uploads logs, or
widens egress. `ok`, `warning`, `error`, and `not_checked` stay distinct; root
exit is 1 only when an operational error exists. Two facts ego cannot read
without mutating something are reported `not_checked` with the reason and the
command that would establish them: sandbox capability, which is measured by
spawning probe children, and the provider catalogue, whose refresh may reach
the network.

### 7.1 Operator-supplied identity headers

**ego does not ship them.** No request ego builds claims to be Claude Code,
the Copilot Chat extension, or VS Code. Where a row declares a client at all it
declares ego — `User-Agent: ego/<version>`, plus `originator: ego` where the
Codex backend reads one. Most rows declare none: `anthropic`, `openai`,
`gemini`, `xai`, `xai-oauth`, `nous`, `openrouter`, `minimax-oauth` and
`ollama` send no `user-agent` at all, and nothing downstream fills one in.
Sending no name is not the same as sending someone else's, which is the whole
of what was removed. maintainer ruled it on 2026-09-05
(`ARCHITECTURE.md`
§1 and §3, amended), and a test fails the build that reintroduces one.

Some vendors gate a subscription flow on their own client's identity, so ego's
honest one can be answered with `401` or `403`. ego relays what the vendor said
verbatim and names this section. Sending the vendor's identity anyway is the
operator's decision, and so are the terms it interacts with. `[auth.<provider>.headers]`
is the whole mechanism — the same table `credential_command` lives beside,
overlaying the row's declared headers by case-insensitive name, with an empty
value deleting one:

```toml
[auth.anthropic.headers]
user-agent = "claude-cli/2.1.212"
x-app = "cli"
anthropic-beta = "claude-code-20250219,oauth-2025-04-20"

[auth.github-copilot.headers]
user-agent = "GitHubCopilotChat/0.35.0"
editor-version = "vscode/1.107.0"
editor-plugin-version = "copilot-chat/0.35.0"
copilot-integration-id = "vscode-chat"
```

`ARCHITECTURE.md` §7.8 carries the same values with their pinned
client source, the vendor terms each one touches — Anthropic reserves OAuth to
its own applications; GitHub's four are a VS Code extension version gate — and
which requests it reaches. It reaches all of them: generation, `ego auth
login`, and the refresh that follows, the Copilot token exchange included.

Overlaying `authorization`, `x-api-key`, `content-type`, `accept`,
`openai-beta` or `chatgpt-account-id` is refused at config load: the first two
are the credential and config holds no secrets, the rest are wire facts. Nothing
in this table reaches the credential store — `ego auth status` reports an id, a
kind and an expiry, never a header.

### 7.2 `models.toml` — hosted model capabilities, in three layers

Per-model execution tier defaults use a separate map in the system or user model
file. `standard` is the default; `fast` is refused unless the shared catalogue
has verified support and pricing for that full slug:

```toml
[service_tier]
"openai/gpt-6.1-sol" = "fast"
"openai-codex/gpt-6.1-sol" = "standard"
```

`/fast on` and `/fast off` override the current model's tier for this session at
an idle boundary. The receipt names the token-price premium and any distinct
included-subscription usage multiplier. The status snapshot exposes `fast`, and
the default status row includes it when enabled. Token quotes and Codex's
separate API-equivalent estimate use the selected tier; unavailable prices stay
unknown, not zero. `ego models` lists verified fast support and its multiplier.
See `ARCHITECTURE.md` for provider evidence and account limits.


Hosted providers do not tell ego what their models can do, so those facts are
*supplied*: stated by an authority rather than guessed. There are three, and
the weakest is compiled into the binary:

```
builtin                   (compiled in; the catalogue revision below)
/etc/ego/models.toml      system   (admin)
$EGO_HOME/models.toml     user     (EGO_HOME overrides ~/.ego)
```

They merge **per slug**, strongest wins. Overriding one model changes nothing
about the others, and the `SOURCE` column of `ego models` names which of the
three won for every row — so a surprising capability is one command away from
its origin.

The same files can contain explicit first-run defaults. These merge per
provider, independently of capability rows:

```toml
[default_model]
openai = "openai/gpt-5.6-sol"
anthropic = "anthropic/claude-opus-5"
```

The builtin choices above are ego policy. A stronger layer replaces only the
provider keys it names; omitting a key does not delete an inherited default.
Each value must be a full model slug belonging to its provider key.

When configuration has no selected model, interactive startup observes
credential presence and performs bounded Ollama discovery. Exactly one usable
provider with an available mapped default is selected and saved atomically
before execution. Otherwise startup opens the shared model picker. An explicit
or configured model bypasses this first-run selection. Headless startup never
chooses or persists a model; its refusal names detected providers and remedies
using `--model` or `ego config set model`.

The builtin layer exists so that a machine with a configured credential and no
files can run. It carries only release-pinned facts from each stated authority,
cites the source and read date beside every provider's rows, and **states no
prices**: a rate is the one fact whose staleness costs money instead of raising
an error. Missing operator and cached rates do not refuse generation: unknown
price stays unknown, with a visible anomaly notice. It lists only
the **current model generations** of each provider, generated from a pinned
models.dev snapshot (`crates/ego-cli/catalogue/models.generated.toml`, rows with a
provenance line each; the `model-catalogue-refresh` skill refreshes it). It ships
rows for the providers whose resolver reads this file: `openai`, `openai-codex`,
`anthropic`, `gemini`, `github-copilot`, `xai-oauth` and `minimax-oauth`. An older
model a vendor still serves is stated in a stronger layer. `openrouter` is
selectable but has no builtin rows: it proxies hundreds of third-party models.
OpenRouter reads reasoning support from the public model inventory's
`supported_parameters` and per-model `reasoning.supported_efforts` and
`reasoning.default_effort`. It admits only published effort levels; a reasoning
parameter without an effort list does not prove that any particular level works.
Without that parameter, an unspecified effort defaults to off with a visible
notice; an explicit non-off effort is refused. Off sends the unified `reasoning.enabled = false` switch, not an effort.
Operator-supplied model facts still override discovery.
Ollama and `kimi-coding` enumerate their own models; `nous` is operator-supplied.
See `ARCHITECTURE.md`.

The `openai` rows are shipped and selectable. The row speaks the public
Responses contract, and
`ARCHITECTURE.md`
admits a public row on a published contract with passing deterministic
fixtures. It amends
`ARCHITECTURE.md`,
which required a sanitized recording of a live call first: no such recording
exists, and withholding the row made every operator's key useless against a
documented endpoint to avoid an error that announces itself — a wrong Responses
body is a `400` from OpenAI on the first call. `ego models` lists the row as
available once `OPENAI_API_KEY` resolves and as `missing_credential` when it
does not.
See `ARCHITECTURE.md`.

`openai-codex/gpt-5.6-sol` is the current built-in ChatGPT/Codex subscription
default. It becomes available after `ego auth login openai-codex` stores its
OAuth credential; no user `models.toml` is needed. Its 128,000-token output
ceiling is the public model fact, while its 272,000-token context ceiling is
the smaller effective limit from the authenticated Codex catalogue, not the
1,050,000-token Platform API limit. This makes preflight conservative for the
actual subscription endpoint. The subscription has no built-in monetary rate;
it uses subscription metering. Its currently expressible reasoning default is
`low`; image input, opaque reasoning-state carryover, parallel tool calls, and
hard tool limits are not claimed for the private subscription contract.
See `ARCHITECTURE.md`.

A full entry, with every field the schema accepts:

```toml
# `provider/model`. The model half is what goes on the wire.
[model."openai/gpt-5.6-sol".capabilities]
schema_version = 6                 # required; rejects an older document
max_context_tokens = 1050000       # omit if the vendor does not state it
max_output_tokens = 128000
# max_tools = 128                  # most tools accepted in one request
supports_parallel_tool_calls = true
reasoning_carryover = "unsupported"   # | "same_provider_only" | "portable"
# reasoning_state_dialect = "anthropic-messages"   # with same_provider_only
reasoning_efforts = { only = ["off", "low", "medium", "high", "xhigh", "max"] }
                                   # or "unknown" (fails closed) or "any"
default_reasoning_effort = "medium" # optional; omission means unknown
cancel = "cooperative"             # "unsupported" | "cooperative" | "immediate"
cache = "short"                    # "none" | "short" (5m) | "long" (1h, user config only)
transport = "sse"

# `image_input` is either a bare string — `"unknown"` (the default, and it
# fails closed) or `"unsupported"` — or this table. There is no
# `kind = "unknown"`.
[model."openai/gpt-5.6-sol".capabilities.image_input]
kind = "supported"
accepted_media_types = ["png", "jpeg", "webp", "gif"]
max_images = 100
# Omit any limit the vendor does not state. Every one is optional, and a
# stated limit above the product's own ceiling is rejected at load.
# max_bytes_per_image = 10485760
# max_total_bytes = 67108864
# max_width = 8000
# max_height = 8000
# max_pixels = 100000000

# A slug the provider has switched off. Optional, and only the builtin layer
# ships them today: it turns "no capabilities are configured" — which says
# *write a file* — into the truth, which is *use this instead*.
[retired."anthropic/claude-3-opus-20240229"]
replacement = "anthropic/claude-opus-4-8"
retired = "2026-01-05"
```

Omit a field rather than guess it: a missing ceiling stays unknown, and
`kind = "unknown"` refuses images rather than inventing a media-type list.
`ego models --json` emits the resulting capabilities verbatim, which is the
fastest way to check that a file said what you meant.

**The `ego models` cache is not permission to generate.** Its rows are a
listing snapshot, and the `FRESHNESS` column says `current` or `cached`
precisely so a stale one is visible. Every generation boundary re-resolves the
model against these files, the live provider row and the current credential —
a cached row is never handed to a request
(`ARCHITECTURE.md`).
Only supplied rows come from this file, and they are rebuilt on every
invocation rather than cached at all; the cache holds only *discovered* rows,
from providers that enumerate themselves.

An obsolete discovery-cache schema is ignored at startup. Supplied facts and
bounded local discovery remain available, and an ordinary catalogue read can
rebuild the cache without `--refresh`. Startup does not replace the old bytes.
A corrupt or newer-schema cache still reports an error and is not overwritten.

### Interactive session picker

On a human terminal, `/sessions` and `ego resume` without an ID open the saved
sessions across project roots, newest first. `/sessions` excludes the active
session. Rows show title, model, updated local time, recorded root, and summary.
Type to filter title, model, or summary, use Up/Down to select, Enter to resume,
and Esc to cancel. Missing roots remain visible as `[unavailable]` and cannot
be selected. `/history QUERY` searches the conversational FTS5 index and opens
the same picker with the matching sessions.

The next prompt uses the selected session's durable context. Switching to another
root re-enters normal run admission with that root: project instructions, config,
trust, and tool confinement are rebuilt. Within the current root, the active model
selection stays in effect. Sessions with unfinished work require `ego resume ID`,
which owns recovery. Pending source work prevents switching. Sequential stdin keeps
the `/sessions` table; headless `ego resume` without an ID keeps latest-in-workspace
selection. `ego ls` and machine output keep their existing format.

When current configuration or flags select a different model from the saved
session, resume prints both model references before the next turn. An explicit
quiet launch suppresses this diagnostic; model precedence does not change.

On human-terminal resume, the transcript replays the most recent 10 user turns
before the composer, using canonical final answers and the live compact tool
renderer. A line reports how many earlier turns were omitted. Display replay
adds no events or usage. Known transcript items that cannot be rendered show
`[transcript item unavailable]`; unknown durable records keep the store's
existing recovery refusal.

### Perimeter roots and profile presets

Without an explicit roots list, the perimeter includes the workspace and
`~/Gits` read-write. If `~/Gits` is missing, a `default-root-missing` warning
reports the workspace-only perimeter. Explicit lists replace that default;
`roots=[]` selects workspace-only. The workspace remains writable under
`sandbox="workspace"`, including within a read-only extra root.

`ego roots add [--profile NAME] [--access read|read-write] PATH`,
`ego roots rm [--profile NAME] PATH`, and `ego roots ls [--profile NAME] [--json]`
manage the exact stored user target. Add defaults to read, updates an existing
path in place, and rm matches its stored spelling. Text listings show access and
path columns; an absent list and an explicit empty list have distinct explanations.
Mutation receipts use plain language and remind the user to exit and resume
running sessions before saved roots can apply. `--json` retains the versioned
stored-config object: `values.roots` is `null` when no list is stored, `[]` for an
explicit empty list, and an array for stored roots. `ego config get roots` still
prints the raw stored value. Each edit is one locked,
validated atomic transaction; the first edit creates an explicit list.
`ego config set 'roots=[{path="~/Gits",access="read"}]'` and its `--profile`
form replace the list. Single-file read roots are supported on Seatbelt and
Landlock; paths and symlinks are canonicalized before admission. Landlock blocks
content writes but does not block timestamp changes to existing files. Project/local
roots may only restrict existing operator reach to read access; they cannot
add authority, and untrusted project configuration remains ignored.

Presets are ordinary profile contents in the user config, selected explicitly
with `--profile NAME`. No preset schema or implicit open selection exists:

```toml
[profile.strict]
sandbox = "workspace"
roots = []

[profile.tree]
sandbox = "workspace"
roots = [
  { path = "~/Gits", access = "read-write" },
  { path = "~/.codex/ORCHESTRATION.md", access = "read" },
]

[profile.open]
sandbox = "off"
roots = []
```

The tree allowlist is explicit: include only existing files/directories needed
by the agent (for example `~/.codex` or `~/.claude` instead of the single file).
`~/Gits/AGENTS.md` is already covered by the writable tree. Selecting open
always prints `open-perimeter`: exec children have unrestricted filesystem
access. Permission mode remains separate; open is adjacent to yolo, not a mode
change.

Production capability probes need a temporary path outside every granted root.
If TMPDIR lies within a granted root, sandbox startup refuses with
`perimeter-probe-temp-overlap` rather than weakening verification or silently
falling back. In particular, a tree run needs TMPDIR outside `~/Gits`.

### Deterministic orchestration inspection

`ego orchestrator brief [--root PATH]` reads the native TUIC session list,
tracked children and result files newer than the watcher stamp. It shares the
bounded child and unread-path projection with `orchestrator_brief`. It does not
resolve a model, consume inbox mail, prune tracking or change the watcher stamp.
The root optionally restricts the child subtree. A missing socket, failed host
read or malformed response is an explicit command failure.

ACP out-of-turn cards append a typed `notice_delivered` record to the existing
session stream after the transport accepts the notification. It carries the
card ID, optional source path and timestamp, and is fsynced before the pump can
acknowledge its pending batch. This is transport enqueue evidence, not a host
render acknowledgement. A journal failure returns an error; replay can enqueue
the same stable card ID again. These facts create no conversation turns and
sanitized exports retain IDs/timestamps but omit source paths.

### Bounded reads and automatic context recovery

The `read` tool returns at most 2000 lines or 32 KiB of original file bytes,
whichever comes first. A truncated page names its next original-byte `offset`.
Pass that offset with the same `path` to continue, including inside a long line.
UTF-8 characters are not split at the page boundary. Small files keep their
plain-text result. The guarded filesystem read and its permission checks do not
change; this is a provider-visible output bound, not a filesystem memory bound.

When a session is reopened after a process or host crash, attempts without a
durable settlement are recorded as interrupted (`Aborted`, `PossiblySent`).
A notice names each interrupted attempt and warns that its request may have
reached the provider. Ego does not replay that request automatically. The next
explicit user request can run in the same session. Recorded usage is retained;
missing usage remains unknown, not free. This also applies to ACP continuation
and clean-session compaction. Live attempts in the current process are not
interrupted by opening another turn coordinator over the same journal.

Durable CLI interactive sessions, including TUIC-managed terminal sessions,
automatically invoke the existing clean-session compaction operation at 90% of
the last measured provider context window. An ongoing tool turn yields before
another generation at that threshold. At the next idle boundary the host seeds
a new durable session, activates it, and continues from the checkpoint without
adding a synthetic human prompt. No source conversation tail is copied. Fresh
instructions, tool definitions, accounting, and auditable lineage use the same
transition as `/compact`.

Pending prompts, questions, or live attempts prevent the transition. A failed
automatic compaction at measured context pressure stops the session with an
explicit diagnostic and a nonzero process exit, so managed parents can observe
the failure instead of sending prompts that cannot run. The durable source is
retained for resume; no automatic retry loop runs in that source epoch. Explicit
operator cancellation and failed manual compaction remain interactive. Unknown
occupancy is not estimated. A known full context still refuses
before any request, including a compaction request: automatic recovery must
start before exhaustion, and the threshold is not an exact fit guarantee for
unsent growth. A 272000-token window triggers at 244800 measured tokens.

ACP keeps explicit recovery because it has no standard session-transition
contract. One-shot headless runs keep explicit recovery because they do not own
an interactive active-session transition. Ephemeral sources cannot compact.
Human terminal sessions use the same automatic policy as managed terminals, so
recovery does not depend on detecting a terminal's operator.

### Request output and reservation bounds

`ego run` and `ego compact` accept `--max-output-tokens N`, where `N` is a
positive integer. The same `max_output_tokens` key is accepted in configuration
and named profiles. Each generated request uses the smaller of the configured
cap and the model output maximum. Without a cap, ego uses the model output
maximum, not the context window. A missing output maximum leaves the monetary
reservation unbounded and raises a cost anomaly notice. System output caps cannot be relaxed by lower
configuration layers. `ego compact --budget LIMIT` uses the same spend grammar
and admission gate as `run`.

Currency admission bounds text input by serialized UTF-8 bytes, including
message framing and tool schemas, capped by the model context window. Image
and opaque reasoning inputs retain the full context-window bound because ego
has no provider-independent maximum for those inputs. Input uses the dearest
reachable rate, including cache creation; output uses the request cap. Actual
provider usage settles the reservation after the response. Missing, offline or
unusable table prices do not refuse model resolution. One durable `cost_anomaly`
notice per session/model states that any configured cap cannot cover unpriced
spend. The notice is visible in text (including quiet mode), JSONL and ACP.
Unknown actual cost charges the reserved upper bound when available, not a
fabricated actual cost. Known and estimated spend remain subject to the cap.

### Checked session goals (2026-10-15)

An active goal drives work after each successfully completed turn. An independent,
tool-less small-model generation checks the objective against session evidence.
This generation uses the normal session budget gate, usage accounting and response
evidence store. A passing check records `check` attribution and evidence, including
for operator-owned goals. `/goal achieve` remains an operator attestation.

An unmet condition admits a durable `goal_continuation` system message, never an
operator prompt. Five consecutive observations of the same normalized condition
block the goal. A changed condition resets that count. The default allowance is
20 automatic work turns per goal identity; `--goal-turns N` sets it when
checks first start. The allowance and consumed count survive resume. Internal
continuations count toward `--max-turns`. `ego run --goal TEXT` (also `ego resume ID --goal TEXT` and
`ego continue ID --goal TEXT`)
sets an operator-owned objective. The `goal` tool sets or updates a model-authored
objective, but cannot replace an operator-owned active goal. Text output shows a
`Goal (operator|automated): OBJECTIVE` receipt when a goal first appears and when
the model updates it. Each automatic turn shows `Goal turn n of N: CONDITION`
before its work output. A goal-check stop shows the objective, turns used, stop
reason, last unmet condition and next actions: `/goal show`, `/goal achieve`,
`/goal block`, and `--goal-turns N` (which sets the allowance when checks first
start, not after it is exhausted). These receipts remain visible with `--quiet`;
headless text sends them to stderr, not answer stdout. JSON/JSONL and ACP keep
their existing structured contracts.

The native status row includes `goal n/N` while a goal is active, including when
a check stop retains that active goal. Until checks configure the durable
allowance, the denominator is `pending`. The segment uses the existing
width-bounded renderer; a disabled status row stays disabled. Achieved and
blocked goals do not retain this active segment.

Provider, permission, pause, cancellation and budget stops do not admit automatic
work. Invalid checks and exhausted allowances emit a visible diagnostic and retain the active goal;
they never claim achievement. ACP session objective metadata exposes `goalStatus`,
`goalStop`, and `goalContinuationTurns` separately from work-turn completion.

`--goal-check-command '["/path/to/check", "arg"]'` selects a command evaluator
instead of a model. It receives objective and session evidence JSON on stdin
and returns only
`{ "verdict": "achieved|continue|needs_operator", "reason": "evidence or unmet condition" }`.
The `verdict` is required and must be exactly one of those three values; the old
`achieved` boolean is not accepted. `achieved` requires evidence of success.
`continue` admits automatic work only when the agent can act on the unmet
condition. `needs_operator` blocks the goal at once, records the reason and
shows an actionable stop receipt without consuming an automatic work turn.
Use it for login, approval, decisions or input that requires a person. A blocked
goal is terminal: after resolving the blocker, `/goal set TEXT` starts a new
goal. The stop receipt states this; `/goal achieve` and `/goal block` require an
active goal. Model checks use the same contract and receive these instructions. The command writes
its verdict on stdout. Its exact exec authority must be granted by ordinary
policy; it uses the same confinement and cancellation scope, with a 60-second timeout. Command
checks do not incur model cost. Model checks remain accounted generations.
Stderr is not part of the verdict. If the command exits unsuccessfully, the
visible goal-check diagnostic includes at most 4096 UTF-8 bytes from the end of
its bounded stderr preview. An exec allow rule does not grant filesystem access:
the evaluator script and its inputs must be readable under the run confinement.


Usage report v4 is the current closed contract: its role lines are `main`,
`judge`, `goal_check`, `compaction`, `subagent`, `hook`, and `total`.
`goal_check` is required in v4 reports, including zero usage; no legacy-report
decoder or compatibility default is provided.
Cost `known` and `unknown` describe monetary spend only. Display-only standard
API estimates are separate `subscription_equivalent` totals by currency, with
missing or overflowing estimates in `subscription_unknown`. The text view labels
them `subscription-equivalent (standard API)` and never presents subscription
accounting zero as known monetary spend.

Usage JSON v4 adds optional observed limits. A session can contain
`context_capacity: {tokens, source, observed_at}` from its latest
`ContextWindowUpdated` event. The source is `context_window_updated` and the
observation time is UTC epoch milliseconds. A missing or zero capacity is
omitted; the command does not look up a denominator in a different model's
catalogue or derive one from billed tokens.

A session can also contain `provider_limits: {provider, model, observed_at,
limits}` from response headers observed during its generations. Only an exact
match with the session's reported model is eligible. Each limit has `kind`,
`remaining`, and `source`, with optional `limit`, `used`, and `reset`. Numeric
limit values are exact nonnegative decimal strings. The supported kinds are
`requests`, `tokens`, `input_tokens`, `output_tokens`, and `usd`. Sources name
the actual remaining header, for example
`response_header:anthropic-ratelimit-input-tokens-remaining` or
`response_header:x-ratelimit-remaining-tokens`. A reset is the provider's raw
bounded reset value, not a predicted refill time. Headers are observations at
one point in time, not a promise that a subsequent request is admitted.

When an OpenRouter session is present, `--json` can also query the free
OpenRouter `/api/v1/key` endpoint with the current environment credential.
This lookup is bounded to five seconds, respects user-level offline
configuration, and does not run credential commands or read a keychain. Its
result is a top-level `provider_limits` array, not a historical session fact.
The observation has `provider: "openrouter"` and no `model`. A `usd` limit uses
`data.limit_remaining` directly; it does not subtract lifetime `usage` from a
periodic `limit`. Its source is `openrouter_key:data.limit_remaining`. The
optional `used` and `limit` values retain their distinct provider meanings.
Consumers must not assign this current credential to a historical session or
use it for another provider. Text output performs no key lookup.

Missing headers, unlimited or absent key limits, missing credentials, offline
configuration, lookup failures, invalid values, and timeouts produce no limit
field. No zero, percentage, balance, reset, or fallback provider is inferred.
The command makes no paid generation request for this information.

### Model-requested context changes

Durable interactive sessions expose `context` with `action: compact|clear`,
`reason`, and `handoff`. The model-authored request is recorded in the source
log and its tool result shows the action and reason. The idle owner waits for
the complete tool batch and pending operator work before it uses the existing
compaction publication path. The successor continues without an operator prompt.
`compact` uses the canonical checkpoint and captured source request prefix.
`clear` skips checkpoint generation and retains only the active goal and its
ceilings, fresh instructions, and the model-written handoff. Historical text
stays untrusted. Both actions use the existing checkpoint lineage and publisher.

The bundled `context-management` skill explains safe sub-task boundaries,
measured context pressure and topic changes, and forbids transitions with
unfinished effects or pending questions. The default cooldown is 10 completed
work turns after a model transition, carried in the atomic successor seed.
`--context-interval N` changes it (N must be positive). `--no-context-tool`
disables model requests, not automatic pressure recovery. Clear is disabled
unless the operator passes `--context-clear`, and requires an active goal.
Headless and ACP hosts do not advertise the tool until they can own automatic
successor activation; their explicit compaction commands remain unchanged.

### Delete a stored session

`ego rm ID [--yes] [--force] [--session-dir PATH] [-C DIR]` deletes the named
session from the catalogue. Without `--yes`, the CLI asks for terminal
confirmation; non-interactive use must pass `--yes`. `--force` acknowledges
an uncertain prior run and does not replace confirmation. Running, leased,
or held sessions are refused even with force. Release a hold first.

Deletion uses the same durable tombstone as ACP `session/delete`. It hides
only the target from `ego ls` and ACP session lists. It retains history for
fork descendants and does not remove shared storage or resolve uncertain
external effects. Repeated deletion succeeds.


## Markdown custom commands

Markdown files under `commands/` in the shared capability roots define `/name`
prompts. Project namespaces are `.ego`, `.agents`, `.claude`, then `.opencode`;
nearest trusted project wins over ancestors, user roots, and `/etc/ego/commands`.
User roots include `$EGO_HOME/commands`, `~/.agents/commands`,
`~/.claude/commands`, `~/.opencode/commands`, and `~/.config/opencode/commands`.
A nested `git/review.md` is `/git:review`. Built-ins and skill-name collisions
are refused visibly rather than shadowed.

`$ARGUMENTS` inserts raw arguments; `$1`, `$2`, and later positions use shell-like
argument grouping. Pi `$@`, `${1:-default}`, `${@:-default}`, `${@:N}` and `${@:N:L}`
expressions are also supported in prompt text; malformed expressions fail visibly.
With no placeholder, raw arguments are appended. `@path`
inlines a workspace file under the instruction byte and file-count limits.
`!` followed by a backtick-delimited shell command captures stdout before the
prompt is sent. Shell execution uses the existing exec permission, approval,
process identity, timeout, cancellation, and sandbox path. A failed, refused,
truncated, or non-UTF-8 result refuses the complete prompt with a visible error.
Arguments, imports and output cannot introduce additional shell markers.
Use positional parameters inside shell markers; raw `$ARGUMENTS` there is refused.

Optional strict YAML frontmatter accepts string `description` and `argument-hint`.
`model`, `allowed-tools`, and all other behavioral fields are rejected visibly;
they never silently grant foreign authority. Commands appear with admitted
skills in help, completion, `/skills`, and ACP `available_commands_update`.
ACP invokes each advertised skill or command by sending `/name args` as a text
prompt. Plain skills use the same admitted instruction, argument substitution,
policy, hook, and child-execution path as an explicit interactive invocation.
Advertisement and selection use the same user-command catalogue; model-only
skills are not advertised. Image and audio attachments are refused for these
text-only invocations rather than silently discarded.
See the [command contract](ARCHITECTURE.md)
for bounds, ordering, and the distinction between atomic prompt publication and
non-transactional shell side effects.

### MCP OAuth registration and refresh

Use an operator-issued **public** client with
`ego mcp login NAME --client-id ID --callback-port PORT`, or select
`--cimd https://example.org/ego-client.json` for an already published client
metadata document. `--dcr` explicitly requests dynamic public-client registration
from the selected issuer. Ego never falls back from one strategy to another.
Confidential clients and client secrets are not accepted.

The command prints a copyable authorization URL. On SSH, forward the callback
port to the machine running ego before completing authorization. The exact
callback is `http://127.0.0.1:PORT/callback`; port zero selects a free port.
Login expires after 120 seconds; Ctrl-C cancels the listener and login future.
Credentials are persisted through the existing native credential-store adapter.
A failed or denied login does not write a credential.

Refresh an existing credential with
`ego mcp login NAME --client-id ID --refresh --issuer ISSUER [--account ACCOUNT]`.
Use the same registration choice as login; `--dcr --refresh` restores the stored
DCR identity and does not register a second client. The issuer and optional
account identify the exact stored credential. Refresh does not open an
authorization interaction, and a failed refresh leaves the old credential intact.

OAuth login writes tokens only through the native credential store and writes a
private non-secret resource/account locator under `EGO_HOME/mcp`. Runtime uses
this locator after restart. Expired access tokens refresh before MCP dispatch.
HTTP 401 triggers refresh without replaying the operation; retry the operation
when requested. Missing or revoked credentials require a new explicit login.
Servers that advertise only confidential-client authentication are not supported.


### Session-owned file checkpoints

Builtin `write` and `edit` save pre-edit bytes in private Git commit objects before
changing files. This works in Git worktrees, including dirty and untracked files,
without changing HEAD or the user's index. Outside Git, builtin results explicitly
state that no checkpoint is available.

```text
ego checkpoint [-C DIR] SESSION_ID preview
ego checkpoint [-C DIR] SESSION_ID restore CHECKPOINT_COMMIT
ego checkpoint [-C DIR] SESSION_ID close
```

Preview emits JSON with each checkpoint's path, restore effect, and conflict state.
Restore selects exactly one checkpoint and refuses external changes, missing edited
files, external atomic replacements, symlinks, and permission changes. There is no force flag: preserve or resolve
an external edit before retrying. Restore successive edits to a file newest first.
A created file is deleted when its creation checkpoint is restored. Repeating a
completed restore is safe. Binary preimages are preserved byte-for-byte.

Refs live at `refs/ego/checkpoints/SESSION_ID`; normal exit keeps them for recovery.
Explicit checkpoint close or `ego rm` removes the selected repository's session
ref, without changing files. Git GC later removes unreachable objects. Child
sessions own separate refs. With multiple repositories, close each with `-C`.
Exec commands, MCP effects, arbitrary deletes, and external editors are not captured.
Checkpoint objects contain original file bytes; do not push private checkpoint refs.
### Named output styles

`--output-style NAME` selects a response-style overlay for a CLI session or a
served session launch (`ego acp`). With no selection, the prompt
is unchanged. The built-in catalogue contains `concise` and `explanatory`.
These names are reserved and cannot be overridden by user files.

For a custom style, create `$EGO_HOME/output-styles/NAME.md` (or
`~/.ego/output-styles/NAME.md` when `EGO_HOME` is unset). The file is plain
UTF-8 prompt text, not a configuration or agent envelope. Names contain 1–80
ASCII letters, digits, underscores or hyphens. Files must be nonempty, contain
no NUL bytes, and be at most 64 KiB. Operator-owned catalogue and file symlinks
are followed; the opened target must be a regular file. Missing or invalid styles
refuse the run; there is no fallback to a workspace file. Install only styles you
trust.

The selected name and source (`builtin` or the user file path) appear in a
diagnostic, not protocol stdout. Styles are explicit launch selections; they
are not discovered from workspace files or stored as a session preference.
To resume with a style, pass the flag again.

Composition order is base instructions or `--system`, then the selected style,
then `--append-system`, then a session prompt in append mode. A session prompt
in replace mode suppresses the launch style and launch append. An explicit
style is available with `--bare`, which disables ambient discovery, not launch
options. Styles do not select tools, change permission policy, execute commands
or expand `@file` text inside the style. The overlay affects model instructions,
not the terminal renderer or the machine-readable output format.

### Notebook cell edits

`notebook_edit` is a builtin available through the same tool registry to CLI,
ACP, and runtime consumers. Use `--tools notebook_edit` to select it explicitly.
It accepts `path`, an explicit `edit_mode`, and mode-specific fields:

- `replace`: `cell_id` and `new_source`. Change only the source of that cell.
- `insert`: `cell_type` (`code` or `markdown`), `new_source`, and optional
  `cell_id`. Insert after that ID, or at the start when the ID is omitted/null.
- `delete`: `cell_id`. Remove that cell only.

The file must be a valid UTF-8 nbformat 4.5 notebook with unique stable IDs,
no larger than 16 MiB before or after the edit. Invalid files and missing IDs
are refused without a write. Notebooks with integer tokens outside the i64/u64
range are also refused to prevent rounding saved data. Inserted cells receive a
new random ID. Existing IDs, metadata, outputs, execution counts, and attachments remain unchanged
unless their cell is deleted. The tool does not execute a kernel or clear old
outputs. JSON formatting can change; preservation is semantic, not byte-for-byte.

Each call declares read and write effects on the same resolved file. A matching
`write(pattern)` allow covers the supporting read, as for `edit`. Explicit read
and write denies, workspace confinement, and plan mode still apply. Concurrent
`notebook_edit` calls in one ego process are serialized. Do not edit the notebook
from another process during a call: external writers do not share that lock,
and a pre-publication reread is not an operating-system compare-and-swap.

Reference contracts captured 2026-10-16:

- <https://code.claude.com/docs/en/tools-reference.md>, NotebookEdit section:
  cell-ID targeting, replace/insert/delete, and UTF-8 refusal. ego uses its own
  lowercase tool name and requires an explicit edit mode.
- <https://nbformat.readthedocs.io/en/latest/format_description.html>:
  source representation, metadata, outputs, and stable ID rules.
- `crates/ego-tools/src/tools/nbformat.v4.schema.json` is the Jupyter v4.5
  schema recorded from
  <https://raw.githubusercontent.com/jupyter/nbformat/main/nbformat/v4/nbformat.v4.schema.json>.
  The upstream BSD license is stored beside it as `nbformat.LICENSE`.
### External status producers

Only system/user configuration or explicit flags can install
`ui.statusline.providers.NAME = { argv = ["program", "arg"], timeout_ms = 250 }`.
Project, local and agent-file configuration cannot install executable providers.
A trusted interactive session runs each provider once when its session epoch
opens, through the current exec policy and confinement. Rendering, resize and
Git-status refresh never execute providers. Permission requests that do not
settle under current policy omit the value and emit a diagnostic; no hidden
human or model approval is requested.

There are at most four providers, each with a timeout from 1 to 1000 ms (default
250), at most 32 argv items and 4096 total argument bytes. The command must exit
successfully, emit no stderr, and return one UTF-8 stdout line of at most 1024
bytes, including an optional final newline. Binary, truncated, multiline and
oversized output is omitted. Terminal controls are sanitized and known secrets
are masked. Values are cached in the live seat, not persisted or admitted as
instructions. Providers inherit the normal admitted exec environment; there is
no shell expansion unless the operator explicitly selects a shell executable.


### ACP audio input

The durable ACP surface advertises audio input. This is a surface capability,
not a promise that every selected model accepts audio. Submitting audio grants
consent for durable session storage and provider transmission; the ACP host must
obtain user-facing consent. Ego checks the resolved model on each generation,
including resumed history and fallback. Unsupported or unknown models refuse
before audio is sent; ego does not drop or transcribe audio.

The supported contract is public OpenAI Chat Completions, exact model
`openai/gpt-audio-1.5`, with text output. Supply its row in `models.toml` because
the pinned builtin catalogue does not contain it:

```toml
[model."openai/gpt-audio-1.5".capabilities]
schema_version = 6
max_context_tokens = 128000
max_output_tokens = 16384
supports_parallel_tool_calls = false
reasoning_carryover = "unsupported"
reasoning_efforts = { only = ["off"] }
default_reasoning_effort = "off"
cancel = "cooperative"
cache = "none"
transport = "sse"
image_input = "unsupported"
audio_input = "supported_wav"
```

Audio requires explicit ACP `mimeType: "audio/wav"` and base64 data. Ego admits
PCM16 WAV only, mono or stereo, at most 48 kHz and sixty seconds per clip. A prompt
can contain at most four clips and 10 MiB of decoded audio in total. These are ego
limits, not documented provider maxima. Mixed image/audio prompts are refused.
No remote URLs, inferred MIME, transcoding or raw PCM input is accepted.

Exact validated bytes are committed into the session blob store before the
journal references them. Replay checks the reference and encoding again. Missing
or corrupt blobs cause an explicit refusal; they are never fetched again or
silently omitted. The operator must preserve the session blob store with its log.

Audio-specific pricing is not represented by text-token quotes. Calls continue
with unknown cost and a visible anomaly notice; a monetary cap cannot cover that
unpriced spend. No paid audio
acceptance probe is part of this feature's validation. Provider contract sources:
<https://developers.openai.com/api/docs/models/gpt-audio-1.5.md> and
<https://developers.openai.com/api/docs/guides/audio-chat-completions.md>, read
2026-10-20.
