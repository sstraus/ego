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
ego run [PROMPT]              headless one-shot; PROMPT from argv, -f FILE, or stdin
ego resume [ID] [PROMPT]      continue a session; no ID = latest in this workspace;
                              --answer QUESTION_ID=JSON answers pending input;
                              --name and --goal are metadata-only operations
ego ls                        list sessions (this workspace; --all for every one)
ego fork <ID> [--at ITEM_ID]   branch after a completed turn
ego compact <ID> [--request-id UUID]
                              publish one idempotent checkpoint target
ego export <ID> [--sanitize] [--format md|json|jsonl] [--file PATH]
                [-C DIR] [--session-dir DIR]
                              render a session; Markdown is the default
ego mcp add NAME --stdio [--cwd DIR] [--inherit-env NAME]... -- COMMAND [ARG]...
        add NAME --http URL [--auth none|oauth] [--allow-insecure-loopback]
        rm NAME | ls [--json] | login NAME | reload [NAME]
                              shared configuration and live admission service;
                              login refuses until product CIMD is published
ego mcp-server --max-concurrent-runs N --retained-requests N
               --max-resource-bytes N [-C DIR] [--session-dir DIR]
                              serve ego AS an MCP server on stdio: scoped
                              run/continue/cancel/status tools, plus M3 compact
                              (delegation surface for Claude, Codex, CI) plus
                              sanitized resources; no MCP Tasks or prompts
ego auth   login|logout|status [PROVIDER]
ego models [--provider P] [--refresh] [--json]
ego hooks  ls [--json]                     read-only admitted hook catalogue
ego config get|set|unset|ls|edit
ego rules  ls|allow|deny|revoke|remember|explain   (permission rules,
                              stable IDs; `explain <tool> <args>` shows the
                              canonical intent + matching rule, dry)
ego usage  [--period P] [--all-roles] [--json] cost and token report
ego acp [--tools LIST | --no-tools] [--allow RULE] [--deny RULE]
        [--profile NAME] [--append-system TEXT|@file]
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
ego doctor [--json]           read-only redacted health snapshot
```

Notes:

- `ego run` is the scripting workhorse. `ego resume <id> "next step"` chains
  scripted multi-turn conversations — a pipeline can carry a session through
  several stages. One session pool across interactive and headless runs
  (Codex split them and had to add `--include-non-interactive` to re-merge).
- There is deliberately **no bespoke RPC mode**: embedders get `ego acp`.
  pi invented `rpc`/`json` modes; ACP is that, standardised.
- An ACP host may supply a stateless HTTP MCP server in `session/new`. HTTPS
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
  Canonical JSON ends in one newline, JSONL is the same single bundle line, and
  Markdown embeds that same bundle. `--file` suppresses stdout; JSONL with a
  file is a usage error. Relative files resolve from canonical `-C`. Existing
  identical bytes succeed without rewrite, different bytes conflict, and a
  published-but-durability-uncertain file remains visible and is not retried.
- Metadata-only session operations reuse `resume` and never start a model turn:
  `ego resume ID --name show|set=TEXT|clear [--output text|json|jsonl]` and
  `ego resume ID --goal show|set=TEXT|achieve|block=REASON` with optional
  `--output text|json|jsonl`. They do not accept a prompt or `--answer` in the
  same invocation.

### 2.1 Availability — what ships today

The same discipline as §3.1, and it was missing here until 2026-08-21. §2 lists
seventeen commands; this build implements **fourteen**. The remaining three are
refused **by name**, each naming the subsystem it waits on, from the `UNBUILT`
table in `crates/ego-cli/src/cli.rs`. A test binds that table to this section,
so a command can be refused as documented only while this list still offers it.
M5 must enable all seventeen from their final owners; this count is current
availability, not permission to ship a fourteen-command product.

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
| `ego resume` | **yes** | continues a session that already exists, never creating one. `[ID]` is optional: without it the most recently updated session of this workspace is taken. Without a prompt source it enters the interactive driver; with a prompt or `-` it remains headless |
| `ego ls` | **yes** | this workspace by default, `--all` for the whole store; `--output json\|jsonl` per §4.6 |
| `ego fork` | **yes** | creates a durable lineage branch at the idle tip or after the completed turn containing `--at ITEM_ID`, without copying source spend or run state |
| `ego compact` | **yes** | a required request ID derives one idempotent clean target from the existing idle compaction operation and supports later continuation |
| `ego export` | **yes** | local transcript export plus the producer-only version-1 structural bundle under `--sanitize`; Markdown, JSON, JSONL, stdout, and atomic file publication follow §2.1 |
| `ego mcp` | **yes** | `add`, `rm`, `ls [--json]`, and `reload [NAME]` use one live admitted registry and truthful health projection. `login NAME` is typed but refuses before network access until the product CIMD origin and loopback redirect are published |
| `ego mcp-server` | **yes** | serves scoped `run`/`continue`/`cancel`/`status`/`compact` tools and sanitized summary/output resources on stdio. `--max-concurrent-runs`, `--retained-requests`, and `--max-resource-bytes` are required non-zero deployment bounds; `-C`/the current directory is the sole canonical root and stdout remains protocol-only |
| `ego auth` | **yes** | `login\|logout\|status [PROVIDER]`; credentials live in one locked `$EGO_HOME/auth.json`, while TOML commands and discovered auth plugins can supply login tokens |
| `ego models` | **yes** | prints the shared current-model catalogue as deterministic text or versioned JSON; `--provider` filters both model and source rows, and `--refresh` updates authoritative discovered slices without promoting cached display facts into generation authority |
| `ego hooks` | **yes** | `ls [--json]` reads the same immutable admitted hook catalogue used by execution; it neither reloads nor edits hook policy |
| `ego config` | **yes** | `get`, `set`, `unset`, `ls`, and whole-document `edit` use one strict atomic user-config transaction; stored profiles are explicit targets and management ignores ambient `EGO_PROFILE` |
| `ego rules` | **yes** | `ls`, `allow`, `deny`, `revoke`, `remember`, and `explain` share the canonical rule grammar, atomic user store, and effective policy service; management ignores ambient `EGO_PROFILE` and never executes the explained call |
| `ego usage` | **yes** | reports the versioned main/judge/compaction/total accounting DTO for this workspace; `--period` accepts `all` or a positive `s`/`m`/`h` duration and `--json` emits the same DTO. The text view is a compact `session model role in out cached cost` table that hides role rows with no activity unless `--all-roles` is given; the flag is presentation only and never reaches the JSON report |
| `ego acp` | **yes** | the Agent Client Protocol on stdio, v1. A host's `session/new` mints an ordinary ego session and its `session/prompt` runs an ordinary turn, so what it opens is listed by `ego ls` and continued by `ego resume`. v1 and not the v2 draft: see `docs/decisions/2026-08-21-the-version-an-agent-answers-with.md` §6.2 |
| `ego doctor` | **yes** | one ordered, versioned, redacted `HealthSnapshot` over nine sections, rendered as text or `--json`. It reads existing facts only: it never logs in, refreshes a credential or a catalogue, reloads an extension, edits config, or calls a hosted provider. `/doctor` renders the same snapshot, and a fact ego cannot observe without mutating something is reported `not_checked` with its reason rather than omitted |

All four M0b demonstrations now have an entry point (`ARCHITECTURE.md` §8): an
interactive session, `ego run … | jq`, `kill → ego resume`, and the same
session over ACP. This is availability, not a release boundary; later
milestones remain dependencies of the final product.

### 2.2 Configuration and rule management grammar

The final root grammar is exact:

```text
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

`config set` accepts one assignment and parses its value as TOML. Its closed
key set is `model`, `thinking`, `mode`, `sandbox`, `budget`, `bare`,
`permissions.judge`, and `ui.statusline.segments` at base user scope; profiles
accept `model`, `thinking`, `mode`, `sandbox`, `budget`, and
`permissions.judge`, with `session_dir` and `artifact_dir` accepted only for
explicit profile `ci`. Permission arrays, credentials, MCP, and hooks remain
owned by their dedicated commands. `unset` is the only null operation and
prunes empty managed tables. There are no table fragments, list indices,
aliases, or arbitrary dotted keys.

The standalone runner also accepts `exec_paths = ["/absolute/tools/root"]`
in `~/.ego/config.toml` (or `$EGO_HOME/config.toml`), selected user/system
profiles, and explicit `-c` overrides. Edit the TOML with `ego config edit`;
`config set` does not manage this key. Higher layers replace the whole list;
an explicit empty list clears configured roots. Project and project-local
configuration cannot add exec roots. Paths must be absolute and must exist;
the runner canonicalizes symlinks before creating the confinement policy.
On macOS and Linux these roots add read and execute access, never write access.
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
Mutations print one five-field tab-separated receipt containing changed state,
target, canonical key, canonical value or `<absent>`, and publication state.
Successful no-ops use `not_published`;
pre-publication failures emit no receipt. Text fields escape backslash, tab,
carriage return, and newline.

`rules allow|deny` stores the canonical AST in the existing user
`permissions` arrays. A same-effect duplicate is an unchanged success; the
opposite effect is a conflict. `revoke` removes the ID from both effects in the
selected user target; an absent ID is unchanged success, a future grammar ID
is exit 2, and a collision is exit 1. Copies in other authorities survive and
are reported. A permissions table is a whole-block override: creating one may
shadow that layer's base judge/lists or lower-layer allow/judge values; system
denies remain binding. Receipts report stored and effective change separately.
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
```

**Permissions and confinement** (one axis pair, Codex-style)

```
    --mode M            plan|default|auto|edits|yolo   shortcuts: --plan, --yolo
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
`docs/decisions/2026-08-21-a-rule-that-means-exactly-one-thing.md`, which
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
    --name OP           resume only; show|set=TEXT|clear, metadata-only
    --goal OP           resume only; show|set=TEXT|achieve|block=REASON,
                        metadata-only
```

**Scripting contract**

```
    --output F          text|json|jsonl              (default text)
    --schema S          JSON Schema (@file or inline): the final answer MUST
                        validate; ego re-prompts on mismatch, fails code 1 after
                        --schema-retries N (default 2)
    --budget B          hard cap: '2.5$' or '400k' tokens → exit 5 when hit
    --max-turns N       cap agent turns
    --timeout DUR       wall clock: '90s', '10m' → exit 124
    --retry N           provider retries, delivery-certainty-aware: only
                        DefinitelyUnsent attempts retry automatically
    --fallback P/M      ordered model degradation chain (repeatable)
-f, --file FILE         prompt/agent file (§5)
    --var K=V           template variable for agent files (repeatable)
-C DIR                  run as if started in DIR (git-style)
-c, --set K=V           dotted-path config override, value parsed as TOML
                        (Codex's mechanism: -c 'sandbox.mode="workspace"')
    --profile NAME      layer [profile.NAME] from config (§7)
    --bare              no ambient discovery even in a trusted workspace
-q / --no-color
```

**Attachments and tools**

```
    --attach IMAGE_PATH|HTTPS_IMAGE_URL
                        repeatable; exact image bytes admitted as a durable
                        content block before provider projection
    --tools LIST        restrict builtin tools;  --no-tools for pure generation
    --mcp NAME|@cfg.json  attach a named effective or strict explicitly named
                          JSON MCP server for this invocation (repeatable)
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
    --instructions FILE          which instruction file, named not discovered
```

Every surface that opens a session takes them, from one declaration: `ego run`,
`ego resume`, and `ego acp`. They are declared once (`Shape` in
`crates/ego-cli/src/cli/commands.rs`) and converted once
(`SessionShape`), so the served surface cannot drift into a `--tools` that
means something else.

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
at most N additional attempts on the same resolved model and only after a
`DefinitelyUnsent` settlement. Each repeatable `--fallback P/M` is an ordered
candidate that is re-resolved from current facts and attempted only after
capability and request-expressibility checks preserve tools, images, schemas,
limits, and reasoning state. A possibly-sent attempt stops automatic retry and
fallback. Every attempt has a fresh ID, reservation, request digest, settlement,
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

Rows marked **M5 target** freeze a final spelling whose owning story has not
yet changed the parser. They are not claimed as current and must become
ordinary parsed flags rather than long-lived compatibility placeholders.

| Flag | Current | Note |
|---|---|---|
| `-m, --model` | **yes** | |
| `--offline` | **yes** | |
| `--thinking` | **yes** | §3 above spelled the second value `min` until 2026-08-21; the parser has only ever accepted `minimal`, so `--thinking min` was a documented value that errored. Corrected above |
| `--system`, `--append-system` | **yes** | `--append-system` is part of §3's general session shape, so `ego acp` takes it too; `--system`, which replaces the prompt outright, is not |
| `--instructions` | **yes** | part of §3's general session shape, so `ego acp` takes it too. It was **missing from §3's grammar** until 2026-09-20 and documented only in §4.5's prose |
| `--bare` | **yes** | |
| `--session`, `--session-id` | **yes** | durable session choices; the parser places them in the §4.5 mutually exclusive session group |
| `--ephemeral` | no | recognized only to refuse by name with usage error exit 2 before opening or creating a session or causing an effect; use the durable default, `--session`, or `--session-id` |
| `--session-dir` | **yes** | |
| `--artifact-dir` | **yes** | `ego run`/`ego resume` only, and only with the exact selected profile name `ci`; flags override `[profile.ci].artifact_dir` |
| `--answer` | **yes** | `ego resume --answer QUESTION_ID=JSON` validates and durably settles pending questions before continuing the session |
| `--name`, `--goal` | **yes** | metadata-only `ego resume ID` operations from §2; they cannot be combined with a prompt, answer, or run-only override |
| `--output` | **yes** | `text\|json\|jsonl`. On `ego ls` the two machine forms are §4.6's, not §4.1/§4.2's — a listing is not a run |
| `--all` | **yes** | `ego ls` only, and documented in §2 rather than here: §3 is the argument grammar of a *run*, and this widens a catalogue query |
| `--schema`, `--schema-retries` | **yes** | `--schema-retries` ships as its own flag; §3 mentions it only inside `--schema`'s description |
| `--budget` | **yes** | binds only on a currency-metered row — the run says so on `metering.selected` |
| `--max-turns`, `--timeout` | **yes** | |
| `--tools`, `--no-tools` | **yes** | part of §3's general session shape, so `ego acp` takes them too. `--no-tools` there is the session that can never ask for a permission seat |
| `-C`, `-c/--set`, `--profile` | **yes** | `--profile` is part of §3's general session shape, so `ego acp` takes it too. `-C` it already shared; `-c/--set` it does not take |
| `-q`, `--no-color` | **yes** | `--no-color` was marked **yes** here on 2026-08-21 while being parsed and read by nothing — the same silently vacuous flag §4.4 refuses elsewhere. It decides for real from 2026-08-22: it is the highest-precedence input to the one capability gate (`crates/ego-cli/src/render/caps.rs`), above `NO_COLOR`, above `TERM`/`COLORTERM`, above `isatty`. See `docs/decisions/2026-08-22-two-axes-and-a-terminal-that-answers.md` §1 |
| `--mode`, `--plan`, `--yolo`, `--allow`, `--deny` | **yes** | `--allow`/`--deny` are part of §3's general session shape, so `ego acp` takes them too; `--mode` and its two spellings are not part of it. `--plan`/`--yolo` are spellings of `--mode`; two at once is a usage error. All five modes enforce. `auto` resolves a separate configured judge and routes its closed verdict before any human escalation. All seven rule heads enforce: `read`, `write`, `list`, `search`, `exec`, `net`, and `tool` — `net` matches `Op::Connect`/`Op::ProtocolConnect` and `tool` matches `Op::Opaque`, both of which this build now produces. This sentence is derived from `parse.rs`'s `OPERATIONS` table by `the_documents_name_the_rule_heads_the_parser_actually_enforces`, not maintained beside it. The seven builtins are read, glob, grep, write, edit, exec, and read_tool_result. `edit` prepares both `Read` and `Write`; a matching `write(pattern)` allow covers its same-file replacement read without a separate `read(pattern)` allow. The glob still limits the target; explicit read/write denies, workspace boundaries, and plan mode still apply. A standalone read still needs read authorization; `read_tool_result` uses its session-evidence capability rather than a user-authored rule head. |
| `--sandbox` | **yes** | `ro\|workspace\|off`, default `workspace`, enforced by the measured platform backend before a prepared process can launch; an unavailable required capability fails closed |
| `-f`, `--file`, `--var` | **yes** | strict version-1 agent files load through the existing bounded YAML implementation, apply their configuration layer below flags, and substitute declared body variables. M4 extracts the shared envelope for skills and still owns the direct executable shebang end-to-end proof, not these flags' availability |
| `--attach` | **yes** | repeatable local-path or HTTPS image input; bytes enter the durable session-blob/content path before model-specific projection and unsupported model/profile combinations refuse before acquisition |
| `--mcp` | **yes** | repeatable ordered selection of effective `NAME` entries and strict invocation-scoped `@FILE` entries; the selected live admission is shared by dispatch, `/tools`, `/doctor`, and deterministic `mcp_errors` |
| `--retry`, `--fallback` | **M5 target** | regression case installs the one-turn attempt coordinator; current delivery certainty alone does not authorize re-issue |
| `-v`, `--verbose` | excluded | the binary refuses this legacy placeholder by name, and the exclusion regression case recorded stands. What it once waited on now exists and is not a flag: `EGO_LOG` (an `EnvFilter` string) installs a subscriber for the `tracing` events this build emits and `EGO_LOG_FILE` collects them to a file, because a diagnostic level is not an argument of a run — see §4.8. regression case was expected to retire the spelling and did not; it stays in the unavailable table rather than the retired one, because retiring a flag requires naming its replacement and there is deliberately no replacement flag to name |

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
  "v": 10,
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

`usage` is the sum of the per-generation deltas, split by the categories that
are *priced differently*; it is a flow and only grows. `context` is the
occupancy *level* of the live context window and goes **down** after a
compaction, so it is reported separately and never derived from `usage`.
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
session stopped because a host called `_ego/pause` (§ `docs/06-acp.md`), and
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
`docs/decisions/2026-08-21-an-exit-code-that-names-who-refused.md`.

The three remedies are alternatives because they act on different axes, and
one of them is not a permission at all: **the workspace boundary belongs to
`ARCHITECTURE.md` §3.4 axis 4 — the execution backend — and no mode moves it.**
A mode says who approves; a backend says what is reachable. So an operation
outside the workspace is refused rather than asked about, `--yolo` does not
lift it, and none of the three remedies above resolves it — because it was
never an unresolvable *permission* for them to resolve. Widening what ego may
reach is `--sandbox`'s business, which is why that clause is listed beside
`--yolo` rather than subsumed by it.

A read, list or search the boundary refuses is the one exception to exit 3: it
changed nothing, so the refusal is returned to the model as a failed tool
result and the turn continues (regression case). Writes and spawns outside the
workspace still end the run.

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
regression case regenerates both artifacts only after all 17 commands and their
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
hooks, skills and MCP servers load only after the interactive seat explicitly
trusts the canonical workspace. Declining, Ctrl-C or EOF
continues bare. Headless always runs untrusted and bare automatically (no
dialog, no ambient code execution). `--bare` forces this and skips the trust
question. The question is asked only where there is something to answer about:
a workspace holding no project config, skill, or discoverable instruction file
starts untrusted in silence, and the `trust` status segment says so. For a
workspace under `~/Gits`, instruction discovery starts at `~/Gits/AGENTS.md`
and descends through every ancestor to the working directory; outside that
tree it starts at the workspace root. Scratch work under `~/Gits/.tmp` does not
inherit shared instructions. `AGENTS.md` is canonical; a relative `CLAUDE.md`
symlink to it adds no duplicate prompt text.



`--instructions FILE` is the headless opt-in and is **not** ambient: it names
a file rather than discovering one, so it survives `Posture::Bare` and
*replaces* the discovered set rather than adding to it. Its bounded imports
are relative to `FILE` and cannot leave that file's directory. `--bare` together with
`--instructions` is a conflict for the applicability matrix above, not for the
loader — from a resolved posture alone, "the user demanded bare" and "the
workspace is untrusted" are the same value and want opposite answers. See
`docs/decisions/2026-08-21-instructions-a-workspace-may-hand-a-model.md`.

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
`docs/decisions/2026-08-22-two-axes-and-a-terminal-that-answers.md`.

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

**One gate decides, once** (`crates/ego-cli/src/render/caps.rs`). In precedence
order: `--no-color`, then `NO_COLOR` (presence, not value), then whether stdout
is a terminal, then `TERM`, then `COLORTERM`, then `TERM_PROGRAM`. No other
code in the workspace reads one of those variables or formats an escape by
hand. `--no-color` on a real terminal removes the *colour*, not the layout: the
person asked for no colour, not for a file.

**Out of band**, on a screen only: OSC 133 C/D around the run (so a host
segments the output into a command block instead of pattern-matching tool
headers), OSC 9;4 for the progress indicator — cleared on every exit path,
including failure — the window title, a desktop notification for a run that
took longer than a minute (suppressed by `-q`), and OSC 7770 `state`/`intent`
under TUICommander, which is proven by `TUIC_SOCKET` and not by `TERM_PROGRAM`.
The human-facing product name is `εgo`, and every window title begins with the
exact prefix `ɛ । `. Every payload is stripped of control characters before it
goes into a sequence: a tool name, an error message and an intent line all come
from somewhere that is not ego.

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
`docs/decisions/2026-09-07-diagnostics-get-a-destination-before-a-switch.md`.

## 5. Agent files — the scripting language

An **executable agent file** is YAML frontmatter + prompt body. Same format
family as skills; `--var` fills `{{placeholders}}`; shebang makes it a program:

```
#!/usr/bin/env -S ego run -f
---
version: 1
model: anthropic/claude-sonnet-5
mode: edits
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

Version 1 is closed to `version`, `model`, `mode`, `sandbox`, `allow`,
`schema`, and `vars`; unknown fields and versions refuse. Those operational
keys mirror their CLI flags one-to-one — the file *is* a saved command line
plus a prompt, not an untyped map of every present or future flag. The one
strict bounded YAML envelope is shared with skills; the complete format,
limits, variable equality, relative-path, and failure contract is
`docs/decisions/2026-08-29-agent-files-share-one-strict-yaml-envelope.md`.
Nothing else examined has this as a first-class, executable unit (Claude
Code's slash commands are REPL-only; pi's prompt templates are not
executables).

### 5.1 Skills are in-session capabilities, not agent files

A skill is a strict `SKILL.md` under `/etc/ego/skills`, `$EGO_HOME/skills`, or
trusted `<workspace>/.ego/skills`. It uses current Agent Skills frontmatter and
the supported current Claude extensions documented in `docs/20-agent-skills.md`.
εgo does not scan `.claude/skills`: compatible metadata and bodies are portable,
while storage aliases would create a second trust and precedence namespace.

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

The live seat enters raw mode before startup discovery, so lines typed before
the composer appears retain their Enter boundaries without cooked terminal
echo. The first starts a turn; further submitted lines enter the busy FIFO.

Slash commands: `/help /model /thinking /mode /plan /yolo /permissions /mcp
/sessions /fork [message-id] /compact /clear /usage /statusline /export /doctor /quit /new
/name /goal /tools /expand /skills /hooks /version`. These 25 commands are the complete
maintainer-selected surface; a missing implementation remains required work rather
than an implicit de-selection. Typing `/` at the beginning of the composer
opens a filterable menu; the menu, shortcut bindings, help, and enabled-state
checks are projections of one action registry rather than parallel command
implementations. An exact registered action executes with one `Enter`; a
partial completion remains inert until explicitly submitted, and pasted slash
text remains ordinary prompt input.

That paragraph is the final contract. The current implementation has the shared
25-command action registry: a typed complete slash is classified as an action,
while bracketed paste and non-command text remain ordinary prompts. Completion,
menu, help, dispatch, and live availability read that one registry. Actions
whose final owner has not landed remain visible with their explicit disabled
reason rather than becoming model input or silently disappearing.

Arguments that do not belong to a selected local action are usage errors, not
model prompts. In particular, `/mcp` inspects the admitted runtime snapshot;
configuration remains explicit through `ego mcp add|rm|ls`, and `/mcp add`
points to that root syntax without consuming a provider turn.

`/help` renders task-oriented Discover, Model, Control, Session, Integrations
and Surface groups from that registry into transcript scrollback. `/doctor`
wraps the shared health snapshot at the live terminal width. Both preserve
every field at 120, 80, 60, and 40 columns instead of relying on terminal
reflow or attaching multiline output to the composer.

`/mode`, `/plan`, `/yolo`, and `/permissions` are argument-free local modal
actions. A direct-argument form such as `/mode edits` or `/permissions allow
...` is rejected as action usage and never submitted as a prompt. `/mode`
selects one of `plan`, `default`, `auto`, `edits`, or `yolo` for this session;
`/plan` stages plan idempotently. `/yolo`, the Yolo palette action, and Yolo
selected through `/mode` stage yolo directly and produce one receipt; they do
not open a second confirmation ritual. Selecting auto validates a configured
judge and its current model/request without a provider effect; deterministic
local calls remain available when no judge is configured.
`/permissions`
exposes the same list, allow, deny, revoke, remember, and explain services and
DTOs as the root `rules` command. List/explain use the session's
already-admitted ruleset; mutations always target base user config, never the
active profile or `EGO_PROFILE`. Remember and explain select an admitted tool,
then edit its schema-derived fields with exact values prefilled; nested shapes
remain in one prefilled JSON fallback instead of requiring a whole call to be
typed as JSON.

An unresolved permission modal is at most four rows: the call as a readable
command, its risk in plain words, and the keys `[y] yes`, `[a] yes, do not ask
again for <rules>`, `[n] no, say what to do instead`, `[e] edit`, `[d]
details`. The risk row never shows an internal name; a limited risk (changes
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
A command whose prefix would authorise code the operator never saw keeps its
exact form: a shell, interpreter, or launcher (`bash -c`, `python3 -c`,
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
records one blocker observation; only the third consecutive observation of the
same condition makes the goal terminal, and progress or a changed condition
resets that audit.

**Tool rows.** Every tool call leaves one row in the transcript, committed
through the same interactive owner as the answer and never through the
ephemeral status row. The row is drawn when the call finishes, because it needs
the outcome and the duration: `✓ read calc.py · 2 lines · 75ms`, `✓ edit
calc.py +3 -1 · 12ms`, `✓ $ seq 1 20 · exit 0 · 0.4s`, `✗ $ make · exit 2 ·
1.2s`, `✗ read gone.py · …No such file or directory · 3ms`. `✓` and `✗` are the
outcome and a failed call never carries `✓`. An `exec` row is followed by the
last three lines of its output, and a line saying how many earlier lines were
left out. The duration runs from the call's start event, so it includes any
wait for a permission answer. A row is cut to the terminal width at the moment
it is drawn (a resize applies to later rows): the target gives way first, the
mark, verb, outcome and duration last. Text from a tool result is shown with
control characters replaced, never passed to the terminal.

`/expand`, also `Alt-O`, prints the full result of the most recent tool call
under its row: for `edit`, the line diff; for every other tool, the whole
result text the model received, up to 256 KiB. Only the 16 most recent calls of
the session are kept, and only the latest can be shown. It runs between turns,
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
newline submits the first line of a paste as a whole prompt — the failure that
makes pasting a stack trace impossible. A paste arrives as one edit, never as
keystrokes, so it is never interpreted as a slash command or a control
sequence.

`Ctrl+V` reads the OS pasteboard: macOS `public.png`/`public.tiff`, Linux
`wl-paste`/`xclip`, Windows `CF_DIB`. A pasteboard image, or an entire
bracketed paste that names an existing supported image file, stages a
removable chip and submits with the next turn through the same durable
`--attach` path (`docs/decisions/2026-09-29-interactive-image-paste.md`).
Escape removes the newest staged chip. Pasteboard text, and a path that is
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
| Bracketed paste | **yes** | on for every read, off on exit. A pasted stack trace is one prompt, not its first line |
| Ctrl+V | **yes** | reads the OS pasteboard; an image stages a removable chip and submits with the next turn through the `--attach` path, text inserts at the cursor |
| Ctrl+Z | **yes** (Unix) | stops ego as a shell job with the terminal back in cooked mode; `fg` restores raw mode and repaints the composer or the open modal, which stays unanswered. A running `exec` keeps running in its own process group, and its timeout does not count the time ego was stopped. Windows has no job control: the key reports that it is unsupported |
| Permission dialog | **yes** | plain `allow`, `deny`, or `amend`; amend then reads replacement JSON parameters. Invalid answers re-prompt; Ctrl-C and EOF deny. The engine keeps the configured mode first and re-prepares/re-classifies every amendment |
| Slash commands | **yes** | one shared 25-command registry drives typed-slash classification, completion, menu, grouped help, and local dispatch. Exact actions use one Enter; partial completion and pasted slash text do not execute accidentally |
| Live composer during a turn | **yes** | one Reedline owner accepts bounded external-printer output and preserves the exact draft through streaming, tools, permission waits, paste, and resize |
| Ctrl-S drafts, prompt queue, steering | **yes** | exact LIFO drafts, durable FIFO busy input, safe-boundary steering, expedited Esc delivery, replay, and exit accounting |
| Model/question menus and status line | **yes** | the shared model picker, typed question path, and width-aware status projection are live |
| `--attach` | **yes** | §3.1's durable image path; interactive `Ctrl+V` and bracketed image paths use the same admission and blob commit |
| OSC 133 A (prompt mark), OSC 7 (cwd) | excluded | not part of the final M5 terminal contract. `ego run` retains 133 C/D, OSC 9;4 and — under TUICommander — 7770 |

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

The status line is one width-aware row owned by the interactive surface, not a
shell subprocess and not model output. It renders ordered segments from a
snapshot; expensive producers such as Git update asynchronously and never
block input. The built-in segment names are `model`, `effort`, `mode`,
`context`, `cost`, `queue`, `drafts`, `interrupt`, `cwd`, `git`, and `trust`.
The queue snapshot distinguishes pending steers from ordinary queued prompts,
and the interrupt segment remains visible while expedited or ordinary
interruption is pending. `trust` renders `trusted` or `untrusted` and counts as
inactive while trusted, so width pressure drops it from exactly the sessions
whose posture is not worth reporting.

Configuration stores one final resolved shape rather than a preset plus a
second override language:

```toml
[ui.statusline]
segments = ["model", "effort", "mode", "context", "queue", "drafts", "interrupt", "cwd", "git", "trust"]
```

When `ui.statusline.segments` is absent, the surface uses that `default` order
in memory and does not mutate configuration. An explicitly stored empty list
is distinct and disables the row.

`/statusline` offers named setup presets but writes that ordered list. An empty
list disables the row. When width is insufficient, lower-priority segments
disappear as units; semantic values are never truncated into ambiguity. Queue
and draft counts, and interrupt-pending state, remain visible while active
whenever the terminal can represent their combined whole segments. If physical
width cannot represent that critical set, whole segments degrade in the order
drafts, queue, interrupt; the retained priority is interrupt, then queue, then
drafts, and the surface emits one overflow diagnostic.

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
source session and source sequence. A tool-use or truncated response, empty or
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
`docs/decisions/2026-08-24-compaction-seeds-a-clean-session.md`.

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

ACP v1 advertises only the optional `_ego/compact` extension in
`AgentCapabilities._meta`. Its request is
`{"v":1,"sessionId":"...","requestId":"..."}`; its response carries
`v`, `sourceSessionId`, `sourceSeq`, `targetSessionId`, `publication`, and
`successorStartRequestId`. A successful response registers the target so the
next standard `session/prompt` adopts it. It does not advertise `session/load`
or add standard list, fork, resume, or delete operations. Because ACP has no
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

### 6.7 Conversation forks

`ego fork ID --at ITEM_ID` and `/fork [message-id]` branch after the successful
completed turn containing that durable user or assistant message. The full answer
is included. Unknown or foreign items, non-message items, failed or incomplete
turns, and compaction checkpoints are refused without creating a target. Messages
hidden behind a compaction checkpoint cannot be selected. An inherited message
branches its owning ancestor at that ancestor's completion sequence. Title and
active goal come from the selected boundary. An explicit completed boundary can
be read while the source writer runs; a tip fork requires an inactive source.

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

`ego config ls --effective` and `/config perimeter` show the same perimeter
block as doctor: canonical workspace, admitted profile roots and their access
and origin layer, sandbox selection and backend, capability evidence, and
network mode. The interactive command projects the session's existing
confinement and admitted roots; it does not reload configuration. Headless
inspection uses the current directory and untrusted configuration resolution.
Capabilities remain `not_checked` because inspection does not execute probes.
Profile roots describe permission reach; they do not widen the exec sandbox.
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
(`docs/decisions/2026-08-22-subscription-login-is-built-in-plus-two-overrides.md`
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

`docs/16-subscription-login.md` §7.8 carries the same values with their pinned
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
an error, so a run under a `--max-cost` ceiling with no operator rates still
refuses with `NoPricingQuote`. Unknown price is not free price. It ships rows
only for providers the selectable table already admits, and only for those
whose resolver reads this file: `openai`, `openai-codex`, `anthropic`, and
`gemini`. `openrouter` is selectable but stays operator-supplied: it proxies
hundreds of third-party models under no citable contract. Ollama and the other
subscription providers enumerate their own models.

The `openai` rows are shipped and selectable. The row speaks the public
Responses contract, and
`docs/decisions/2026-09-07-a-published-contract-is-enough-to-offer-a-row.md`
admits a public row on a published contract with passing deterministic
fixtures. It amends
`docs/decisions/2026-09-03-public-provider-wires-require-live-promotion.md`,
which required a sanitized recording of a live call first: no such recording
exists, and withholding the row made every operator's key useless against a
documented endpoint to avoid an error that announces itself — a wrong Responses
body is a `400` from OpenAI on the first call. `ego models` lists the row as
available once `OPENAI_API_KEY` resolves and as `missing_credential` when it
does not.
See `docs/decisions/2026-09-07-a-builtin-catalogue-layer-below-system-and-user.md`.

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
See `docs/decisions/2026-09-12-openai-codex-subscription-bootstrap-ceiling.md`.

A full entry, with every field the schema accepts:

```toml
# `provider/model`. The model half is what goes on the wire.
[model."openai/gpt-5.6-sol".capabilities]
schema_version = 5                 # required; rejects an older document
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
cache = "short"                    # "none" | "short" | "long"
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
(`docs/decisions/2026-08-25-the-catalogue-is-a-snapshot-not-generation-authority.md`).
Only supplied rows come from this file, and they are rebuilt on every
invocation rather than cached at all; the cache holds only *discovered* rows,
from providers that enumerate themselves.

