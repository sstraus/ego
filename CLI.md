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

## 2. Command set

```
ego [PROMPT]                  interactive session (prompt submitted if given)
ego run [PROMPT]              headless one-shot; PROMPT from argv, -f FILE, or stdin
ego resume [ID] [PROMPT]      continue a session; no ID = latest in this workspace;
                              --answer QUESTION_ID=JSON answers pending input;
                              --name and --goal are metadata-only operations
ego ls                        list sessions (this workspace; --all for every one)
ego fork <ID>                 branch a session
ego compact <ID> --request-id UUID
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
ego usage  [--period P] [--json]             cost and token report
ego acp                       serve ACP on stdio (the embedding surface)
ego doctor [--json]           read-only redacted health snapshot
```

Notes:

- `ego run` is the scripting workhorse. `ego resume <id> "next step"` chains
  scripted multi-turn conversations — a pipeline can carry a session through
  several stages. One session pool across interactive and headless runs
  (Codex split them and had to add `--include-non-interactive` to re-merge).
- There is deliberately **no bespoke RPC mode**: embedders get `ego acp`.
  pi invented `rpc`/`json` modes; ACP is that, standardised.
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
| `ego fork` | **yes** | creates a durable lineage branch at an explicit valid source boundary without copying source spend or run state |
| `ego compact` | **yes** | a required request ID derives one idempotent clean target from the existing idle compaction operation and supports later continuation |
| `ego export` | **yes** | local transcript export plus the producer-only version-1 structural bundle under `--sanitize`; Markdown, JSON, JSONL, stdout, and atomic file publication follow §2.1 |
| `ego mcp` | **yes** | `add`, `rm`, `ls [--json]`, and `reload [NAME]` use one live admitted registry and truthful health projection. `login NAME` is typed but refuses before network access until the product CIMD origin and loopback redirect are published |
| `ego mcp-server` | **yes** | serves scoped `run`/`continue`/`cancel`/`status`/`compact` tools and sanitized summary/output resources on stdio. `--max-concurrent-runs`, `--retained-requests`, and `--max-resource-bytes` are required non-zero deployment bounds; `-C`/the current directory is the sole canonical root and stdout remains protocol-only |
| `ego auth` | **yes** | `login\|logout\|status [PROVIDER]`; credentials live in one locked `$EGO_HOME/auth.json`, while TOML commands and discovered auth plugins can supply login tokens |
| `ego models` | **yes** | prints the shared current-model catalogue as deterministic text or versioned JSON; `--provider` filters both model and source rows, and `--refresh` updates authoritative discovered slices without promoting cached display facts into generation authority |
| `ego hooks` | **yes** | `ls [--json]` reads the same immutable admitted hook catalogue used by execution; it neither reloads nor edits hook policy |
| `ego config` | no | a config writer. The loader is strict and read-only, and nothing in this build writes a config file — `-c/--set` overrides one run |
| `ego rules` | no | somewhere to keep a rule. The grammar and its stable IDs are built and `--allow`/`--deny` already reach the engine through them, so `ls` and `explain` have their material; what `allow`, `deny`, `revoke` and `remember` have nowhere to write to is a rule store — the same read-only loader `config` waits on |
| `ego usage` | **yes** | reports the versioned main/judge/compaction/total accounting DTO for this workspace; `--period` accepts `all` or a positive `s`/`m`/`h` duration and `--json` emits the same DTO the text view renders |
| `ego acp` | **yes** | the Agent Client Protocol on stdio, v1. A host's `session/new` mints an ordinary ego session and its `session/prompt` runs an ordinary turn, so what it opens is listed by `ego ls` and continued by `ego resume`. v1 and not the v2 draft: see `docs/decisions/2026-08-21-the-version-an-agent-answers-with.md` §6.2 |
| `ego doctor` | no | a unified diagnostic contract and actionable probes. Authentication, model resolution, sandbox execution, and MCP now exist, but no health projection composes their sanitized status and actionable remediation |

All four M0b demonstrations now have an entry point (`ARCHITECTURE.md` §8): an
interactive session, `ego run … | jq`, `kill → ego resume`, and the same
session over ACP. This is availability, not a release boundary; later
milestones remain dependencies of the final product.

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
                        auto = judge model evaluates the canonical call and
                        allows/denies/escalates (permissions.judge in config:
                        a cheap tier like gpt-5.6-luna or GLM, or local ollama)
    --sandbox S         ro|workspace|off             (off = classifier only)
    --allow RULE        repeatable   e.g. --allow 'exec(git *)' --allow 'net(api.github.com)'
    --deny RULE         repeatable; deny beats allow
```

A rule's head names an **effect** and never a tool: `exec(git *)`, not
`bash(git *)`. A tool-named rule's coverage is a function of the tool
inventory, so every new tool that shells out is a silent hole in every rule
ever written, and the hole is invisible because the rule still reads
correctly. `bash(...)` is a parse error whose message names `exec` rather than
an alias for it. The heads are `read`, `write`, `list`, `search`, `exec`,
`net` and `tool` — grammar in
`docs/decisions/2026-08-21-a-rule-that-means-exactly-one-thing.md`, which
corrects the example this line used to carry.

Rules follow a published formal grammar matched against the *canonical* call
(resolved paths, argv, endpoints — never rendered shell strings); the
mode × sandbox matrix is published and contradictory combinations are
rejected, not guessed. Every control is individually disableable; hard
ceilings come only from an administrator's `/etc/ego`.

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
`crates/ego-cli/src/cli.rs`, not against intent, on 2026-09-03.

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
| `--system`, `--append-system` | **yes** | |
| `--instructions` | **yes** | ships, and is **missing from §3's grammar** — documented only in §4.5's prose |
| `--bare` | **yes** | |
| `--session`, `--session-id` | **yes** | durable session choices; the parser places them in the §4.5 mutually exclusive session group |
| `--ephemeral` | no | recognized only to refuse by name with usage error exit 2 before opening or creating a session or causing an effect; use the durable default, `--session`, or `--session-id` |
| `--session-dir` | **yes** | |
| `--artifact-dir` | **yes** | `ego run`/`ego resume` only, and only with the exact selected profile name `ci`; flags override `[profile.ci].artifact_dir` |
| `--answer` | **yes** | `ego resume --answer QUESTION_ID=JSON` validates and durably settles pending questions before continuing the session |
| `--name`, `--goal` | **M5 target** | regression case adds the exact metadata-only `ego resume` operations from §2; the current parser does not serve them |
| `--output` | **yes** | `text\|json\|jsonl`. On `ego ls` the two machine forms are §4.6's, not §4.1/§4.2's — a listing is not a run |
| `--all` | **yes** | `ego ls` only, and documented in §2 rather than here: §3 is the argument grammar of a *run*, and this widens a catalogue query |
| `--schema`, `--schema-retries` | **yes** | `--schema-retries` ships as its own flag; §3 mentions it only inside `--schema`'s description |
| `--budget` | **yes** | binds only on a currency-metered row — the run says so on `metering.selected` |
| `--max-turns`, `--timeout` | **yes** | |
| `--tools`, `--no-tools` | **yes** | |
| `-C`, `-c/--set`, `--profile` | **yes** | |
| `-q`, `--no-color` | **yes** | `--no-color` was marked **yes** here on 2026-08-21 while being parsed and read by nothing — the same silently vacuous flag §4.4 refuses elsewhere. It decides for real from 2026-08-22: it is the highest-precedence input to the one capability gate (`crates/ego-cli/src/render/caps.rs`), above `NO_COLOR`, above `TERM`/`COLORTERM`, above `isatty`. See `docs/decisions/2026-08-22-two-axes-and-a-terminal-that-answers.md` §1 |
| `--mode`, `--plan`, `--yolo`, `--allow`, `--deny` | **yes** | `--plan`/`--yolo` are spellings of `--mode`; two at once is a usage error. All five modes enforce. `auto` resolves a separate configured judge and routes its closed verdict before any human escalation. Five of the seven rule heads enforce: `read`, `write`, `list`, `search`, and `exec`; `net` and `tool` remain refused by name because no builtin produces those operations. The seven builtins are read, glob, grep, write, edit, exec, and read_tool_result. `edit` prepares both `Read` and `Write`; `read_tool_result` uses its session-evidence capability rather than a user-authored rule head. |
| `--sandbox` | **yes** | `ro\|workspace\|off`, default `workspace`, enforced by the measured platform backend before a prepared process can launch; an unavailable required capability fails closed |
| `-f`, `--file`, `--var` | **yes** | strict version-1 agent files load through the existing bounded YAML implementation, apply their configuration layer below flags, and substitute declared body variables. M4 extracts the shared envelope for skills and still owns the direct executable shebang end-to-end proof, not these flags' availability |
| `--attach` | **yes** | repeatable local-path or HTTPS image input; bytes enter the durable session-blob/content path before model-specific projection and unsupported model/profile combinations refuse before acquisition |
| `--mcp` | **yes** | repeatable ordered selection of effective `NAME` entries and strict invocation-scoped `@FILE` entries; the selected live admission is shared by dispatch, `/tools`, `/doctor`, and deterministic `mcp_errors` |
| `--retry`, `--fallback` | **M5 target** | regression case installs the one-turn attempt coordinator; current delivery certainty alone does not authorize re-issue |
| `-v`, `--verbose` | excluded | the current binary refuses this legacy placeholder by name. No final behavior was ratified; regression case retires the spelling while preserving the named retired-flag error contract |

The table distinguishes implemented flags, intentional refusals, frozen M5
target spellings, and the excluded verbose placeholder. The defect would be §3
reading, to someone who has not run the binary, as though all of it worked.

Milestones in bold above are from `ARCHITECTURE.md` §8's table, which owns
them; the flags with none are waiting on a subsystem that table does not place.

## 4. Output contracts

### 4.1 `--output json` (one object on stdout at completion)

```json
{
  "v": 9,
  "session": "s_9f2k…",
  "result": "…final assistant text or --schema-validated object…",
  "exit_reason": "done|budget|max_turns|timeout|permission|error",
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
4 provider/network error   5 budget or turn limit   124 timeout
130 SIGINT    143 SIGTERM
```

Only Gemini ever documented an exit-code table; Codex and Claude collapse
almost everything into 0/1. Scripting needs the distinctions (retry on 4,
escalate on 3, alert on 5). The shutdown contract is explicit (Claude's, which
is right): on SIGTERM abort the turn, kill the tool process tree, run
session-end hooks, flush the session file, exit 143. Flags are never removed
silently: a retired flag errors **by name** with its replacement.

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



`--instructions FILE` is the headless opt-in and is **not** ambient: it names
a file rather than discovering one, so it survives `Posture::Bare` and
*replaces* the discovered set rather than adding to it. `--bare` together with
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

Slash commands: `/help /model /thinking /mode /plan /yolo /permissions /mcp
/sessions /fork /compact /clear /usage /statusline /export /doctor /quit /new
/name /goal /tools /skills /hooks /version`. These 24 commands are the complete
maintainer-selected surface; a missing implementation remains required work rather
than an implicit de-selection. Typing `/` at the beginning of the composer
opens a filterable menu; the menu, shortcut bindings, help, and enabled-state
checks are projections of one action registry rather than parallel command
implementations.

That paragraph is the final contract. The current implementation has the shared
24-command action registry: a typed complete slash is classified as an action,
while bracketed paste and non-command text remain ordinary prompts. Completion,
menu, help, dispatch, and live availability read that one registry. Actions
whose final owner has not landed remain visible with their explicit disabled
reason rather than becoming model input or silently disappearing.

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

Current availability remains narrower: `/doctor` dispatches only the admitted
MCP health projection, `/name` and `/goal` are disabled, and root `ego doctor`
is refused. M5 replaces that partial doctor handler and enables the root
command together over one redacted, versioned, non-mutating health snapshot.

The permission prompt has a plain representation that works in every terminal
and scripted transcript: `allow`, `deny`, or `amend`. Amend reads a complete
JSON object of replacement tool parameters, then re-runs prepare and policy
from the beginning. Invalid answers re-prompt; Ctrl-C and EOF deny.

### 6.1 Paste

The input line runs in **bracketed paste** mode (`ESC[?2004h`, off on exit).
Every terminal in the compliance matrix implements it, and without it a pasted
newline submits the first line of a paste as a whole prompt — the failure that
makes pasting a stack trace impossible. A paste arrives as one edit, never as
keystrokes, so it is never interpreted as a slash command or a control
sequence.

εgo does not read a platform pasteboard and assigns no image semantics to
`Ctrl+V`. Terminal text paste remains ordinary editor input. Image input uses
the explicit, scriptable `--attach IMAGE_PATH|HTTPS_IMAGE_URL` grammar; no
clipboard fallback or OSC 52 read path is part of the product.

### 6.2 Availability — what ships today

Same discipline as §2.1 and §3.1. This table records the implementation that
ships today; §§6.3-6.5 are the final product contract and are not optional
scope. The current reader, trust question, permission answer path, and live
composer ship. `ego` with no subcommand opens a session whose single Reedline
owner stays active while rendered output, tools, and modal prompts cross
bounded channels.

| Surface | Current | Note |
|---|---|---|
| Input line, history, multiline | **yes** | reedline. `Ctrl+C` abandons the line and keeps the session; `Ctrl+D` ends it |
| Bracketed paste | **yes** | on for every read, off on exit. A pasted stack trace is one prompt, not its first line |
| Ctrl+V | legacy image action unavailable | M5 removes the special image placeholder; the final surface leaves text paste to the terminal/editor and uses `--attach` for images |
| Permission dialog | **yes** | plain `allow`, `deny`, or `amend`; amend then reads replacement JSON parameters. Invalid answers re-prompt; Ctrl-C and EOF deny. The engine keeps the configured mode first and re-prepares/re-classifies every amendment |
| Slash commands | **partly** | one shared 24-command registry drives typed-slash classification, completion, menu, help, and dispatch. Enabled actions have installed local handlers; unavailable actions remain registered and name their owning subsystem |
| Live composer during a turn | **yes** | one Reedline owner accepts bounded external-printer output and preserves the exact draft through streaming, tools, permission waits, paste, and resize |
| Ctrl-S drafts, prompt queue, steering | **yes** | exact LIFO drafts, durable FIFO busy input, safe-boundary steering, expedited Esc delivery, replay, and exit accounting |
| Model/question menus and status line | **yes** | the shared model picker, typed question path, and width-aware status projection are live |
| `--attach` | **yes** | §3.1's durable image path; no pasteboard spelling is required |
| OSC 133 A (prompt mark), OSC 7 (cwd) | excluded | not part of the final M5 terminal contract. `ego run` retains 133 C/D, OSC 9;4 and — under TUICommander — 7770 |

### 6.3 Live composer, drafts, queue, and steering

The composer remains editable while a provider streams, a tool runs, or the
harness waits at an interactive boundary. Renderer output enters the
interactive surface through a bounded channel; reedline's external printer
appends it above the composer and restores the exact buffer and cursor. In
interactive mode reedline is therefore the terminal writer. In headless modes
the existing renderer remains the writer. There is never one of each writing
the same terminal.

`Ctrl-S` with a non-empty composer pushes the exact multiline buffer onto a
session-local LIFO draft stash and clears the composer. `Ctrl-S` with an empty
composer restores the newest stash. Stashes are not submitted, do not enter
model history, and are not silently overwritten; exiting with stashes reports
their count.

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
can extend the durable expedited prefix. `Esc` with no pending input retains
ordinary interrupt behaviour.

The three input states do not overlap. A **Draft** is a local LIFO editor stash.
A durable **PendingSteer** is FIFO input accepted during a busy regular turn. A
durable ordinary **Queued** FIFO is reserved for non-steerable or
idle-transition cases. One queue coordinator owns both durable forms and their
exactly-once consumption; there is no steering inbox or action-registry-local
queue. None of these paths bypasses budget, permission, sandbox, attempt
settlement, usage, cost, or tool-result accounting.

### 6.4 Menus, model selection, and questions

`Option-P` on macOS and `Alt-P` elsewhere, normalised as terminal `Meta-P`,
open the same action as `/model`. A terminal that does not expose Option as a
modifier still has `/model` as the exact equivalent. The picker shows provider,
model, context capability, and reasoning effort. Arrow keys navigate; `Enter`
sets the default for new sessions, `s` applies only to this session, and `Esc`
cancels. A selection made while a request is in flight applies at the next
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
`context`, `cost`, `queue`, `drafts`, `interrupt`, `cwd`, and `git`. The queue
snapshot distinguishes pending steers from ordinary queued prompts, and the
interrupt segment remains visible while expedited or ordinary interruption is
pending.

Configuration stores one final resolved shape rather than a preset plus a
second override language:

```toml
[ui.statusline]
segments = ["model", "effort", "mode", "context", "queue", "drafts", "interrupt", "cwd", "git"]
```

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
interactive surface to it. The new session resolves ordinary system, project,
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
`CompactionTransitionService`. `/compact` mints a fresh request ID. The exact
headless grammar is:

```
ego compact ID --request-id UUID [--output text|json|jsonl]
            [-C DIR] [--session-dir DIR]
```

There is no alias and no implicit request ID. The target is deterministically
derived from the request ID; that same value is the
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

All config mutations use the one atomic transaction from the accepted config
ADR. `ego config edit` writes a mode-0600 candidate, selects `$VISUAL` then
`$EDITOR`, parses the editor value as argv without a shell, and edits outside
the config lock. Publication reacquires the lock, compares the original digest,
strictly parses the complete candidate, applies system ceilings, and atomically
replaces the file. Conflict or publication uncertainty preserves an explicit
candidate recovery path; cancel, unchanged content, or editor failure removes
the candidate and leaves authoritative config untouched.

`ego doctor [--json]` and `/doctor` are two renderings of one ordered,
versioned `HealthSnapshot`: config, credential status, session store,
sandbox/backend, provider/model catalogue, MCP, skills, and hooks. Doctor is
non-mutating and redacted. It may read existing facts and perform bounded local
or loopback checks; it never logs in, refreshes credentials or catalogues,
reloads extensions, edits config, probes a hosted provider, uploads logs, or
widens egress. `ok`, `warning`, `error`, and `not_checked` stay distinct; root
exit is 1 only when an operational error exists. Until the shared service
lands, root doctor is unavailable and slash doctor remains MCP-only.

