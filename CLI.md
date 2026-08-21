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
fourteen commands; this build implements **one**. The other thirteen used to
reach the *interactive* refusal, so `ego resume` was answered with a sentence
about the permission dialog — true, and about a surface the caller had not
asked for. They are now refused **by name**, each naming what it waits on, from
the `UNBUILT` table in `crates/ego-cli/src/cli.rs`. A test binds that table to
this section, so a command can be refused as documented only while this list
still offers it.

A word that is **not** on this list stays a prompt: §2's first line is
`ego [PROMPT]`, so `ego "explain this failing test"` is the interactive form
and gets the interactive answer. `ego -- resume` likewise prompts the word.

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
| `ego [PROMPT]` | no | the terminal surface itself — a line editor for the prompt, and the dialog that renders a `PermissionRequest` and reads a person's answer. The `Approver` port that dialog would occupy is built and the mode already answers through it (**M1**); what is missing is the human seat and the screen it draws on |
| `ego resume` | no | a session the catalogue can attribute to a workspace. `SessionMeta.workspace_root` is `None` for every entry, so `no ID = latest in this workspace` has nothing to select on. Where the root will live is decided — the log header, at a new codec version — and the bump is what is waiting: under the version check the header now carries, a v5 log makes every session already on disk unopenable, and accepting them anyway is a backward-compatibility decision this build will not take on its own. The machinery underneath is already here: `--session NAME` attaches to a named session and the store repairs a torn tail on the way in |
| `ego ls` | no | the same workspace attribution `resume` waits on. `SessionStore::list` exists and returns every session, which is what `--all` would show; the default — this workspace only — is the half that cannot be computed yet |
| `ego fork` | no | session forking (**M3**), which needs the branch point to be nameable in the log rather than just a file copy |
| `ego export` | no | replay and export with `--sanitize` (**M3**). The projection exists; the redaction contract that makes an exported session safe to hand over does not |
| `ego mcp` | no | the MCP client (**M2**). There is no server to add, remove or log in to |
| `ego mcp-server` | no | ego's own MCP server surface (**M2**): the scoped run/continue/cancel/status tools it would expose |
| `ego auth` | no | a credential store. Every provider reads one environment variable (`Auth::bearer_from_env`), so there is nowhere for a login to put anything and nothing for a logout to remove |
| `ego models` | no | a catalogue to enumerate. Hosted facts are supplied and local ones are discovered per resolution (`docs/decisions/2026-08-21-what-a-model-resolves-to.md`), so this build resolves one model at a time and holds no list to print or refresh; generated catalogues are **M5** |
| `ego config` | no | a config writer. The loader is strict and read-only, and nothing in this build writes a config file — `-c/--set` overrides one run |
| `ego rules` | no | somewhere to keep a rule. The grammar and its stable IDs are built and `--allow`/`--deny` already reach the engine through them, so `ls` and `explain` have their material; what `allow`, `deny`, `revoke` and `remember` have nowhere to write to is a rule store — the same read-only loader `config` waits on |
| `ego usage` | no | a report across sessions, which needs both the workspace attribution `ls` waits on and the per-session cost line budgets add in **M3** |
| `ego acp` | no | an agent that serves the protocol on stdio. `ego-acp` already projects events onto v1 and v2 (`project_acp_v1`/`project_acp_v2`), so what is missing is the connection and its version negotiation, not the mapping |
| `ego doctor` | no | most of what it would diagnose: the credential store, the MCP client and the sandbox backend are each waiting on their own milestone, and a report on three absent subsystems is not a diagnosis |

**Three of M0b's four exit criteria are commands that do not exist yet**
(`ARCHITECTURE.md` §8: interactive, `ego run … | jq`, `kill → ego resume`, and
the same session over ACP). Only the second ships. That is the fact this table
exists to stop being invisible.

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
not swallowed, and not reported as an unknown argument. That mechanism already
exists: `UNAVAILABLE` in `crates/ego-cli/src/cli.rs:67-94`, checked before
clap sees the tokens, and it covers every "no" below. This table is the
document half of it; the code is the source of truth, and the two were compared
flag by flag against the built binary to write this.

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
| `--output` | **yes** | `text\|json\|jsonl` |
| `--schema`, `--schema-retries` | **yes** | `--schema-retries` ships as its own flag; §3 mentions it only inside `--schema`'s description |
| `--budget` | **yes** | binds only on a currency-metered row — the run says so on `metering.selected` |
| `--max-turns`, `--timeout` | **yes** | |
| `--tools`, `--no-tools` | **yes** | |
| `-C`, `-c/--set`, `--profile` | **yes** | |
| `-q`, `-v`, `--no-color` | **yes** | |
| `--mode`, `--plan`, `--yolo`, `--allow`, `--deny` | **yes** | `--plan`/`--yolo` are spellings of `--mode`; two at once is a usage error. **Four of the seven rule heads run**: `read`, `write`, `list`, `search`. `exec`, `net` and `tool` parse and get stable IDs, and are then **refused by name at load** — no tool in this build produces those operations, so a rule naming one would enforce nothing. So `--allow 'exec(git *)'` (§3's example, which is design) is exit 2 here, as is `--allow 'bash(git *)'` with §3's teaching message. Both refuse **before** the run starts |
| `--sandbox` | no | waiting on a sandbox backend (**M1**). No process is spawned anywhere in this build, so there is nothing yet to confine |
| `-f`, `--file`, `--var` | no | waiting on executable agent files (**M4**). §5 is a design, not a feature: the shebang form runs nothing today |
| `--attach` | no | waiting on a non-text content block — the conversation model carries text, tool calls and tool results, so an image has nothing to be sent in |
| `--mcp` | no | waiting on the MCP client (**M2**). `mcp_errors` exists on `session.started`, but no flag attaches a server |
| `--retry`, `--fallback` | no | waiting on a retry seam. Delivery certainty is implemented and enforced; a generation owns its reservation for its whole life, so re-issuing one changes the attempt contract rather than adding a loop |

**Eight documented flag spellings do not ship.** That is the gap this table
exists to stop being invisible, and it is not a defect list — every one of them
refuses by name and says what it is waiting on. The defect would be §3 reading,
to someone who has not run the binary, as though all of it worked.

Milestones in bold above are from `ARCHITECTURE.md` §8's table, which owns
them; the flags with none are waiting on a subsystem that table does not place.

## 4. Output contracts

### 4.1 `--output json` (one object on stdout at completion)

```json
{
  "v": 1,
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



`--instructions FILE` is the headless opt-in and is **not** ambient: it names
a file rather than discovering one, so it survives `Posture::Bare` and
*replaces* the discovered set rather than adding to it. `--bare` together with
`--instructions` is a conflict for the applicability matrix above, not for the
loader — from a resolved posture alone, "the user demanded bare" and "the
workspace is untrusted" are the same value and want opposite answers. See
`docs/decisions/2026-08-21-instructions-a-workspace-may-hand-a-model.md`.

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
completion and multiline via reedline. The permission prompt is fx's proven
dialog: number keys choose, **Tab amends the proposed command**, Enter
confirms, Esc cancels.

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

