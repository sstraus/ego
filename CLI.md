# ego — Command language

Date: 2026-08-19. Status: **§4 (output contracts) ratified by maintainer
2026-08-21 — §4.1 through §4.5 in full, not a subset.** The rest of this
document remains proposed.



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
ego resume [ID] [PROMPT]      continue a session; no ID = latest in this workspace
ego ls                        list sessions (this workspace; --all for every one)
ego fork <ID>                 branch a session
ego export <ID>               render a session (--format md|json|jsonl)
ego mcp    add|rm|ls|login    MCP server management
ego mcp-server                serve ego AS an MCP server on stdio: scoped
                              run/continue/cancel/status tools (delegation
                              surface for Claude, Codex, CI orchestrators)
ego auth   login|logout|status [PROVIDER]
ego models [--provider P] [--refresh]
ego config get|set|unset|ls|edit
ego rules  ls|allow|deny|revoke|remember|explain   (permission rules,
                              stable IDs; `explain <tool> <args>` shows the
                              canonical intent + matching rule, dry)
ego usage  [--period P] [--json]             cost and token report
ego acp                       serve ACP on stdio (the embedding surface)
ego doctor                    environment / config / credential diagnosis
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
- `ego export --sanitize` redacts secrets before a transcript leaves the
  machine (opencode's idea; mandatory for CI artifacts).

### 2.1 Availability — what ships today

The same discipline as §3.1, and it was missing here until 2026-08-21. §2 lists
fourteen commands; this build implements **four**. The other ten used to
reach the *interactive* refusal, so `ego resume` was answered with a sentence
about the permission dialog — true, and about a surface the caller had not
asked for. They are now refused **by name**, each naming what it waits on, from
the `UNBUILT` table in `crates/ego-cli/src/cli.rs`. A test binds that table to
this section, so a command can be refused as documented only while this list
still offers it.

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

| Command | M0b | Waiting on |
|---|---|---|
| `ego run` | **yes** | the scripting workhorse; the whole of §4 is about it |
| `ego [PROMPT]` | **yes** | asks once whether to trust project files before ambient reads, then runs the line-oriented session. Permission questions accept `allow`, `deny`, or `amend`; amended raw JSON parameters are prepared and classified again from the beginning |
| `ego resume` | **yes** | continues a session that already exists, never creating one. `[ID]` is optional: without it the most recently updated session of this workspace is taken. Without a prompt source it enters the interactive driver; with a prompt or `-` it remains headless |
| `ego ls` | **yes** | this workspace by default, `--all` for the whole store; `--output json\|jsonl` per §4.6 |
| `ego fork` | no | session forking (**M3**), which needs the branch point to be nameable in the log rather than just a file copy |
| `ego export` | no | replay and export with `--sanitize` (**M3**). The projection exists; the redaction contract that makes an exported session safe to hand over does not |
| `ego mcp` | no | the MCP client (**M2**). There is no server to add, remove or log in to |
| `ego mcp-server` | no | ego's own MCP server surface (**M2**): the scoped run/continue/cancel/status tools it would expose |
| `ego auth` | no | a credential store. Every provider names one environment variable (`CredentialStrategy::BearerFromEnv`), so there is nowhere for a login to put anything and nothing for a logout to remove |
| `ego models` | no | a catalogue to enumerate. Hosted facts are supplied and local ones are discovered per resolution (`docs/decisions/2026-08-21-what-a-model-resolves-to.md`), so this build resolves one model at a time and holds no list to print or refresh; generated catalogues are **M5** |
| `ego config` | no | a config writer. The loader is strict and read-only, and nothing in this build writes a config file — `-c/--set` overrides one run |
| `ego rules` | no | somewhere to keep a rule. The grammar and its stable IDs are built and `--allow`/`--deny` already reach the engine through them, so `ls` and `explain` have their material; what `allow`, `deny`, `revoke` and `remember` have nowhere to write to is a rule store — the same read-only loader `config` waits on |
| `ego usage` | no | a report across sessions, which needs both the workspace attribution `ls` waits on and the per-session cost line budgets add in **M3** |
| `ego acp` | **yes** | the Agent Client Protocol on stdio, v1. A host's `session/new` mints an ordinary ego session and its `session/prompt` runs an ordinary turn, so what it opens is listed by `ego ls` and continued by `ego resume`. v1 and not the v2 draft: see `docs/decisions/2026-08-21-the-version-an-agent-answers-with.md` §6.2 |
| `ego doctor` | no | most of what it would diagnose: the credential store, the MCP client and the sandbox backend are each waiting on their own milestone, and a report on three absent subsystems is not a diagnosis |

All four M0b demonstrations now have an entry point (`ARCHITECTURE.md` §8): an
interactive session, `ego run … | jq`, `kill → ego resume`, and the same
session over ACP. This is availability, not a release boundary; later
milestones remain dependencies of the final product.

## 3. Flags (the argument grammar)

Grouped; all long flags, short forms only for the hot five
(`-m -q -v -C -f`).

**Model and generation**

```
-m, --model P/M         provider/model slug: anthropic/claude-opus-5,
                        openrouter/z-ai/glm-4.7, ollama/qwen3 …
                        local daemons (ollama, lmstudio, vllm, llamacpp) are
                        first-class providers, catalogued live from the daemon
    --offline           no non-loopback egress from ego's own HTTP transport:
                        loopback addresses and `localhost` only, no DNS, no
                        proxy, and every redirect hop re-checked. It does not
                        confine spawned commands, hooks or MCP transports, and
                        it cannot stop an admitted local daemon from relaying
                        upstream itself (see `crates/ego-ai/src/http.rs`)
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
    --ephemeral         no session written
    --session-dir DIR   override store location
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
-q / -v / --no-color
```

**Attachments and tools**

```
    --attach PATH|URL   repeatable; files, images, URLs as context
    --tools LIST        restrict builtin tools;  --no-tools for pure generation
    --mcp NAME|@cfg.json  attach MCP server for this invocation (repeatable)
```

### 3.1 Availability — what ships today

§4.5 requires this table: *no flag documented before its contract is
implemented*. The grammar above is the **whole** design; this says which of it
you can actually run. Checked against the clap definitions in
`crates/ego-cli/src/cli.rs`, not against intent, on 2026-08-21.

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

| Flag | M0b | Note |
|---|---|---|
| `-m, --model` | **yes** | |
| `--offline` | **yes** | |
| `--thinking` | **yes** | §3 above spelled the second value `min` until 2026-08-21; the parser has only ever accepted `minimal`, so `--thinking min` was a documented value that errored. Corrected above |
| `--system`, `--append-system` | **yes** | |
| `--instructions` | **yes** | ships, and is **missing from §3's grammar** — documented only in §4.5's prose |
| `--bare` | **yes** | |
| `--session`, `--session-id`, `--ephemeral` | **yes** | one mutually exclusive group, as §4.5 requires |
| `--session-dir` | **yes** | |
| `--output` | **yes** | `text\|json\|jsonl`. On `ego ls` the two machine forms are §4.6's, not §4.1/§4.2's — a listing is not a run |
| `--all` | **yes** | `ego ls` only, and documented in §2 rather than here: §3 is the argument grammar of a *run*, and this widens a catalogue query |
| `--schema`, `--schema-retries` | **yes** | `--schema-retries` ships as its own flag; §3 mentions it only inside `--schema`'s description |
| `--budget` | **yes** | binds only on a currency-metered row — the run says so on `metering.selected` |
| `--max-turns`, `--timeout` | **yes** | |
| `--tools`, `--no-tools` | **yes** | |
| `-C`, `-c/--set`, `--profile` | **yes** | |
| `-q`, `--no-color` | **yes** | `--no-color` was marked **yes** here on 2026-08-21 while being parsed and read by nothing — the same silently vacuous flag §4.4 refuses elsewhere. It decides for real from 2026-08-22: it is the highest-precedence input to the one capability gate (`crates/ego-cli/src/render/caps.rs`), above `NO_COLOR`, above `TERM`/`COLORTERM`, above `isatty`. See `docs/decisions/2026-08-22-two-axes-and-a-terminal-that-answers.md` §1 |
| `--mode`, `--plan`, `--yolo`, `--allow`, `--deny` | **partly** | `--plan`/`--yolo` are spellings of `--mode`; two at once is a usage error. **Four of the five modes run**: `plan`, `default`, `edits`, `yolo`. `auto` is **refused by name**, because the whole of what it means is that a judge model decides and there is no judge in this build (**M1**) — it was accepted and silently behaved as `default` until 2026-08-22, which is the accepted-and-ignored flag §4.5 forbids, wearing a value rather than a spelling. **Four of the seven rule heads run**: `read`, `write`, `list`, `search`. `exec`, `net` and `tool` parse and get stable IDs, and are then **refused by name at load** — no tool in this build produces those operations, so a rule naming one would enforce nothing. `write` spent 2026-08-23 on the refused side and has come back, which is the mechanism working rather than churn: `Op::Write` had a matcher and a scope arm while no builtin ever constructed one, so `--deny 'write(**)'` loaded, took an ID, and matched nothing — the vacuous boundary this whole mechanism exists to prevent, and the one an operator cannot tell apart from a boundary that works. `write` the *tool* now constructs one, so the head enforces again, on the same rule IDs. The five tools are read, glob, grep, write and edit. `edit` adds no head: it prepares a `Read` **and** a `Write` of the same file, so `--allow 'write(src/**)'` on its own does not authorise it — the read is uncovered and the call stops there. So `--allow 'exec(git *)'` (§3's example, which is design) is exit 2 here, as is `--allow 'bash(git *)'` with §3's teaching message. All of them refuse **before** the run starts |
| `--sandbox` | no | waiting on a sandbox backend (**M1**). No process is spawned anywhere in this build, so there is nothing yet to confine |
| `-f`, `--file`, `--var` | no | waiting on executable agent files (**M4**). §5 is a design, not a feature: the shebang form runs nothing today |
| `--attach` | no | waiting on a non-text content block — the conversation model carries text, tool calls and tool results, so an image has nothing to be sent in |
| `--mcp` | no | waiting on the MCP client (**M2**). `mcp_errors` exists on `session.started`, but no flag attaches a server |
| `--retry`, `--fallback` | no | waiting on a retry seam. Delivery certainty is implemented and enforced; a generation owns its reservation for its whole life, so re-issuing one changes the attempt contract rather than adding a loop |
| `-v`, `--verbose` | no | waiting on a **decision**, not a subsystem — the only such entry. §4.7 says exactly what `-q` suppresses and nothing anywhere says what `-v` adds. It was marked **yes** beside `--no-color` until 2026-08-23 while being parsed and read by nothing, which is `--no-color`'s own defect sitting in `--no-color`'s own row. The obvious answer, turning up `tracing`, is not obvious: this build installs no subscriber at all, so every `tracing::warn!` in the workspace already goes nowhere, and adding one puts a second writer on the stream §4.7's marks and notification own (R12) |

**Ten documented flag spellings do not ship.** That is the gap this table
exists to stop being invisible, and it is not a defect list — every one of them
refuses by name and says what it is waiting on. The defect would be §3 reading,
to someone who has not run the binary, as though all of it worked.

Milestones in bold above are from `ARCHITECTURE.md` §8's table, which owns
them; the flags with none are waiting on a subsystem that table does not place.

## 4. Output contracts

### 4.1 `--output json` (one object on stdout at completion)

```json
{
  "v": 6,
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
2 usage error 3 permission needed in non-interactive and not granted
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

Headless runs never hang on a prompt: an unresolvable permission ⇒ exit 3, with
the denied action in the JSON result. Scripts opt in with `--allow` rules, a
stronger `--sandbox`, or `--yolo` — never by answering prompts.

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
are generated from the same clap definitions and snapshot-tested.

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
  "v": 1,
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
  key and a key nobody has set are two states with identical bytes.
- `created` and `updated` are UTC milliseconds since the epoch, the same
  `Timestamp` the session log carries.
- The empty store writes `{"v":1,"sessions":[]}` and exits 0. A script that
  gets no output cannot tell an empty store from a command that failed.

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
Every payload is stripped of control characters before it goes into a sequence:
a tool name, an error message and an intent line all come from somewhere that
is not ego.

## 5. Agent files — the scripting language

An **executable agent file** is YAML frontmatter + prompt body. Same format
family as skills; `--var` fills `{{placeholders}}`; shebang makes it a program:

```
#!/usr/bin/env -S ego run -f
---
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

Frontmatter keys mirror the CLI flags one-to-one — the file *is* a saved
command line plus a prompt. Nothing else examined has this as a first-class,
executable unit (Claude Code's slash commands are REPL-only; pi's prompt
templates are not executables).

## 6. Interactive surface (kept deliberately small)

Slash commands: `/help /model /thinking /mode /plan /yolo /permissions /mcp
/sessions /fork /compact /clear /usage /export /doctor /quit`. History,
completion and multiline via reedline.
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

**Ctrl+V asks for an image first and text second.** The image cannot come
through the terminal: bracketed paste carries what the emulator decided to
send, which is text, and OSC 52's read form is disabled by default on every
host in the matrix — it lets anything on the far end of a PTY exfiltrate the
clipboard, so a host that answered it would be the bug. It comes from the
platform pasteboard, read directly. An image found there becomes a temporary
file and then the same content block `--attach` produces; anything else is
pasted as text.

That works where ego runs on the machine that owns the pasteboard. Over SSH
there is usually no pasteboard to read, and Ctrl+V falls through to text — the
image is attached by naming it instead (`--attach PATH|URL`, §3). Stated here
because it is a limit of where the clipboard lives, not a limit ego can lift:
no CLI on the far end of a PTY can reach the pasteboard in front of the person
typing.

### 6.2 Availability — what ships today

Same discipline as §2.1 and §3.1. The reader, trust question and permission
answer path ship. `ego` with no subcommand opens a session: it reads lines,
runs a turn for each and renders it through §4. The editor and the renderer are
never live at the same time, which is how §4's "exactly one writer" survives a
second writer existing at all: each read ends before a turn renders.

| Surface | M0b | Note |
|---|---|---|
| Input line, history, multiline | **yes** | reedline. `Ctrl+C` abandons the line and keeps the session; `Ctrl+D` ends it |
| Bracketed paste | **yes** | on for every read, off on exit. A pasted stack trace is one prompt, not its first line |
| Ctrl+V | **refused by name** | it asks for a pasteboard *image* first (§6.1), and there is nowhere to put one — the same gap that refuses `--attach`. It refuses rather than silently pasting text, because only one of those two can be discovered |
| Permission dialog | **yes** | plain `allow`, `deny`, or `amend`; amend then reads replacement JSON parameters. Invalid answers re-prompt; Ctrl-C and EOF deny. The engine keeps the configured mode first and re-prepares/re-classifies every amendment |
| Slash commands | no | waiting on the subsystems they would drive; §2.1 already refuses each by name |
| `--attach` | no | §3.1's row: waiting on a non-text content block |
| OSC 133 A (prompt mark), OSC 7 (cwd) | no | deliberately: both are owned by the surface rather than by a run, and their exact terminal contract is not implemented. `ego run` emits 133 C/D, OSC 9;4 and — under TUICommander — 7770 |

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
bundle. Precedence: flags > env (`EGO_MODEL`, `EGO_PROFILE`, …) > local >
project > user > system, with system ceilings binding everyone. Credentials
resolve through a source chain (OS keyring, systemd credentials, env,
`credential_command` for Vault/SOPS, device-flow OAuth) via `ego auth` —
never plaintext in config.

