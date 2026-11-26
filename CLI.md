# ego — Command language

Date: 2026-08-19; M5 contract freeze: 2026-09-03. Status: **§4 (output
contracts) ratified by Boss 2026-08-21 — §4.1 through §4.5 in full, not a
subset.** Story 082 freezes the
M5 amendments in this document; unaffected previously proposed prose remains
proposed.

§4 is ratified *whole* deliberately, against the advice of both root and
a second reviewing agent, who proposed ratifying only the part M0b could already prove and
marking the rest unavailable. That advice was wrong here: `AGENTS.md`'s
end-to-end rule forbids reduced products and provisional public contracts,
and "mark required final behaviour unavailable merely to close a milestone"
is the thing it names. So §4 is the target in full, and the work it implies —
a real termination taxonomy, a live event sink, a permission seam — is work
to do, not scope to trim.
Companion to `ARCHITECTURE.md`.
Final-M5 grammar is normative even before its implementation story lands;
availability sections state what the current binary actually serves. See the
[0.4.0 release notes](docs/release-notes/0.4.0.md) for the scope of that release.

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
   locked behind the REPL.

### Persistent headless stdin

`ego run --input-format jsonl --output jsonl` and
`ego resume ID --input-format jsonl --output jsonl` retain one headless session,
writer and provider context. Send one UTF-8 JSON object per line:

```json
{"prompt":"Explain the change"}
{"prompt":"Now check the edge cases"}
```

Do not combine this input mode with a positional prompt, `-`, `--file` or
`--answer`. Empty, malformed or non-UTF-8 records stop with exit 2 and the
line number; that record starts no turn. Turns run in strict FIFO order.
EOF drains complete records already received and exits cleanly. SIGTERM
cancels the active turn, discards pending input and exits 143 even if stdin
remains open. The session can then be resumed normally.

With `--output jsonl`, existing events are followed by one `type: "result"`
record per completed prompt. It contains the ordinary result report, including
`session`, `result` (the answer) and `usage.cache_read_tokens`. Usage and cost
are cumulative session totals, as in `--output json`; cache counts come from
the provider and do not guarantee a cache hit or a cache lifetime. A fork can
use the same input mode: `ego resume ID --fork --input-format jsonl --output jsonl`.

<!-- ego-cli-command-set:start -->
## 2. Command set

```
ego [PROMPT]                  interactive session (prompt submitted if given)
ego run [PROMPT]              headless one-shot; PROMPT from argv or stdin; -f FILE loads an agent file
ego continue [ID] [PROMPT]    alias of resume; same session selection and options
ego resume [ID] [PROMPT]      continue a session; no ID = interactive picker or headless latest;
                              --answer QUESTION_ID=JSON answers pending input;
                              --title TITLE selects the session by its exact title;
                              --name and --goal-operation are metadata-only operations
ego ls                        list sessions (this workspace; --all for every one)
ego rm <ID> [--yes] [--force]  tombstone one session; confirm unless --yes;
                              --force acknowledges an uncertain run
ego rm --incompatible --yes   permanently remove unsupported-format session directories;
                              keep readable sessions, including descendants
ego rm --expired --yes        permanently remove sessions not opened for
                              sessions.retention_days (30); keep open, held
                              and recently resumed sessions and their sources
ego fork <ID> [--at ITEM_ID]   branch after a completed turn
ego rewind [-C DIR] <ID> --at ITEM_ID
                              restore the files changed after a turn, then fork there
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
                              (delegation surface for other agents and CI) plus
                              sanitized resources; no MCP Tasks or prompts
ego auth   [--account N] login|logout|remove|status|usage [PROVIDER]
ego models [--account N] [--provider P] [--refresh] [--json]
ego hooks  ls [--json]                     read-only admitted hook catalogue
ego config capabilities|snapshot|batch|get|set|unset|ls|edit
ego roots  add|rm|ls                         user perimeter roots
ego rules  ls|allow|deny|revoke|remember|explain   (permission rules,
                              stable IDs; `explain <tool> <args>` shows the
                              canonical intent + matching rule, dry)
ego usage  [--period P] [--all-roles] [--json] cost and token report
ego acp [--tools LIST | --no-tools] [--no-hooks] [--allow RULE] [--deny RULE]
        [--profile NAME] [--append-system TEXT|@file] [--output-style NAME]
        [--instructions FILE] [--exclusive-read-only [--env-allow NAME]...]
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
                              --allow-public-bind. Authenticated LAN clients
                              use the selected sandbox ceiling; the address
                              does not force `adaptive` or `off` to `workspace`.
ego a2a --listen ENDPOINT [--allow-public-bind] [--sandbox LEVEL]
                              serve Agent2Agent discovery information.
                              ENDPOINT is a socket path —
                              authenticated by the operating system, the
                              preferred form — or an address, which requires a
                              credential in EGO_A2A_TOKEN. An address reachable
                              from outside this machine is refused by name
                              unless the operator asks for it
ego worktree show|cleanup RECEIPT_UUID   inspect or safely remove a retained local checkout
ego build-recipes ls|show|disable|enable|rm [ID]
                             inspect, disable, enable or delete the learned
                             build-output recipes of this workspace (§7.5)
ego doctor [--json] [--offline] [--probe-network]  read-only redacted health snapshot
ego setup [--profile NAME] [--directory PATH]
                             terminal configuration guide for user defaults and profiles;
                             presence-only provider credential help (no secret read,
                             no provider call; diagnose with ego doctor); a scrollable
                             review with default No before an atomic save
```

Notes:

- In `ego setup` and `/setup`, use PageUp/PageDown to move one page of rows
  and Home/End to select the first or last row. Press `S` on any settings page
  to review staged edits. Saving still requires an explicit Yes. The footer
  shows the navigation and save keys.

- Put the subcommand before its flags: `ego acp --profile NAME`, not
  `ego --profile NAME acp`. A command name parsed as an unescaped root prompt
  after flags is refused with usage exit 2; it is never sent to the model.
  Use `ego --plain -- acp` to submit the literal prompt `acp`. Command names
  used as option values remain values, for example `ego --system acp "hello"`.
- `ego run` is the scripting workhorse. `ego resume <id> "next step"` chains
  scripted multi-turn conversations — a pipeline can carry a session through
  several stages. One session pool across interactive and headless runs.
  For a bounded stdin-to-stdout call without tools or hooks, see
  [one-shot text transformers](docs/integrations/one-shot-transformers.md).
- For an installation-free IDE custom-agent setup over native ACP, see
  [IDE setup](docs/integrations/ide.md); no `--ide` auto-connect flag is required.
- For standard ACP client libraries and runnable isolated examples, see
  [ACP client libraries](docs/integrations/acp-clients.md).
- There is deliberately **no bespoke RPC mode**: embedders get `ego acp`.
  Other agents invented bespoke `rpc`/`json` modes; ACP is that, standardised.
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
- Stdin contract (the cleanest of those surveyed): prompt as argument; `-`
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
- `ego resume ID --fork PROMPT` (also `-f FILE` or `-` for stdin) runs
  headless in a new fork without writing to the source. If the source is busy
  or unsettled, the fork ends at its last successful completed turn. A busy
  source with no such turn cannot be forked. `--output json` returns the fork
  ID, result and usage, including cache reads reported by the provider.
- `ego resume ID --queue PROMPT` (also `-f FILE` or `-` for stdin) queues a
  prompt for a saved session that another process may own. It waits while an
  interactive session, another resume, an ACP connection or an earlier queued
  prompt holds the session, then runs the prompt headless through the same
  parse, configuration, trust, permission, sandbox and budget admission as
  `ego resume ID PROMPT`. Queued prompts on one session run one turn at a
  time; their order is not guaranteed. The exit status is that turn's status
  (§4.3). The wait counts toward `--timeout` (exit 124) and ends on
  SIGINT/SIGTERM (130/143). `--queue` requires a prompt and is refused with
  `--answer` and with profile `ci`. Without an ID it pins the latest session
  of the workspace before it waits.
- Metadata-only session operations reuse `resume` and never start a model turn:
  `ego resume ID --name show|set=TEXT|clear [--output text|json|jsonl]` and
  `ego resume ID --goal-operation show|set=TEXT|achieve|resume|block=REASON` with optional
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

When stdin is redirected, EOF preserves the last turn's exit status, including
provider or response-stream failure (4), invalid provider input (2), and context
overflow (4). A successful turn exits 0. On a human terminal, closing the session
after a provider or model refusal remains a normal session close (0). Use
`ego run -` for a one-shot stdin prompt with headless retry defaults.

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
| `ego resume` | **yes** | continues a session that already exists, never creating one. `[ID]` is optional: without it a human terminal shows this workspace's session picker in pages of ten; Page Down loads the next page. Resume ordering uses event-log last-write time, with descending session-ID ties. Headless execution takes the first readable session in that order. With terminal stdin and stdout, a positional prompt is submitted once to the interactive driver, which accepts further messages. Explicit `--output` (including `text`), stdin `-`, `--answer`, an agent file, or profile `ci` selects headless execution. Redirected stdin or stdout also selects headless execution |
| `ego ls` | **yes** | this workspace by default, `--all` for the whole store; `--output json\|jsonl` per §4.6 |
| `ego fork` | **yes** | creates a durable lineage branch at the idle tip or after the completed turn containing `--at ITEM_ID`, without copying source spend or run state |
| `ego rewind` | **yes** | restores the files that the session's builtin writes changed after the completed turn containing `--at ITEM_ID`, newest first, then forks the conversation after that turn; refuses before any change when a file changed outside ego (§6.7) |
| `ego compact` | **yes** | publishes one idempotent clean target from the idle compaction operation and supports later continuation; `--request-id` optionally reuses an existing ID, otherwise the parser generates one |
| `ego export` | **yes** | local transcript export plus the producer-only version-1 structural bundle under `--sanitize`; Markdown, JSON, JSONL, stdout, and atomic file publication follow §2.1 |
| `ego mcp` | **yes** | `add`, `rm`, `ls [--json]`, and `reload [NAME]` use one live admitted registry and truthful health projection. `login NAME --client-id ID`, `login NAME --cimd URL`, or `login NAME --dcr` selects an explicit public-client registration strategy, prints a copyable authorization URL, and waits up to 120 seconds on an exact loopback callback; `--callback-port PORT` selects a registered callback port |
| `ego mcp-server` | **yes** | serves scoped `run`/`continue`/`cancel`/`status`/`compact` tools and sanitized summary/output resources on stdio. `--max-concurrent-runs` (default 1), `--retained-requests` (default 16), and `--max-resource-bytes` (default 65536) are optional non-zero deployment bounds; `-C`/the current directory is always an allowed root, and the repeatable `--allow-root`, `--allow-model` and `--allow-mode` add reviewed alternatives; stdout remains protocol-only |
| `ego auth` | **yes** | `login\|logout\|status [PROVIDER]`; credentials live in one locked `$EGO_HOME/auth.json`, while TOML commands and discovered auth plugins can supply login tokens. Interactive `/login [PROVIDER]` handles built-in OAuth only in the running session; a successful login is usable by its next model turn |
| `ego models` | **yes** | prints the shared current-model catalogue as deterministic text or versioned JSON; `--provider` filters both model and source rows, and `--refresh` updates authoritative discovered slices without promoting cached display facts into generation authority |
| `ego hooks` | **yes** | `ls [--json]` reads the same immutable admitted hook catalogue used by execution; it neither reloads nor edits hook policy |
| `ego config` | **yes** | `get`, `set`, `unset`, `ls`, and whole-document `edit` use one strict atomic user-config transaction; stored profiles are explicit targets and management ignores ambient `EGO_PROFILE` |
| `ego rules` | **yes** | `ls`, `allow`, `deny`, `revoke`, `remember`, and `explain` share the canonical rule grammar, atomic user store, and effective policy service; management ignores ambient `EGO_PROFILE` and never executes the explained call |
| `ego usage` | **yes** | reports the versioned main/judge/compaction/total accounting DTO for this workspace; `--session ID` reads only that session's metadata and reports its current main model without enumerating the workspace; `--period` accepts `all` or a positive `s`/`m`/`h` duration and `--json` emits the same DTO. The text view is a compact `session model role in out cached cache_hit cost` table that hides role rows with no activity unless `--all-roles` is given; the flag is presentation only and never reaches the JSON report. A generation whose price was unknown when it ran keeps its recorded `unknown` cost; the report adds a separate read-time estimate from its recorded token usage (`estimated` in JSON, `~X estimated` in text), never persisted and never counted toward a budget. The estimate uses the newest cached price table that arrived before the request (`request-time table`); with none it uses the current table and says `current-table estimate`; when neither prices the model, `estimate_unavailable` names the reason. A request above a model's long-context threshold is priced at that tier's rate |
| `ego acp` | **yes** | the Agent Client Protocol on stdio, v1. A host's `session/new` mints an ordinary ego session and its `session/prompt` runs an ordinary turn, so what it opens is listed by `ego ls` and continued by `ego resume`. v1 and not the v2 draft: see `docs/internals/decisions/2026-08-21-the-version-an-agent-answers-with.md` §6.2 |
| `ego doctor` | **yes** | one ordered, versioned, redacted `HealthSnapshot` over nine sections, rendered as text or `--json`. It reads existing facts and makes no network call by default. With `--probe-network`, or `doctor_network = true` in the user configuration (a row in `ego setup`; a project or local file that sets it is ignored), it makes for each configured provider (a stored credential, or the key variable its row reads) one unauthenticated reachability probe of the provider endpoint: DNS, TCP, TLS, then a bare `GET` with no credential, proxy or redirect, each layer bounded at 2 seconds and the whole probe at 5. A failure names its layer. `--offline`, or `network="off"`, skips every probe and wins over the opt-in. `/doctor` probes only when `doctor_network` is true. It never logs in, refreshes a credential or a catalogue, reloads an extension, edits config, or makes an authenticated or paid provider call. `/doctor` renders the same snapshot, and a fact ego cannot observe without mutating something is reported `not_checked` with its reason rather than omitted |

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
| `web_private_network` | yes | no | no |
| `budget` | yes | yes | yes |
| `bare` | yes | no | no |
| `permissions.judge` | yes | yes | yes |
| `ui.statusline.template` | yes | no | no |
| `ui.keybindings` | yes | no | no |
| `ui.vim` | yes | no | no |
| `compaction.idle` | yes | no | no |
| `compaction.idle_margin_seconds` | yes | no | no |
| `compaction.idle_threshold` | yes | no | no |
| `compaction.headless` | yes | no | no |
| `doctor_network` | yes | no | no |
| `recap.enabled` | yes | no | no |
| `recap.after_minutes` | yes | no | no |
| `mcp_tasks` | yes | no | no |
| `memory` | yes | no | no |
| `checkpoints` | yes | no | no |
| `tools` | yes | no | no |
| `sessions.compress` | yes | no | no |
| `cache` | yes | no | no |
| `instructions.max_files` | yes | no | no |
| `lsp` | yes | no | no |
| `build_recipes` | yes | no | no |
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
A stdio MCP server that this run attaches (an enabled configured server, or one
selected with `--mcp NAME|@FILE`) adds its own exact canonical executable to the
exec perimeter, because you chose that server and the sandbox must not refuse to
start it (`with_selected_mcp_executables`). You need not add its path to
`exec_paths`. If a read-only root denies execution, startup names that root:
change or remove its read-only restriction, then exit and resume. Adding `exec_paths` cannot override a read-only root, including under
adaptive confinement. MCP diagnostics do not expose child stderr,
arguments, or environment values.
On macOS an installed `/opt/homebrew` is also a default runtime root, including
its `bin` symlinks, Cellar targets and libraries. Installed Apple developer tools
at `/Applications/Xcode.app` and
`/Library/Developer/CommandLineTools` are also runtime roots. Workspace confinement
permits reads of the exact `/private/var/select/developer_dir` selector and `/Library/Preferences/com.apple.dt.Xcode.plist` license record so Apple
launchers can locate those tools and check the accepted license. It does not open
the surrounding `/var` or preferences directories.
Other installation prefixes must be listed explicitly. Git can also read existing `~/.gitconfig` and
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
    --account N         select a stable account number within the model provider
-m, --model P/M         provider/model slug: anthropic/claude-opus-5,
                        openrouter/z-ai/glm-4.7, ollama/qwen3 …
                        ollama is catalogued live from the local daemon;
                        other local servers run as a loopback
                        `openai-compatible` provider instance
    --offline           no non-loopback egress from any owned exit:
                        loopback addresses and `localhost` only, no DNS, no
                        proxy, and every redirect hop re-checked. Provider HTTP,
                        MCP, hooks, and spawned commands use their guarded
                        transport or demonstrated no-IP confinement. It cannot
                        stop an admitted local daemon from relaying upstream
                        itself
    --thinking L        off|minimal|low|medium|high|xhigh|max
    --system TEXT|@file          replace system prompt
    --append-system TEXT|@file   append to it
    --output-style NAME         select a trusted prompt-only response style
```

**Permissions and confinement** (one axis pair)

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
`docs/internals/decisions/2026-08-21-a-rule-that-means-exactly-one-thing.md`, which
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
incident (story 224-0e9d), the launcher leaked lock descriptors 8 and 9 without
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
    --no-session-compression  skip the background compression of old sessions
                        for this run; `sessions.compress = false` turns it off
    --artifact-dir DIR  override the sanitized artifact root; valid only for
                        the explicit `ci` profile
    --answer ID=JSON    repeatable; answer a pending AskQuestion while resuming
    --title TITLE       resume/continue only; the session of this workspace with that exact title
    --name OP           resume/continue only; show|set=TEXT|clear, metadata-only
    --goal-operation OP resume/continue only; show|set=TEXT|achieve|resume|block=REASON,
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

With nonempty `TUIC_SESSION` and an executable `tuic` on PATH, launch uses TUIC `repo.worktree_create` and its receipt,
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
    --budget B          spend cap: '2.5$' or '400k' tokens → exit 5 after exceeded
    --max-turns N       cap agent turns
    --timeout DUR       wall clock: '90s', '10m' → exit 124
    --retry N           provider retries, delivery-certainty-aware: a
                        transient failure that was unsent or showed no
                        output retries, with backoff (headless default 3;
                        interactive and ACP recover for 24 hours)
    --fallback P/M      ordered model degradation chain (repeatable)
-f, --file FILE         strict version-1 agent file with invocation settings (§5)
    --var K=V           template variable for agent files (repeatable)
-C DIR                  run as if started in DIR (git-style)
-c, --set K=V           dotted-path config override, value parsed as TOML
                        (for example: -c 'sandbox.mode="workspace"')
    --profile NAME      layer [profile.NAME] from config (§7)
    --bare              no ambient discovery even in a trusted workspace
    --trust-workspace   explicitly trust this launch's workspace, without asking
-q / --no-color / --plain
```

**Native web tools**

`web_fetch` retrieves public HTTP(S) UTF-8 HTML, text, Markdown, or JSON through
the same network permission rules as other connections. It sends no credentials,
cookies, or proxy authorization. It rejects private and special-use resolved
addresses before connect. Each call has a 30-second deadline and an independent
8 MiB response limit. Rendered content defaults to 32 KiB; `max_output_kib`
accepts 1 to 256 and changes only the rendered-content limit.
Results identify their source and
mark web content as untrusted. Redirects return a destination for a new
`web_fetch` call; they do not transfer the original network permission.

Native `web_search` is enabled by default on the official OpenAI Responses
(API key and subscription), Anthropic Messages, and xAI Responses endpoints.
Ego does not filter model names. Compatible custom gateways are opt-in.
OpenAI subscription defaults use cached search; explicit selection retains live
access. Anthropic and xAI native search use live access. An offer does not prove
provider acceptance or account entitlement. Use `--tools web_search` for explicit
native selection without `EXA_API_KEY`; an unsupported endpoint can reject it.
`--no-tools` disables tools.
Each generation reevaluates native capability without changing the selected recipient. Hosted search uses the model
provider's credential and shares the authority to use that provider. No separate
search-consent prompt is required. Pricing depends on the route; do not infer
that every API route is free. Bounds depend on the provider route; subscription
routes do not establish an
individual-search bound. A saved connection does not prove model support or
entitlement, and wire support does not prove custom-endpoint conformance.
Provider rejection is reported without changing account or search backend.

Default composition never selects the independent Exa backend, including on
non-native routes with `EXA_API_KEY` configured. Explicit `--tools web_search`
is native-only; unsupported native search is omitted, not replaced by Exa.
Select `--tools web_search_external` for independent Exa search. Each external
call requires a nonempty `EXA_API_KEY`, `allow_paid: true` and normal network
permission. Results contain at most five cited URLs, titles, and highlights;
search never forwards its key through a redirect. Model changes, retries,
children and resume preserve each selector, without an account or recipient
fallback. The admitted tool list is retained with durable session selections;
a new explicit `--tools` or `--no-tools` on resume replaces it. This also pins
a config-derived tool list over later config edits. Selecting another session in
process restores that target's tools through normal admission; the source
invocation's `--tools` does not override the picked target. Same-workspace
switches retain the active model and thinking selection. An unavailable
independent-search backend is an error, never a recipient fallback.
Existing configurations that used `web_search` for Exa must select
`web_search_external`; there is no compatibility alias. Both tools may be
selected explicitly, but remain distinct operations. The local per-call
consent and policy workflow does not run for provider-executed search.
Explicit search exclusion, offline mode, and existing applicable denies remain
authoritative. Subscription defaults set `external_web_access` to `false`
(cached access), matching the documented default. Explicit selection retains
`true` (live access). Live provider acceptance and entitlement remain unverified.

**Attachments and tools**

```
    --attach IMAGE_PATH|HTTPS_IMAGE_URL
                        repeatable; exact image bytes admitted as a durable
                        content block before provider projection
    --tools LIST        restrict builtin tools;  --no-tools for pure generation
                        (the `tools` config key sets the same list for every run;
                        this flag overrides it)
    --mcp NAME|@cfg.json  select an effective server by name (even one marked
                          enabled = false) or attach a strict explicitly named
                          JSON MCP server for this invocation (repeatable);
                          enabled effective servers attach without it
    --mcp-only          attach only the --mcp selections; enabled effective
                        servers stay detached for this invocation
```

**The general session shape**

Seven of the flags above are not about *running a prompt*: they are about the
*session* a prompt runs in — what the model may use, what it is allowed to do,
and what it is told about where it is working.

```
    --tools LIST / --no-tools    select builtin tools
    --no-hooks                   run with every hook off, for this invocation
    --allow RULE / --deny RULE   what it is allowed to do
    --profile NAME               which stored configuration layer applies
    --append-system TEXT|@file   what it is told, appended to the prompt
    --output-style NAME         trusted prompt-only response style
    --instructions FILE          which instruction file, named not discovered
```

Every surface that opens a session takes them, from one declaration: `ego run`,
`ego resume`, and `ego acp`. They are declared once (`Shape` in
`crates/ego-cli/src/cli/commands.rs`) and converted once
(`SessionShape`), so builtin selection has the same meaning on each surface. MCP admission is separate.

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
`--no-tools` offers no builtin tools. On standalone `run` and `resume`, it also
disables MCP attachment. On ACP, it still permits MCP tools from servers admitted
for that session through the host. For an MCP-only session, launch
`ego acp --no-tools` and supply only the intended servers in `session/new`
`mcpServers`. With no supplied servers the catalogue is empty. With supplied
servers it contains only their admitted named tools, including admitted catalogue
refreshes; it does not acquire builtin tools from configuration. The host must
keep those catalogues within its intended scope. Calls to undeclared builtin names
are refused before effects. Host MCP calls can still require permission.
This contract confines tool offering and dispatch, not the process filesystem,
credentials, or recovery of an original call after restart. `--instructions` still names a file rather than
discovering one — a served session is untrusted (§4.4), and an untrusted
posture does no ambient discovery, so an explicit file is the only way to tell
a served session anything about its workspace.

`ego acp --exclusive-read-only` is the policy for an unattended session whose host sends it
untrusted text and needs a guarantee, not a convention. The launch line owns it, and no
configuration layer, profile or host request can widen it:

- no native tool (the empty tool list), no hook, no skill or custom command (none is offered to the
  host either), and no MCP server: a configured server is not attached and a `session/new` that
  carries one is refused;
- it conflicts with `--tools`, `--no-tools`, `--allow`, `--deny` and `--profile`, and `EGO_MODEL`
  and `EGO_PROFILE` in the launch environment are refused, so nothing outside the launch line
  selects the model;
- the process replaces itself at start with the same command line under a cleared environment.
  It keeps only platform values (`PATH`, `HOME`, `EGO_HOME`, `TMPDIR`, `TEMP`, `TMP`, `LANG`,
  `LC_*`, `TZ`, the proxy and CA variables, the Windows system variables), the credential
  variables of the provider of the configured model, and each `--env-allow NAME`. `GH_TOKEN`, an SSH
  agent socket, git credential variables and `TUIC_*` do not survive. Provider base-URL variables
  are dropped too; name one with `--env-allow` when a local daemon needs it. A session that later
  switches provider has no key for it and fails closed.

Final retry semantics are owned above provider mappings. `--retry N` permits
at most N additional attempts on the same resolved model after a `Transient`
failure that emitted no content. An attempt with only opaque reasoning state
and parsed tools proven unrun may retry once per candidate generation, within
that same allowance. Pending tool items close as not run before the retry.
On interactive and ACP seats, visible narration without parsed tools, visible
reasoning or unknown-effect provider content may also retry once per candidate
generation. Opaque reasoning state does not disqualify narration. The partial
text is marked `[interrupted]` in the transcript and is not appended to the retry
request. Headless text, visible reasoning and provider content of unknown effect
still block retry. Explicit `--retry 0` also forbids both recovery exceptions.
A provider capacity refusal is an HTTP 429, 503 or 529 status, or a typed
overload or rate-limit error in the stream. It retries the same model after 1, 2 and 5 minutes, without
jitter, and every later retry waits 5 minutes. A provider `Retry-After` longer
than the current step replaces it. A provider wait above 5 minutes stops a
finite plan rather than retrying early.
Any other transient failure, such as another 5xx status, a failed connection or
a lost stream, uses
exponential backoff of 1, 2, 4 … seconds (at most 120),
with up to 25% downward jitter, or the provider's `Retry-After` without jitter.
`Retry-After` accepts delta seconds and HTTP-date; `retry-after-ms` takes precedence.
A provider wait above 120 seconds stops such a finite plan rather than retrying early.
A retry never changes the model; only an explicit `--fallback` can.
Without an explicit `--retry`, interactive and ACP seats use a 24-hour monotonic
recovery horizon. It starts at the first eligible failure and does not reset
when an operator forces a retry. Provider waits can use the remaining horizon.
Headless runs default to three additional attempts for transient failures,
including provider capacity refusals (at most four requests per candidate).
A capacity refusal is reported only after the third retry also fails.
This is a finite allowance, without the interactive recovery horizon. Explicit
`--retry N`, including zero, replaces that allowance. `--retry 0` makes one
attempt, so a script can own its retry policy. A streamed refusal preserves
`Retry-After` and `retry-after-ms` from its HTTP response just as a non-200
refusal does. Repeated failure frames for one xAI response produce one error;
process completion does not repeat an already printed terminal diagnostic.

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
unknown-effect output stops automatic retry, and any possibly-sent attempt stops fallback. Every attempt has a fresh ID, admission, request digest, settlement,
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
| `--queue` | **yes** | `ego resume` only. Waits until no other process owns the session, then runs the prompt headless; see §2 |
| `--accept-authority-delta` | **yes** | `ego resume`/`ego continue` only. Allows the execution-authority change measured and shown for this resume without a question, also headless; the decision is journaled as a party decision and the measurement becomes the saved authority. Covers that one resume: a later, different change asks again. It cannot widen past administrator ceilings, which are applied while the configuration loads, before the measurement. Without it a headless resume of widened authority exits 3 and starts no provider call. Recovery: rerun with the option, or restore the previous configuration |
| `--title` | **yes** | `ego resume`/`ego continue` only. Selects the session of this workspace whose title (set with `--name set=TEXT` or `/name`) is exactly TITLE, case-sensitive; every positional is then the prompt. No match, or a title combined with an id, is a usage error (exit 2). When several sessions have the title, the error lists them, most recent first, and no session is resumed |
| `--name`, `--goal-operation` | **yes** | metadata-only `ego resume ID` / `ego continue ID` operations from §2; they cannot be combined with a prompt, answer, or run-only override |
| `--output` | **yes** | `text\|json\|jsonl`. On `ego ls` the two machine forms are §4.6's, not §4.1/§4.2's — a listing is not a run |
| `--all` | **yes** | `ego ls` only, and documented in §2 rather than here: §3 is the argument grammar of a *run*, and this widens a catalogue query |
| `--schema`, `--schema-retries` | **yes** | `--schema-retries` ships as its own flag; §3 mentions it only inside `--schema`'s description |
| `--budget` | **yes** | binds only on a currency-metered row — the run says so on `metering.selected` |
| `--max-turns`, `--timeout` | **yes** | |
| `--tools`, `--no-tools` | **yes** | part of §3's general session shape, so `ego acp` takes them too. `--no-tools` removes builtins; ACP host-admitted MCP tools remain available |
| `--no-hooks` | **yes** | part of §3's general session shape, so `ego acp` takes it too. No configured or skill hook is admitted or run, in the session and in its task children; `session.started` lists `hooks_off` in `capabilities`, and the configuration is not read differently or written. `ego hooks ls` is unaffected: it reads the configuration, not a session |
| `-C`, `-c/--set`, `--profile` | **yes** | `--profile` is part of §3's general session shape, so `ego acp` takes it too. `-C` it already shared; `-c/--set` it does not take |
| `-q`, `--no-color` | **yes** | `--no-color` was marked **yes** here on 2026-08-21 while being parsed and read by nothing — the same silently vacuous flag §4.4 refuses elsewhere. It decides for real from 2026-08-22: it is the highest-precedence input to the one capability gate (`crates/ego-cli/src/render/caps.rs`), above `NO_COLOR`, above `TERM`/`COLORTERM`, above `isatty`. See `docs/internals/decisions/2026-08-22-two-axes-and-a-terminal-that-answers.md` §1 |
| `--mode`, `--plan`, `--yolo`, `--allow`, `--deny` | **yes** | `--allow`/`--deny` are part of §3's general session shape, so `ego acp` takes them too; `--mode` and its two spellings are not part of it. `--plan`/`--yolo` are spellings of `--mode`; two at once is a usage error. All five modes enforce. `auto` resolves a separate configured judge and routes its closed verdict before any human escalation. All seven rule heads enforce: `read`, `write`, `list`, `search`, `exec`, `net`, and `tool` — `net` matches `Op::Connect`/`Op::ProtocolConnect` and `tool` matches `Op::Opaque`, both of which this build now produces. This sentence is derived from `parse.rs`'s `OPERATIONS` table by `the_documents_name_the_rule_heads_the_parser_actually_enforces`, not maintained beside it. The seven builtins are read, glob, grep, write, edit, exec, and read_tool_result. `edit` prepares both `Read` and `Write`; a matching `write(pattern)` allow covers its same-file replacement read without a separate `read(pattern)` allow. The glob still limits the target; explicit read/write denies, workspace boundaries, and plan mode still apply. A standalone read still needs read authorization; `read_tool_result` uses its session-evidence capability rather than a user-authored rule head. |
| `--sandbox` | **yes** | `ro\|workspace\|adaptive\|off`; fresh-config default `adaptive` on macOS and `workspace` elsewhere, independent of permission mode; effective configuration can override it. The measured platform backend enforces the selected level before a prepared process can launch; an unavailable required capability fails closed |
| `-f`, `--file`, `--var` | **yes** | strict version-1 agent files load through the existing bounded YAML implementation, apply their configuration layer below flags, and substitute declared body variables. M4 extracts the shared envelope for skills and still owns the direct executable shebang end-to-end proof, not these flags' availability |
| `--attach` | **yes** | repeatable local-path or HTTPS image input; bytes enter the durable session-blob/content path before model-specific projection and unsupported model/profile combinations refuse before acquisition |
| `--mcp` | **yes** | repeatable ordered selection of effective `NAME` entries and strict invocation-scoped `@FILE` entries; the selected live admission is shared by dispatch, `/tools`, `/doctor`, and deterministic `mcp_errors`. `--mcp-only` attaches only these selections and keeps every enabled configured server detached |
| `--retry`, `--fallback` | **yes** | The shared turn coordinator owns delivery-safe retries and ordered, capability-checked fallback attempts; explicit retry limits and default interactive/ACP recovery follow §3 above |
| `-v`, `--verbose` | excluded | the binary refuses this legacy placeholder by name, and the exclusion Story 082 recorded stands. What it once waited on now exists and is not a flag: `EGO_LOG` (an `EnvFilter` string) installs a subscriber for the `tracing` events this build emits and `EGO_LOG_FILE` collects them to a file, because a diagnostic level is not an argument of a run — see §4.8. Story 093 was expected to retire the spelling and did not; it stays in the unavailable table rather than the retired one, because retiring a flag requires naming its replacement and there is deliberately no replacement flag to name |

The model can also call `task` with a printable `name` and a `prompt` to run a depth-one, in-process child. The child inherits the parent mode, sandbox, offline posture and workspace ceilings, shares its budget and cancellation scope, and has no `task` tool. Task-only provider batches run concurrently. A batch that mixes `task` with other tool calls is refused with failed tool feedback for every call; no child or sibling tool runs. Child tool rows are nested under their names; permission prompts identify the child through the existing human seat. Only the final answer, bounded to the existing 64 KiB tool-result limit, enters the parent context. Child usage is booked once to the parent; child conversation state is held in memory and is not resumable.

Set `task.fork` to `true` to start a foreground child from the current conversation
instead of an empty one, so `prompt` only needs the directive. The child sends the same
tool catalogue, system messages and history as the parent's last request, then the
directive, under the parent's lineage root, so a provider with prompt caching can read
the inherited prefix from cache when model and provider permit it. Optional
`model` and `effort` override each inherited field without changing the parent.
An explicit model pin disables fallback. The target must accept the inherited
context and effort before submission; cache reuse across models is not guaranteed.
Without overrides, the prefix is identical at content-block level: after a
tool result, the directive joins that result's user message. A forked child runs exactly
one turn without goal checks, keeps the `task` descriptor but cannot delegate (a `task`
call fails with tool feedback), and returns only its own final answer. Its conversation
stays in memory, and its usage is booked to the parent as `subagent` generations, as for
a clean child. Cost: when the parent's cache entry has expired, or the provider has no
prompt cache, the child pays the whole parent context as input again, and a clean child
with a short briefing can cost less. Limits: hook context, or host context that changed
since the parent request, adds a message after the directive; skill instructions active
in the parent turn are not active in the child, so the system block, and with it the
cache prefix, differs; `fork` with `background` or `agent` is refused.

Set `task.background` to `true` to launch a durable standalone child without waiting.
The tool returns `background agent ID`. The child has its own journal and run lineage,
shares the parent budget and authority ceilings, cannot delegate, and emits no child
tool rows in the parent. From a session with a saved account, an explicit model on
another provider uses that route's unique saved account. If the account is absent
or the route has multiple saved accounts, the launch is refused. This does not
change the parent's selected account or the child's effect limits.
Parent-turn interruption (including Ctrl+Enter) does not
cancel this child. `/tasks` lists compact exec and agent entries. `/tasks details` shows full identities and output. Agent entries have
stable parent-session aliases `t1`, `t2`, and so on, in task admission order; a
completed task keeps its alias. Full IDs remain accepted. `/tasks t1` (or its full
ID) attaches the terminal input to that live child's conversation and shows its
output. Text goes only to that child through its existing turn coordinator; the
parent continues running with its own input queue. `/detach` returns input to the
parent without cancelling or collecting the child. `/tasks cancel t1` requests
cancellation independently. Completed tasks can be inspected but are not restarted
by selection. Exec IDs retain their output-inspection behavior.

Attachment accepts direct text, not parent shell commands, images, skills or
configuration changes. Use `/detach` before those controls. The parent and child
keep separate saved composer drafts. As with other state-changing terminal
controls, answer an open permission or question modal before attaching or
detaching. Attached child output is marked with its alias; parent execution and
its output continue. If the child settles while attached, further text is refused
rather than silently submitted to the parent; use `/detach` to return.

A normal child starts with fresh conversation state: it does not see the parent's
messages. `fork: true` is the foreground-only exception described above. Fresh
context does not mean a tool-less process: child tools clone the parent's effect
ports and working directory, and retain its admitted tool ceiling, permission,
sandbox and network limits. `exec` can run shell commands only when that tool is
admitted and the effect passes those limits. A child gets no extra shell authority
from being a child, from `fork`, or from attachment. Background children start a
new durable session under the same authority ceiling and have independent
cancellation. Source: `turn/task.rs`, `turn/tools.rs`, `subagent.rs`, and the CLI
`run/port_adapters.rs`.
A completed child enters parent context once at the next generation boundary, not
in the middle of another generation. A successful answer of at most 8,192 bytes
(after secret masking) is delivered inline. A larger answer is not: the parent
receives a short receipt with the answer length and an evidence reference. The
masked answer is stored as immutable evidence of the parent session before the
receipt is recorded, and the child's own copied result is not changed. Read it with
`read_tool_result` (32 KiB pages, original-byte offsets); another session cannot
resolve the reference. If the evidence cannot be stored, nothing is delivered and
the failure is reported. Failure outcomes stay inline. There is no search over
a receipt yet; page through it. In the interactive REPL, settlement wakes an
idle parent through the same coalesced seat notification as background exec. The
parent starts a checkpoint turn only if an undelivered task or process outcome is
ready. Duplicate or stale notifications do not start another generation. Headless
and ACP hosts do not attach this interactive wake callback: they retain their
existing generation-boundary collection and session-end waiting behavior.
Recovery uses the durable task ID, original
parent run and captured tool ceiling. An uncertain interrupted child is not rerun.
Closing a session waits for its admitted children to settle. A new background launch
refuses if live permission policy differs from the admitted session epoch; start a
new session to inherit that changed authority. Background tasks do
not accept `agent` or `worklist_step`; use an explicit prompt instead.
Task guidance is the same in every host. External coordination uses the host API,
not a private ego coordinator bridge.
Live task conversion with Ctrl+B and child transcript navigation are not included.

`task` also accepts `agent`, the filename stem of a named definition. Ego discovers
`*.md` files in the trusted workspace's `.ego/agents`, then `$EGO_HOME/agents`
(`~/.ego/agents` by default), then `/etc/ego/agents`. An untrusted workspace does
not contribute project definitions. The first definition of a name wins; files
are read-only inputs, not installed commands. The task descriptor lists available
names in both terminal and ACP runs. Symlinked definitions and catalogue roots
are refused.

Named definitions use the versioned agent-file envelope and prompt body. Optional
`model` selects the child's model without changing the parent. Optional
`effort` (`off|minimal|low|medium|high|xhigh|max`) selects the child's reasoning
effort the same way. The `task.model` and `task.effort` config keys set a default for every
foreground `task` child. Precedence, highest first: explicit task-call `model`/`effort`, the named
definition, the `task.*` default, the actual parent value. Each field resolves
on its own. Call `model_catalogue` to page canonical model IDs and known supported
efforts without implicit network, authentication or cache writes. Unknown facts
stay unknown. A fork uses only explicit call fields and the actual parent profile. Background tasks use explicit call fields, `task.*` defaults, then the actual
parent profile. Their admitted model and effort remain pinned through recovery
and attachment. Forked skills do not read these defaults: a skill's own `model`
and `effort` apply to its fork. Optional `tools`
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

The `worklist` tool owns one durable ordered list of actions, separate from planning
mode and the checked goal. Call
`{"operation":"replace","steps":[{"id":"inspect","text":"Inspect the source","status":"pending","reason":null}],"id":null,"status":null,"reason":null}`
to replace it, or `{"operation":"step","steps":null,"id":"inspect","status":"in_progress","reason":null}`
to update an existing ID. All five fields are required: inactive fields must be
explicitly `null`, while active fields retain strict types and validation.
Unexpected fields and non-null inactive fields are rejected without changing
the worklist or goal. `pending` and `in_progress` mean independently actionable
work already authorized by the user. Use `blocked` for missing input or authority,
`waiting` for an external or background event, `cancelled` for withdrawn work, and
`completed` for finished work. A status never grants effect authority.
Keep IDs stable when reordering or replacing actions. IDs are unique printable
strings of 1–80 bytes; action text must not be empty. The list is limited to 128
actions and 16 KiB of combined IDs and text. Reasons have a separate bounded
allowance of at most 512 bytes per row so failure settlement cannot exhaust the
action-text budget. At most one direct action
may be `in_progress`; extra active actions require actual live foreground task
bindings. A prepared tool call, invented execution ID or ended task does not count.
A started ID can never return to `pending`, including through replacement or
removal/re-addition. Complete or cancel unfinished started work before removing
it. An empty replacement clears only work that can be removed under these rules.
`waiting` and `blocked` require a nonempty printable `reason` of at most 512 bytes;
other statuses require `reason:null`. Replacement carries each action's reason
and has `reason:null` at the top level. Unchanged recorded rows remain readable;
new updates cannot introduce additional unbound active actions.
Updates remain journal events (`plan_updated` in JSONL), survive replay, fork,
and compaction, and appear as terminal worklist rows and ACP plan replacements.
The ACP protocol retains its plan vocabulary and exposes exact action status in
metadata. Worklist completion does not achieve a goal.

At a natural model stop, unfinished actionable work can start one ordinary
continuation under the existing turn, token, permission and input boundaries.
Blocked, waiting, cancelled and completed actions do not request continuation.
Independent actionable work can proceed while another action waits. A new
completion is progress; tool calls, inspections, prose edits and status toggles
alone are not. Without progress, another continuation waits for new user input or
an actual background completion. Replay and compaction preserve that boundary.
An explicit user cancellation cancels pending and in-progress actions. Queued
user input takes priority over automatic work. Background completion is an event,
not permission for repeated empty turns. When a goal and worklist coexist,
one natural-stop decision selects at most one continuation: useful checked goal
work first, then independently actionable worklist work, then a required managed
report. An active goal's wait, no-progress pause, failed check or exhausted
allowance prevents fallback work. A blocked goal can still leave independent
authorized actions ready. Queued input and cancellation win before any candidate
opens a new turn. Worklist completion alone is never checked goal achievement.

When an update leaves every action `completed`, its tool result requests one short
recap: what each action did, what was verified, and what stays open.
An optional `task.worklist_step` binds a foreground child to an existing action.
The parent records `in_progress` when the child future starts executing, then
`completed` on success or `blocked` with a cause on failure or interruption. The
host owns that row until settlement; model updates cannot overwrite or remove it.
Unknown or duplicate IDs are refused before the child runs. Reopening a session
retires interrupted task calls rather than treating them as still running.
Background tasks do not accept a binding. Child worklists stay local to the child.

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
  "v": 21,
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
`{"status":"achieved|stopped|exhausted|waiting|active","reason":"…"}`.
The field is absent for sessions without a goal. `achieved` means the durable
goal was achieved; it has no `reason`. `stopped` carries the goal-check stop
reason (including bounded evaluator failure diagnostics), or the blocker reason.
`exhausted` carries the goal continuation or goal-check token budget reason.
`waiting` means the goal remains active while automatic work waits for new input
or completion evidence; its reason identifies the dependency or idle fallback.
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
context overflow remains a separate typed failure. With `compaction.headless = true`, a headless run compacts and
continues on either overflow (see "Bounded reads and automatic context
recovery"); this summary then reports the successor `session`. Without it, and when
compaction was not possible or failed, the run ends with exit 4.
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
session stopped because a host called `_ego/pause` (§ `docs/internals/06-acp.md`), and
the only thing that moves it again is that host calling `_ego/resume`. It is
exit 6 and deliberately not exit 3 for exactly that reason: a script that reads
3 goes looking for a `denied`/`denied_by` pair or a `question` object, and a
pause has neither.

<!-- ego-cli-json-output:end -->
### 4.2 `--output jsonl` (one event per line, live)

The events are the **`PublicEvent` projection** of `ego-core`'s semantic model
(`ARCHITECTURE.md` §1) — privacy-filtered, versioned, with its own
compatibility policy; internal events never leak into it by default. They are
enveloped in a three-level shape — `session.started` /
`turn.started|completed|failed` / `item.started|delta|completed` with a
`status` lifecycle per item (item kinds: `user_message`, `agent_message`,
`reasoning`, `tool`, `permission`, `question`, `subagent`) — the best-factored schema
surveyed
(`docs/internals/07-cli-prior-art.md` §4). Rules learned from the field's mistakes:

- `session.started` carries a **`capabilities` string array** (feature
  detection by name, never version parsing) and error arrays
  (`mcp_errors`, `hook_errors`) **present only when non-empty** — a CI gate
  is a presence check.
- The **final result rides the terminal event** (`turn.completed` of the last
  turn) — no `--output-last-message` temp-file dance.
- **One error field.** `ok: bool` plus `exit_reason`; no second status field
  that can contradict it (for example `subtype:"success"` beside `is_error:true`).
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
6 paused at a host-requested boundary   7 local generation guard
124 timeout
130 SIGINT    143 SIGTERM
```

Windows ConPTY reports a double idle `Ctrl+C` exit with the native
`STATUS_CONTROL_C_EXIT` (`0xC000013A`) status rather than the POSIX-derived
`130`; both identify an operator interrupt.

Other agent CLIs document a short exit-code table or collapse
almost everything into 0/1. Scripting needs the distinctions (retry on 4,
escalate on 3, alert on 5, resume on 6). The shutdown contract is explicit (and
right): on SIGTERM abort the turn, kill the tool process tree, run
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
`docs/internals/decisions/2026-08-21-an-exit-code-that-names-who-refused.md`.

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
to read-only roots (stories 265-d371 and 616-1ec3). A user-decided denial, a
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

### Startup failure evidence

If a CLI launch takes its writer lease but fails before its first manifest or
journal is published, the session directory retains a private
`startup-<invocation-id>.json` diagnostic. It records the session, PID, startup
phase, outcome, message and exit code. Returned failures have outcome `failed`
and their actual exit code. A startup that has not returned an outcome is
`unresolved`, with no known exit code. This is diagnostic evidence, not a
completed session or a replay journal. A later successful launch does not erase
an earlier invocation's failure record.

Once the writer has opened, a pending record also identifies startup interrupted
by a process kill. Panic unwinding or a dropped startup future records an
unresolved outcome. The record is removed when this invocation publishes session
state or completes startup successfully. A forced kill inside lease acquisition
or recovery, before the writer has opened, cannot preserve this pending record.
If the filesystem refuses diagnostic publication, stderr names that secondary
failure; the original startup failure is unchanged. Ego adds no prompt or argv
fields. The files are readable only by the file owner on Unix.

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
Story 093 regenerates both artifacts only after all 18 commands and their
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
starts untrusted in silence, and the `trust` status segment says so. Instruction discovery starts at the repository root. The user can set
`[instructions] shared_root = "/absolute/shared"` to include shared ancestor
rules for repositories within that root. `exclude_roots = ["/absolute/shared/scratch"]`
keeps selected subtrees isolated. These absolute paths are user-only settings;
they grant no sandbox reach. `[instructions] max_files = N` (at least 1, default 8) sets
how many instruction files one run loads. A symlinked `AGENTS.md` is read from
its canonical target; a target outside the discovery scope needs the workspace
trust approval of that exact path and digest (see the configuration reference).
`AGENTS.md` is canonical; a relative `CLAUDE.md`
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
entries are hard errors (a strict-config mode, as the only behaviour).

`--instructions FILE` is the headless opt-in and is **not** ambient: it names
a file rather than discovering one, so it survives `Posture::Bare` and
*replaces* the discovered set rather than adding to it. Its bounded imports
are relative to `FILE` and cannot leave that file's directory. `--bare` together with
`--instructions` is a conflict for the applicability matrix above, not for the
loader — from a resolved posture alone, "the user demanded bare" and "the
workspace is untrusted" are the same value and want opposite answers. See
`docs/internals/decisions/2026-08-21-instructions-a-workspace-may-hand-a-model.md`.

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
`docs/internals/decisions/2026-08-22-two-axes-and-a-terminal-that-answers.md`.

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
leave one settled cause receipt when the turn recovers or fails. Each retry
shows its wait once; a capacity refusal also says `provider at capacity` and the
local clock time of the next attempt, for example
`At capacity — provider at capacity, retrying in 60.0s at 2:03:12 PM (attempt 1/3)`.
`-q` suppresses ordinary diagnostics, not recovery controls; `--output json` and `jsonl` keep
their machine-readable contracts.

**Out of band**, on a screen only: OSC 133 C/D around the run (so a host
segments the output into a command block instead of pattern-matching tool
headers), OSC 9;4 for the progress indicator — cleared on every exit path,
including failure — the window title, a desktop notification for a run that
took longer than a minute (suppressed by `-q`), and OSC 7770 `state`
under TUICommander, which is proven by `TUIC_SOCKET` and not by `TERM_PROGRAM`.
New user messages do not emit synthetic intent metadata. Assistant-declared
`intent: text (title)` markers own the contextual host title; automatic messages
must not replace it with the product name.
Native activity also emits bounded `agent-event` frames with exact session,
turn, item and child identities. Modal metadata comes from the terminal owner;
permission requests settled by policy emit no human wait.
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
`docs/internals/decisions/2026-09-07-diagnostics-get-a-destination-before-a-switch.md`.

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
`docs/internals/decisions/2026-08-29-agent-files-share-one-strict-yaml-envelope.md`.
No other agent CLI examined has this as a first-class, executable unit:
their prompt commands are REPL-only or not executable.

### 5.1 Skills are in-session capabilities, not agent files

A skill is a strict `<name>/SKILL.md`. Ego checks these roots in order:

1. Trusted project `.ego/skills`, `.agents/skills`, `.claude/skills`,
   `.codex/skills`, and `.github/skills` at each directory from the launch
   directory up to the nearest repository root. The nearest directory wins.
   Outside a repository, only the launch directory contributes project roots.
2. User `$EGO_HOME/skills` (default `~/.ego/skills`), `~/.agents/skills`,
   `~/.claude/skills`, `$CODEX_HOME/skills` and its `.system` subdirectory
   (default base `~/.codex`), then `$CLAUDE_CONFIG_DIR/skills` if set and distinct.
3. The managed `.claude/skills` directory, the managed `skills` directory,
   then `/etc/ego/skills`. On Linux the imported managed roots use
   `/etc/claude-code` and `/etc/codex` as their bases. On macOS the first base
   is `/Library/Application Support/ClaudeCode`. On Windows the bases are
   `C:\Program Files\ClaudeCode` and `C:\ProgramData\OpenAI\Codex`.

Project roots are not inspected for the skill catalogue without workspace trust.
A project root cannot link to a directory outside its project scope. Bare
headless runs do not discover ambient skills.
The first valid definition of a name wins. Canonical file aliases are read once;
fixed actions and the bundled `context-management` name remain reserved.
Slash completion and `/skills` label each skill by origin: project, user,
system, or built-in. The badge uses a muted color on true-color terminals and
keeps its text when color is off. `/skills details` shows the selected file path.
See the [skills guide](docs/integrations/skills.md) for supported frontmatter and extensions.

`/skills`, `/skills list`, and `/skills diagnose` inspect the one admitted
catalogue. `/skills diagnose` reports skipped project discovery, missing or empty
roots, load errors, and the path that wins when definitions share a name.
`/skills rescan` prepares and atomically publishes a complete
candidate; `/skills invoke <name> [arguments]` and `/<skill-name> [arguments]`
invoke a user-visible skill through the same typed operation. A model-visible
skill is
announced by bounded name/description/trigger metadata like an admitted MCP
capability. Its full body and rooted read-only resources enter context only
after invocation. A user-only skill is never announced to the model, and a
model-only skill is hidden from slash completion and refuses direct user
invocation. `/hooks` shows one compact row per admitted hook. `/hooks details` shows the full identity and provenance from the active immutable hook
catalogue without reloading or editing it.

Skill frontmatter may narrow tools, model, effort, paths, and invocation
visibility. It cannot relax system rules, trust, permission, sandbox, offline,
budget, judge, or egress. `context: fork`, `agent`, `background`, and `hooks`
use the final child-run and source-bound hook owners; `shell`, dynamic
`!command` context, and the runtime variables of other agents are refused permanently.
Invocation stores the rendered instruction and source digest durably so resume
and compaction never silently reread a changed file.

### Skill safety review

Before first use, ego sends bounded skill text and declared hooks to the active model in a separate request without tools, web search, or conversation history. It reviews text resources before delivery too. Ego reuses verdicts for unchanged content within the process, but never reuses an operator approval. An interactive operator can allow blocked content once; without an answer it stays blocked. Existing trust and permissions still apply. Use `--no-skill-safety-check` for one launch or `[skills] safety_review = false` in user config to skip the review. Project settings cannot disable it.

## 6. Interactive surface (complete, linear, keyboard-first)

The surface stays in the terminal's primary buffer. The terminal owns
transcript scrollback, selection, and copy; ego never enables alternate-screen
mode or mouse capture. One interactive owner appends rendered transcript
batches, then repaints only the ephemeral status, menu, and composer rows.
History, completion, multiline editing, bracketed paste, and concurrent output
use reedline. A smaller presentation may collapse detail by width, never remove
a capability.

Interactive startup shows the selected three-row εgo braille logo directly above
the application name and version, before configuration,
trust, provider, or MCP notices. It reports the selected model, effective posture,
and key hints after admission. Startup notices remain visible; `--quiet` keeps
its existing announcement policy. The logo requires interactive stdin and TTY stdout
and at least 17 observed terminal columns. It keeps the same glyphs without SGR
when colour is disabled (including `NO_COLOR`); narrow terminals keep the identity
line only. Headless and protocol modes do not show the logo. The name does not
require a resolved model.

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
/compact /pin /unpin /refresh /clear /usage /context /statusline /export /config /setup /doctor /quit /new
/login /name /goal /tasks /detach /side /tools /expand /skills /hooks /init /retry /version`. These commands and aliases form the
Boss-selected surface; a missing implementation remains required work rather
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

### Reviewed project initialization

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

`/tasks [tN | ID]`, `/config`, `/config perimeter`, and `/usage` answer
while a turn runs. They read the current session journal, admitted configuration,
and durable usage records without starting a parent generation. Task selection
changes only the terminal input target. `/tasks cancel tN` (or a full task ID)
remains a queued mutation while the parent turn is active; it targets only the
selected child when that turn finishes. Other state-changing commands
remain on their existing host-command path; an inspection does not cancel,
restart, or complete the active turn.

`/side QUESTION` asks a side question while a turn runs. The model answers from
the turn's newest request and its context. The request keeps the tool list,
including any provider-hosted web search, to preserve the cache prefix. The
provider can run hosted search if the turn already offered it. Ego never runs
local function tools for `/side`: a function call fails the question. The
question and the answer are a notice only. They do not enter the
session history, a later request, or the turn that runs. The answer is followed
by a usage line (uncached input, cached input, output tokens). `/usage` counts
the same usage with the main model. The question runs on the same budget and
admission gate as the turn. An idle session refuses `/side` and asks for a
normal prompt. A turn that has not yet sent its first request, or whose model
changed since that request, refuses it with a retry hint.

In the interactive terminal, a line beginning `!` runs its remaining text as a
local shell command through the same guarded exec admission as tools. It does
not request a model generation. The command and its output become user-run
context for the next turn. `!!` sends a literal leading `!` to the model;
multiline prompts are not shell escapes. A shell escape submitted during a
busy turn is refused and stays in the editor; submit it again after the turn
is idle. The terminal editor is the authority: text returned by a model,
tool, or replay does not run a command. Anyone who can control terminal input
can act as that user; this is not proof of physical keystrokes. Deny rules and
process confinement still apply. Headless and ACP prompts do not have this
editor escape.

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
uncovered writes ask and explicit allow rules are honored. Explicit denies and hard boundaries
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
`/name clear`, with bare `/name` equal to `show`; and `/goal show`, `/goal TEXT`
(or `/goal set TEXT`), `/goal achieve`, `/goal stop`, and `/goal block REASON`, with bare
`/goal` equal to `show`. Goal text activates an operator-owned goal and submits
TEXT as the first work turn after the goal is saved. Other actions use the same
metadata-only services as §2's `ego resume` spellings. A name never changes
session identity. One goal may be active; setting a new operator objective replaces the active goal with a fresh identity and counters while preserving the previous events. `achieved`, `blocked`, and `cancelled` are terminal and a later objective also starts a new goal.
`achieve` is the operator's explicit completion attestation. `block REASON`
records one blocker observation; only the fifth consecutive observation of the
same condition makes the goal terminal, and progress or a changed condition
resets that audit.

`/goal stop` durably cancels the current goal immediately, including while a main
reply or independent goal check is running. It does not attest achievement, change
the objective, close the session, cancel the current reply/tools/children, or
rewrite the worklist. It also pauses automatic work from the admitted request:
no goal, worklist or completion-report continuation starts, and the model cannot
create a goal from that request (the refusal says so). Worklist rows and history
stay. A new user request or an explicit operator goal enables automatic work
again; the pause persists across resume and compaction. No-goal and repeated-stop
requests are harmless. `/goal show`
and `/name show` also inspect the current session during a reply. Other metadata
mutations retain their pending-work conflict. The metadata-only external spelling
is `ego resume ID --goal-operation stop`; the usual writer-lease boundary applies.

The model can create an automatic goal when a request needs work and verification,
not for every simple answer. The `goal` tool uses `operation=create` with
`objective`, or `operation=cancel` with the exact current `goal_id`. Inactive
sibling fields may be omitted or null. Cancellation is limited to model-authored
automatic goals; stale identities, malformed requests and operator-owned goals
refuse without changing unrelated state. Creation returns the identity, and the
active-goal system block includes its current identity and owner, including after
compaction. Cancellation preserves history and permits later creation. A goal
check result cannot schedule a continuation after cancellation wins the shared
journal admission; a continuation already admitted is not implicitly interrupted. It records the requested outcome and verification,
separately from the ordered worklist. The model cannot rewrite an active goal's text
or ownership. Compaction preserves both and the used continuation allowance.
Automatic goals pause after 20 automatic continuation turns without claiming
success. `/goal resume` grants 20 more turns to an exhausted active automatic
goal and starts an ordinary work turn. The metadata-only spelling is
`ego resume ID --goal-operation resume`; submit a work prompt afterwards. Neither
operation replaces the goal or expands effect permissions. An operator goal is
not subject to this automatic default and cannot use this resume operation.
At a natural stop the independent check verifies the outcome, continues only
useful already-authorized work, waits for in-flight work, or requests the operator.
Repeated tool output or tool activity alone does not prove progress or success.

**Tool rows.** Every tool call leaves one row in the transcript, committed
through the same interactive owner as the answer and never through the
ephemeral status row. The row is drawn when the call finishes, because it needs
the outcome and the duration: `✓ read calc.py · 2 lines · 75ms`, `✓ edit
calc.py +3 -1 · 12ms`, `✓ $ seq 1 20 · exit 0 · (timeout 2m) · 0.4s`, `✗ $ make · exit 2 ·
(timeout 2m) · 1.2s`, `✗ read gone.py · …no such file or directory · 3ms`. `✓` and `✗` are the
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

Foreground `exec` calls default to a two-minute process lifetime. The model can
set `timeout_ms` from 1 through 600000 (ten minutes); omitted or null uses the
default. The row shows the effective limit as `(timeout 2m)` or the selected
value. Longer work uses `background=true`: omitted or null then means no
deadline, and an explicit lifetime remains supported. The model can call
`exec_monitor` with the returned ID to receive new output lines without
polling: see "Background output monitor" below. Promoting a running
foreground command with Ctrl+B retains its original lifetime limit. On expiry,
the existing process boundary terminates the child and its descendants and
reports a timeout. Time while ego is suspended does not count toward the limit.

**Background output monitor.** `exec_monitor` takes the background exec `id`,
an optional `pattern` (a regular expression) and an optional `max_lines`
(1 through 100; omitted or null means 20). It needs the same authority as
`exec_status`. From the call on, every new complete stdout or stderr line that
the pattern matches (every new line when `pattern` is omitted) is queued. A
line that was in progress at the call is not new output. The masked bytes are
the same bytes as the `exec_status` tail. The queue enters model context at
the next generation boundary, before the settlement of the same command, as
one notice: `background exec ID monitor: N new lines`, then one
`stdout: LINE` or `stderr: LINE` row for each line. An idle interactive
session wakes for a queued delivery as it wakes for a settlement, so the model
does not need a polling turn. Headless and ACP runs receive the delivery at
their next generation boundary. One delivery carries at most `max_lines` lines
and 4096 bytes of line text; a line longer than 512 bytes is cut and marked
`[line cut]`, and the pattern is tested on the cut line. Matching lines after
either bound are counted as `N later lines omitted`; `exec_status` still shows
the current tail. The monitor ends when the command settles, including
cancellation and timeout. A second call for the same ID replaces the pattern
and the limit and keeps the queued lines. A call for an owned settled ID returns a normal no-new-output receipt and
keeps any queued monitor lines. Use `exec_status` to read its output and outcome.
An unknown or foreign ID is refused. Without a pattern, every delivery of a chatty command can start a
new turn in an idle session; use a pattern for a server or a build.

A submitted prompt is echoed in full. This receipt does not change the prompt
sent to the model or saved in history.

Compact successful process previews show up to three useful output rows.
Rows with one to three punctuation or symbol characters, such as `}`, `});`,
`[]` or `…`, do not consume that preview budget. Spaces and tabs do not count.
A letter, number or fourth non-whitespace character keeps the row visible.
The omitted-row count includes filtered rows. Failed-command output and
stderr remain unchanged, and expansion retains the complete output.

Successful `edit` and `write` calls show a one-line summary of added and
removed lines in the header and the first twelve diff rows directly below it,
including removed and added lines. A longer diff shows the remaining row count
and retains its complete expansion. A unique `edit` diffs its two fragments.
A `write` and a repeated substitution (`replace_all`) diff the file against a
before image that the result carries: a new file shows as added lines. The
image holds at most 64 KiB of UTF-8 text, is read under the write authority,
is journaled with the result and never sent to the model. When the replaced
file is larger, is not text or is a protected credential file, the receipt says
the diff is omitted and the row shows the receipt only.

Rendered Markdown answers keep link labels compact. A relative local target is
resolved against the run workspace, including `-C` and selected-session changes,
and emitted as an absolute `file://` URI through OSC 8 when the terminal supports
hyperlinks. The visible label can remain a short relative path. Without hyperlink
support or a resolvable target, the original destination appears beside the label
once, unless the label already is that destination. Piped Markdown is unchanged.

`/expand`, also `Alt-O`, prints the full result of the most recent tool call
under its row: for `edit` and `write`, the line diff; for every other tool, the whole
result text the model received, up to 256 KiB. Expanded process output omits
labels for empty stdout and stderr streams; nonempty streams retain their labels.
Only the 16 most recent tool calls of the session are kept, and only the latest
can be shown. It runs between turns,
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

The terminal editor queries Kitty keyboard protocol support at startup. On a
supported terminal, each composer or modal read pushes `DISAMBIGUATE_ESCAPE_CODES`
(`ESC[>1u`) and pops it on return (`ESC[<1u`). Ctrl+Z releases the mode before
stopping the job; `fg` enables it again when the same read resumes. Editor drop,
including panic unwinding, restores an active mode. Terminals without support
keep the existing CR and LF behavior; they cannot distinguish Ctrl+Enter from
Enter unless the host provides a separate encoding.

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
`--attach` path (`docs/internals/decisions/2026-09-29-interactive-image-paste.md`).
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

The session picker opened by `/resume` or promptless `ego resume` loads ten
readable sessions of the current workspace per page. `/sessions` and `/history`
retain their cross-workspace browsing scope and use the same paged loader. Page Down loads
the next ten; filtering applies to the current page. Candidates are ordered by
`events.jsonl` last-write time, with descending session-ID ties. Unreadable or
in-flight sessions are silently skipped without taking a lease, repairing data,
or writing files. Page loads do not block typing, Escape, or `/quit`. A refused
writer lease on selection returns to the current prompt. `ego ls` retains its
recorded event-time ordering and diagnostics.

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
keyboard-independent equivalent. `/model QUERY` selects a model for this
session when exactly one available chat-model slug contains the query,
ignoring case; ambiguous or absent matches are refused. The shortcut uses
the target's stated default reasoning effort, or keeps the current effort if
the target supports it. Otherwise use the picker to choose an effort explicitly. The picker shows provider,
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
A same-provider change writes `Model: <slug>` to the transcript. A selection
made while a request is in flight applies at the next generation boundary.

To change providers, select an available target before the first prompt or
while the session is idle and pending work has finished. Before the first
prompt, the selection keeps the session ID. After a turn, Ego shows the
destination account, any configured endpoint, and the history that a later
prompt will share. After confirmation and provider admission, it opens a fork
and prints `ego resume SOURCE_ID` for the original session. The fork carries
conversation text and tool results. Private provider reasoning, prompt cache
state, and session-only grants stay with the source. A successful switch runs
the `PostModelSwitch` hook.

The picker owns no rows. Story 046-e52a brings the final shared catalogue into
M1: supplied hosted rows and authoritative live provider/local discovery form
one ordered projection consumed by generation-time resolution, `ego models`,
`/model`, and `Meta-P`. `ego models --json` is its versioned non-interactive
form. Selection resolves the chosen slug again at the generation boundary, so
a stale listing cannot bypass changed credentials, capabilities, or
availability. Missing facts remain explicit and never become UI defaults.

Local control actions such as the model picker and status inspection execute
immediately while the harness is busy. The action registry only declares
whether an action is local or prompt-producing. Every prompt-producing spelling
dispatches through story 048-6961's sole coordinator and §6.3 semantics; the
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
While a foreground `exec` runs, the last three output lines can appear below
`Working`. This preview updates in place and disappears when the command ends.
It does not change the final tool result.

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
interactive surface to it. Successful live compaction does not print preparation,
publication, skill-generation, session-identity, or continuation notices. Only
problems are reported. During manual or automatic compaction, the live activity
row reads `Compacting` with the elapsed time and existing interrupt hint, rather
than the generic `Working` label. It returns to the appropriate ordinary activity
or idle state when compaction settles.
Structured lifecycle events retain the source and target identities. The model
resumes unfinished work without a context-restored acknowledgement or recap.
The successor occupancy remains unknown until its next generation; checkpoint
length does not measure its freshly resolved provider baseline. A tip `/fork`
prints its session identities and preserves the measured source context.
The new session resolves ordinary system, project,
runtime, model, and tool context afresh. The only conversation material carried
from the source is one distinct context-checkpoint block containing the
compaction response and the latest user request, copied verbatim by core into a
separate field. A new user message replaces that quote; repeated compactions do
not accumulate quotes. It is historical context, not a declaration that the
request remains unfinished. The summary records completed work. Old checkpoint
records without the field remain readable. A context clear does not retain the
source request. No old user, assistant, reasoning, tool-call, tool-result, or
retained-tail item is replayed. The checkpoint is context, not a new user request
or higher-authority instruction.

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
`docs/internals/decisions/2026-08-24-compaction-seeds-a-clean-session.md`.

#### Pinned standing user constraints

`/pin TEXT` keeps the exact text as a standing user constraint outside the
model-written summary. `/pin` or `/pin list` lists pins with stable numbers. `/unpin NUMBER`
removes one. Numbers are not reused. To change a constraint, remove its old pin
and add the replacement text. Text states its own scope; ego does not infer it.
These are local commands, not model requests. Pasted or quoted examples do not
execute them. Pins persist across replies, resume, and compaction successors;
they are not global preferences.

Pins are limited to 64 entries, 2,048 UTF-8 bytes per entry, and 16,384 total text
bytes. Invalid or oversized operations leave existing pins unchanged. No pin is
silently evicted, truncated, or summarized. Ordinary unmarked chat text is not
automatically pinned. Workspace instructions retain their separately admitted
system authority and are loaded again for a compaction target; they are not
copied into the user pin ledger.

Each checkpoint carries a typed copy of the ledger. Its Binding constraints
and preferences section is rendered from pins rather than accepted from the
model, including when the ledger is empty. Multiline pin text is indented in the
Markdown view; the typed carrier preserves every text byte. The remaining
summary is fallible historical context, not additional standing authority.

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
The operator can then invoke `/compact` or `ego compact`. Interactive sessions
also compact automatically at `compaction.threshold` and after a typed
context-limit error; headless runs do so when `compaction.headless` is on (see
"Bounded reads and automatic context recovery" in §8).

ACP v1 advertises the optional `_ego/compact` extension in
`AgentCapabilities._meta`. The durable lifecycle adapter also advertises
standard load, list, tip fork, resume, delete, and close. Its request is
`{"v":1,"sessionId":"...","requestId":"..."}`; its response carries
`v`, `sourceSessionId`, `sourceSeq`, `targetSessionId`, `publication`, and
`successorStartRequestId`. A successful response registers the target so the
next standard `session/prompt` adopts it. The extension itself adds no standard
lifecycle operation; those are served separately by the durable M5 adapter.
Because ACP has no
standard context-overflow stop reason, a `session/prompt` whose recovery is not
possible returns a structured application error naming `_ego/compact` after the
typed settlement is durable; it does not report max tokens or refusal.

An ACP prompt compacts automatically with the same threshold and clean-session
compaction as a headless run, only for a client that accepts the transition with
`clientCapabilities._meta.ego.compacted.version` = `1`. Any other client keeps the
typed overflow error and explicit `_ego/compact`. The prompt continues in the successor and answers
the continuation's stop reason. Before the first continuation update, the agent
sends a `session_info_update` on the client's session id with
`_meta.ego.compacted` = `{"version":1,"sourceSessionId","sourceSeq","targetSessionId"}`
(advertised as `_meta.ego.compacted` beside `compact`). After the prompt
settles, the connection rebinds the client's session id to the successor:
`session/prompt`, `session/cancel`, `_ego/steer`, `_ego/retry`, `_ego/compact`,
`_ego/pause` and `_ego/resume` that name the old id reach the successor. The
rebinding lives only in this connection; a client that reconnects must load
`targetSessionId`. A failed compaction returns the error above.

At M3 `ego mcp-server` exposes five tools: `run`, `continue`, `cancel`, `status`,
and `compact`. The compact input is exactly
`{"request_id":"...","run_id":"...","expected_revision":N}` and its
version-1 result carries `source_session_id`, `source_seq`,
`target_session_id`, `publication`, and `successor_start_request_id`.
Visibility and revision checks remain mandatory. A `compact` operation failure
returns MCP `isError: true`. Invalid `compact` input and a failure of `run`,
`continue`, `cancel` or `status` (refused authority, unknown run, invalid
argument, request-id replay with a different digest) return a JSON-RPC
`invalid_params` error (-32602). Published
uncertainty is a successful non-retryable tool
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

`ego rewind ID --at ITEM_ID` returns the conversation and the files to the same
turn. It selects the turn as `ego fork --at` does, takes the session's lease
(so the session must not be running), and finds the file checkpoints that the
session's builtin `write`, `edit` and `notebook_edit` results recorded after
that turn (see "Session-owned file checkpoints"). Before it changes anything,
it checks each file against the disk and against every later write of it, and
refuses, listing the files, when a file changed outside ego after that turn,
also between two of those writes; keep or undo those changes first. It also refuses when the checkpoint chain holds writes after the turn
that no result records (a `task` child's writes, or writes from a version that
did not record checkpoint ids), when a write after the turn saved no checkpoint
(checkpoints are off, or the file is outside Git), and when a recorded checkpoint
is gone from the repository. Otherwise it restores the checkpoints newest first and then creates
the fork. Text output is the new session ID, with one `restored PATH` note per
changed file; `--output json` lists every checkpoint with its path and effect.
The source session is not changed: its journal stays, each restore is a restore
checkpoint in its ref, and the new session's lineage names the source and the
turn. Exec commands and MCP effects are not captured, so they are not undone.
Use `-C` for the repository the session wrote to. A failure while restoring
leaves the earlier restores applied and creates no session; the same rewind can
be run again, because a completed restore repeats safely.

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
before current user intent. Provider cache usage and subscription continuation are separate observations; see
[subscription transport](docs/guides/providers.md#subscription-transport), with decisions in
`docs/internals/decisions/2026-09-30-conversation-fork-cache-prefix.md`.

## 7. Configuration

### Named account connections

Connection metadata belongs only to user configuration. Project configuration
cannot define accounts or redirect the selected identity. The root `connection`
is an internal account reference; use `--account N` with a model to override it for an invocation.

```toml
connection = "personal"

[connections.personal]
name = "OpenAI 1"
vendor = "openai"
access = "oauth"

[connections.work_key]
name = "Work"
vendor = "anthropic"
access = "key"
key_env = "WORK_ANTHROPIC_API_KEY"
```

The map key is the identity. Edit `name` directly to rename it; names may repeat
and do not select credentials. Key connections with `key_env` use only that environment-variable
name, not a plaintext key. Without `key_env`, a key connection uses only its
named credential in ego’s private auth store. Use a secret-marked variable such as `WORK_API_KEY`
(a name segment must end in KEY, TOKEN, SECRET or PASSWORD); this keeps the value
in the tool-output masking inventory. Values shorter than eight bytes in named
`key_env` sources are refused. OAuth connections forbid `key_env`. Multiple
matching accounts require a configured default or `--account N`; map order and
the last login never select an account.

`ego models --provider PROVIDER --account N` uses that account for discovery and its saved model
catalogue. Named caches are separate from other accounts and the ambient cache;
a rename does not change the cache identity. A conflicting `--provider` refuses
before discovery. Without `--provider`, a selected connection limits the catalogue
to its route; multiple accounts without a default require a provider and account number. The
command does not probe every saved account. ACP restores the saved session account for its model picker,
not the process default account's discovered models.

In an account-bound terminal session, `/model` and `model_catalogue` can show supplied
models from other providers as unavailable in that session. Start a new session
with `ego -m PROVIDER/MODEL` to use another provider.

`ego auth login PROVIDER` creates a separate saved account. Use
`ego auth login PROVIDER --renew` to renew the same upstream account; if the
provider has multiple accounts, add `--account N`. See the numbers with
`ego auth status PROVIDER`. `ego auth logout PROVIDER` removes the local
credential but retains the identity and number; `ego auth remove PROVIDER`
removes the account. Both require a selector if several accounts match.
Deleting account 1 leaves account 2 unchanged, and no selector is needed if
account 2 is the only one left. A named
connection never falls back to another saved account, an ambient subscription
token, or a different API key. Login does not silently switch an active session.
Subscription login programs must return OAuth material; key material is refused
before either store changes. `ego auth login gemini` asks for an API key with terminal echo
disabled, even if an environment key exists. It saves the key in the private
auth store, never in TOML. `ego auth login openai-codex` starts OAuth and does
not ask for an API key. Neither needs `--method`.
If both methods are enabled, the interactive command asks you to choose;
`--method api-key` and `--method oauth` bypass that choice. API-key entry always
requires an interactive terminal; pipes and redirected input are refused. Never
put a key in command arguments or in a shell command. For a noninteractive OAuth
login on a dual-method route, select OAuth with `--method oauth` or `--device`.
Existing environment-source connections cannot be
replaced by stored-key login; create a separate connection instead. Ordinary
runtime environment-key use is unchanged.
The built-in OpenRouter login issues a key rather than OAuth, so it is not offered
as named OAuth login and refuses before network/browser exchange. Supply its key
through a key connection instead. Key-returning login programs are refused at
publication; this does not revoke a key such a program has already created.
An expired named account needs targeted re-login with
`ego auth login PROVIDER --renew` (with `--account N` if needed). In a named session, `/login` refuses
with that command rather than creating a replacement account. In an unpinned session, bare `/login` can render its built-in provider inventory during an active reply, without starting authorization or another generation. `/login PROVIDER`, Setup and Init remain unavailable while a reply is running; modal ownership and named-account restrictions are unchanged. Bare `ego auth status`
lists saved accounts and orphaned named slots, not old provider-keyed records.
Remove an orphaned named credential with `ego auth --connection ID logout`.
For an environment-backed key connection, `logout` refuses; remove its declared
variable from the process environment. For a stored key, `logout` deletes the
local copy, not the key at the provider. Use a new connection for a different
upstream account; targeted re-login assumes that the stable ID still names the
same account.

`ego auth status --json` is the version-1 local credential-presence read. This
explicit machine mode is approved for settings integrations; unflagged status,
including its human headings, credential type/expiry text and empty-output
behavior, is unchanged. Its complete success envelope is:

```json
{"v":1,"credentials":[{"provider":"anthropic","connection":"account_a","present":true},{"provider":"anthropic","connection":"account_b","present":false}]}
```

Each record has a boolean `present` and a `connection` field containing the full
stable ID. `provider` is the configured vendor, not the OAuth route or display label.
A stored orphan retains `connection` and omits the unknown `provider`. Identities
use their existing nonempty ASCII letter/digit/underscore/hyphen grammar without
a new length limit. Consumers must reject unknown versions or malformed identity
records explicitly, and preserve the connection ID and optional vendor independently.

The inventory contains every configured named slot plus stored named orphan slots.
A configured key is present when its declared process environment source is nonblank,
or, for a stored key, when the named store entry contains key material. A configured OAuth slot is
present when its namespaced store entry exists. Presence is local source
existence, not credential kind, expiry, validity, entitlement or provider success.
The read does not invoke credential commands, refresh or contact a
provider. JSON contains no labels, secret bytes, environment variable names,
key-source text or expiry diagnostics. With no inventory it emits
`{"v":1,"credentials":[]}` and a newline, without an empty-listing diagnostic.

Ordering is deterministic: named records (configured and orphan together) are
sorted by full connection ID. A provider selector filters by configured vendor;
`--account N` selects one account. A missing configured source is reported with
`present:false`. Unfiltered inventory can report stored orphans.

Saved logins require named connections. Older provider-keyed records are ignored
and left untouched. Run `ego auth login PROVIDER` to create a named account;
there is no conversion command or fallback to a provider-keyed record.

Plain resume and ACP reconnect restore the saved stable ID, not its display name
or the current config default. Missing identities refuse preparation. On CLI
resume, an explicit model from another provider resolves that provider’s account;
use `--account N` if the selection is ambiguous. A model override within the same
provider keeps the saved account. An ACP model pick can switch providers before
the first prompt, but after that the host must start a new session. Private
provider continuation from another or an unbound connection cannot be adopted
by a named connection; start a fresh session instead. Native search still needs
route-specific wire support and entitlement; a saved connection grants neither.
A configured default connection binds one vendor and route. Selecting a different
vendor, including a judge model, refuses rather than changing accounts; use a
matching explicit connection for that invocation.

### Trusted provider instances

User and system configuration may define complete deployment instances under
`[providers.instances.ID]`. Project/local configuration, environment and invocation
layers may select models but cannot define or redirect endpoint/auth authority,
even in a trusted workspace. Duplicate instance IDs across declaring layers are
rejected; endpoint and credential fields are not merged independently.

Select an instance model as `ID/ALIAS`. The alias is separate from its upstream
model ID and never chooses a wire format by its spelling. Each instance contains:

| Field | Required contract |
| --- | --- |
| `profile` | One of the compiled profiles in the table below. |
| `base_url` | HTTPS, or HTTP on an exact loopback host; userinfo, query and fragments are forbidden. |
| `credential_ref` | Stable ID of an existing matching connection. Use OAuth for `vertex-gemini`, a resource-bound key or OAuth connection for `azure-openai`, and a matching key connection for the other authenticated profiles. It may be absent only for an exact-loopback `openai-compatible` instance. |
| `models.ALIAS.upstream_id` | Explicit nonempty upstream model/deployment ID. |
| `models.ALIAS.contract` | Exact allowlisted mapping compatible with the profile. |
| `models.ALIAS.facts` | Strict native model-capability facts, including the current schema version and explicit positive context/output limits; output must not exceed context. Unknown fields fail instead of being silently ignored. |
| `models.ALIAS.metering` | Explicit metering identity; a separately configured endpoint does not inherit the compiled service's prices or provider-reported settlement decoder. Unknown prices remain unknown. |

| Profile | Admitted model contracts |
| --- | --- |
| `openai-compatible` | `chat-standard`, `chat-max-tokens`, `responses` |
| `openai` | `chat-standard`, `responses` |
| `azure-openai` | `chat-standard`, `responses` |
| `anthropic` | `messages` |
| `gemini` | `gemini-interactions` |
| `vertex-gemini` | `gemini-interactions` |
| `deepseek` | `deepseek` |
| `bedrock-runtime` | `bedrock-converse`, `bedrock-chat` |
| `bedrock-runtime-sigv4` | `bedrock-converse`, `bedrock-chat` |
| `zai` | `zai` |
| `kimi-api` | `kimi-api`, `kimi-responses` |
| `mistral` | `mistral-conversations` |

For `vertex-gemini`, bind an explicit Google Cloud project and location in the
HTTPS v1 base URL. Use a matching named OAuth connection and declare the
Gemini 4 Argon foreground model facts. See the [Vertex Gemini setup](docs/guides/providers.md#gemini-on-google-cloud-vertex-ai).

For `azure-openai`, the v1 resource base must match the connection's
`azure_resource` and `upstream_id` must name the Azure deployment.
See [Azure OpenAI v1 deployments](docs/guides/providers.md#azure-openai-v1-deployments)
for key and Entra setup.

`bedrock-runtime` is distinct from the existing Bedrock Messages profile. Its
`bedrock-converse` contract uses native binary eventstream for tool-free turns
and synchronous Converse JSON for tool turns. Supply a regional runtime base,
matching API-key connection, explicit upstream model, context/output/tool limits,
Off-only/default-Off reasoning, native `amazon_event_stream` transport, and no
reasoning replay, cache or modalities. API-key credentials use bearer presentation;
this profile does not implement SigV4, discovery, default models or price facts.
A false parallel capability does not enforce server-side single-call output.

`zai` and `kimi-api` are API-key profiles separate from coding subscriptions.
Their exact contracts cover GLM-5.3 and Kimi K3.1 text/tools, respectively, with
Low/High/Max efforts, default Max, SSE and matching provider-only reasoning
replay (`zai-glm53-chat` or `kimi-api-k3-chat`). Supply explicit model aliases,
context/output/tool limits and matching key connections. The Chat contracts
omit parallel controls and refuse explicit cache retention and modalities.
Z.ai uses `max_tokens` with preserved thinking. Explicit `--tools web_search`
declares Z.ai's in-chat hosted tool; the published stream schema does not
provide search sources or a search count. Kimi Chat uses `max_completion_tokens`
and does not offer hosted search.

A `kimi-api` instance can instead select `kimi-responses` with the
`kimi-responses` replay dialect, including hosted search and the Responses cache
retention options. This does not change existing Kimi Chat or Coding selections.
An instance with profile `mistral` and contract `mistral-conversations` uses a
matching Mistral key connection and the `mistral-conversations` replay dialect.
It sends stateless Conversations requests and can use hosted search alongside
local function tools. The Conversations schema provides no prompt-cache control
or cache counter. Neither instance contract infers a default model, model
facts, price, account entitlement or live conformance.

`chat-standard` keeps the existing `max_completion_tokens` projection.
`chat-max-tokens` is a bounded nonreasoning text/tool contract using `max_tokens`:
declare Off-only/default-Off reasoning, an explicit tool limit, no enabled parallel
control or reasoning carryover, SSE, no cache retention and no positive image/audio
support. It omits unproven parallel and stream-usage controls, and refuses unsupported
controls or opaque state rather than assuming universal compatible-server behavior.
A false parallel-capability fact is not a guarantee that a server emits at most one
tool call. The actual nonreasoning deployment, tool parser/template and limits remain
operator declarations; this mapping alone does not establish every local/gateway row.

Capabilities are operator declarations for the actual deployment, not inferred
from a model-list response or compatibility label. Text/tool, reasoning,
transport, cancellation and input-modality facts retain their existing typed
meanings; missing required facts and unsupported mappings fail before publication.
Two instances of one profile keep independent IDs, endpoint routes, credential
references and model facts. Unknown aliases and conflicting connection selections
fail rather than falling back to another model, account or compiled profile.

The existing atomic user-config mutation boundary sets or removes one complete
instance with an expected prior value; stale updates fail without overwriting the
newer document. Saving a new instance affects future admission. An already
admitted generation retains its captured endpoint, credential reference, mapping
and facts. Runtime selection, model listing and the startup picker consume that
captured configuration rather than rereading endpoint authority independently.

For example, a trusted user configuration can declare a text-only loopback
server without a credential reference. The model ID and facts below are
**illustrative operator declarations**, not discovered capabilities: replace them
with the limits, template and behavior established for the actual deployment.
`unmetered` is an explicit local billing policy, not an inference from its URL.

```toml
[providers.instances.workstation]
profile = "openai-compatible"
base_url = "http://127.0.0.1:1234/v1/"

[providers.instances.workstation.models.assistant]
upstream_id = "served-model-id"
contract = "chat-standard"

[providers.instances.workstation.models.assistant.facts]
schema_version = 6
max_context_tokens = 32768
max_output_tokens = 2048
max_tools = 0
supports_parallel_tool_calls = false
reasoning_carryover = "unsupported"
reasoning_efforts = { only = ["off"] }
default_reasoning_effort = "off"
cancel = "unsupported"
cache = "none"
transport = "sse"
image_input = "unsupported"
audio_input = "unsupported"

[providers.instances.workstation.models.assistant.metering]
kind = "unmetered"
revision = "local-declared-v1"
```

Select this explicit text-only contract with
`ego run --model workstation/assistant --thinking off --no-tools "Summarize this text"`.
A remote authenticated instance must name a matching existing connection
with `credential_ref`; do not put a key or token in the instance table. Unknown
reasoning support is not equivalent to `off`: declare the supported effort set
explicitly when the deployment contract establishes it.

Model listing, startup and agent model metadata, setup readiness, and saved ACP
model choices project the same validated instance definitions. Readiness reports
local presence in the instance's exact named connection. Setup checks the
declared environment variable or the matching kind of saved credential.
Credential commands, token refresh and entitlement checks belong to generation.
Invalid credential references fail admission. A missing declared key leaves
the instance unavailable.

MCP deployment policy and run preparation share one captured configuration.
Changing the saved instance does not rewrite that deployment's model allowlist,
endpoint, upstream ID or facts. ACP picker capture and actual turn/resume
admission must likewise share the existing session capture; picker metadata alone
is not execution authority. Persisted reload is a separate admission boundary,
not a guarantee that old secret bytes remain pinned.

This interface does not add custom cloud/subscription credential rebinding, an
interactive instance-setup wizard, or support for an undocumented cloud route.
Only the exact compiled contracts listed above are selectable.

### Configuration locations

```
/etc/ego/config.toml      system    (admin: fleet defaults + non-relaxable
                                     deny/budget ceilings — the only source
                                     of ceilings; ego itself imposes none)
~/.ego/config.toml        user      (EGO_HOME overrides ~/.ego)
.ego/config.toml          project   (committed; no secrets)
.ego/config.local.toml    project-local (gitignored)
```

TOML, flag-mirroring keys, `[profile.NAME]` sections selected with
`--profile NAME` (one flag flips a whole posture). The `ci`
profile is a **contract**, not detection: explicit trusted config, headless
credential sources, isolated session/artifact dirs, no prompts, sanitized run
bundle. It activates only through `--profile ci` or `EGO_PROFILE=ci`; ambient
`CI` never activates it. Its exact root configuration is:

```toml
[profile.ci]
session_dir = "/absolute/precreated/session-root"
artifact_dir = "/absolute/precreated/artifact-root"
```

An optional profile value sets a display-only estimate for provider-managed
prompt-cache lifetimes:

```toml
[profile.work]
cache_ttl_seconds = 1800
```

`cache_ttl_seconds` must be a positive integer. Select it with `--profile work`.
It does not send a provider parameter or change caching charges. Explicit wire
TTLs take precedence over this estimate. Without a known request TTL or this
profile estimate, the interactive status shows `cache=unknown` until the session
learns three comparable warm-to-cold cycles and a nearby confirmed hit. Loss
gaps must agree within 10 percent. They are upper bounds, not a safe TTL by
themselves: the largest confirmed hit gap must be below and within 10 percent
of the smallest loss gap. A confirmed hit must cover the entire preceding
request's reported input tokens; a partial shared-head read is not an anchor.
No cache-block rounding tolerance is guessed. That confirmed lower bound becomes the estimated
TTL, anchored at the latest confirmed hit request's admission.
Custom cache countdowns show `~M:SS (estimated, 3 samples)`. Learning is passive
and local to the durable session. It currently supports subscription Responses
requests with proven append-only input, unchanged scope and the same observed
full-input transport mode. Actual HTTP fallback, fresh WebSocket and reused
full-input WebSocket dispatches are distinguished in the settled journal.
Missing transport evidence and incremental continuations do not participate;
no equivalence with a full-input request is assumed. Failed or unreported
attempts, helper requests, edited prefixes, routing-header changes and long
generation latency do not contribute. A provider retention tier alone is not
an exact TTL.
The countdown uses `M:SS` or `H:MM:SS`, then shows `cache=expired`. Custom
status templates can use `{{ cache }}`. The countdown estimates request policy
or observed lifetime; it does not guarantee that the provider stored a prefix
or will reuse it.
Read renewal is applied only for documented policies; a profile estimate
explicitly assumes renewal on each request. Kimi keeps the first-write tier
while the tracked entry is live. `cache = "none"` leaves the countdown unknown.

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
are not confined. A missing or unresolvable configured root never stops ego (story 966-97ad). Each
skipped or narrowed entry is reported once on stderr, once in the model's first
request (`Configuration notices` among the runtime facts), and by `ego config ls
--effective` and `ego doctor`, with path, reason and effect. A skip never widens
authority: a missing read root stays as a read root at its nearest existing
canonical ancestor plus the missing tail, so it still restricts writes inside a
writable root or the workspace; an unresolvable read root stays by its configured
path only where it restricts; every entry that would grant authority and cannot
be admitted (a missing or unresolvable read-write root, a root over a system
ceiling, a permanent root without a committed receipt) is skipped. It is never
shown as an admitted root. A directory a host asks for explicitly is still refused
when it cannot be admitted.

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

`ego doctor [--json] [--offline]` and `/doctor` are two renderings of one ordered,
versioned `HealthSnapshot`: config, credential status, session store,
sandbox/backend, provider/model catalogue, MCP, skills, hooks, and the ACP
surface this build advertises — the protocol version it serves and each `_ego`
extension with its version, which is the one fact about ego that only a second
implementation could previously discover. Doctor is
non-mutating and redacted. It may read existing facts and perform bounded local
or loopback checks, plus one unauthenticated reachability probe per configured
provider (DNS, TCP, TLS and a bare HTTP `GET`, each failure naming its layer;
skipped under `--offline` and the offline posture); it never logs in, refreshes
credentials or catalogues, reloads extensions, edits config, sends a credential
or a paid provider request, uploads logs, or widens egress. `ok`, `warning`, `error`, and `not_checked` stay distinct; root
exit is 1 only when an operational error exists. Two facts ego cannot read
without mutating something are reported `not_checked` with the reason and the
command that would establish them: sandbox capability, which is measured by
spawning probe children, and the provider catalogue, whose refresh may reach
the network.

### OAuth login availability

Built-in Anthropic OAuth login is absent from login discovery and login-method
pickers, and direct login is disabled by default. To opt in,
set this field in your **user** `config.toml`:

```toml
[auth.anthropic]
oauth_login_enabled = true
```

`auth.<provider>.oauth_login_enabled = false` disables login for any provider.
Other providers keep login enabled when the field is absent. The setting
controls `ego auth login` discovery, `/login` discovery and interactive login
methods, and direct login before a browser, network request or credential
command can run. Only user configuration can set the field; system, project,
local, environment, flag and agent-file layers reject it. A protocol filter
does not enable login or establish provider compatibility. Opting in does not guarantee that a
provider will accept the flow.

This policy does not hide providers or disable API keys, saved OAuth
credentials, credential refresh, status or logout. Generic troubleshooting and
model-readiness hints can still point to a disabled login flow; direct login
then explains the user opt-in. You can instead set `ANTHROPIC_API_KEY` to use
Anthropic with an API key without enabling OAuth login.

### 7.1 Operator-supplied identity headers

**ego does not ship them.** No request ego builds claims to be another agent,
an editor extension, or an editor. Where a row declares a client at all it
declares ego — `User-Agent: ego/<version>`, plus `originator: ego` where the
subscription backend reads one. Most rows declare none: `anthropic`, `openai`,
`gemini`, `xai`, `xai-oauth`, `nous`, `openrouter`, `minimax-oauth` and
`ollama` send no `user-agent` at all, and nothing downstream fills one in.
Sending no name is not the same as sending someone else's, which is the whole
of what was removed. Boss ruled it on 2026-09-05
(`docs/internals/decisions/2026-08-22-subscription-login-is-built-in-plus-two-overrides.md`
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
user-agent = "example-client/1.0"
x-app = "cli"
anthropic-beta = "example-beta-flag"

[auth.github-copilot.headers]
user-agent = "example-editor-chat/1.0"
editor-version = "example-editor/1.0"
editor-plugin-version = "example-chat/1.0"
x-client-id = "example-integration"
```

`docs/internals/16-subscription-login.md` §7.8 carries the same values with their pinned
client source, the vendor terms each one touches — Anthropic reserves OAuth to
its own applications; GitHub's four are a VS Code extension version gate — and
which requests it reaches. It reaches all of them: generation, `ego auth
login`, and the refresh that follows, the subscription token exchange included.

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
the default status row includes it when enabled. Token quotes and the subscription’s
separate API-equivalent estimate use the selected tier; unavailable prices stay
unknown, not zero. `ego models` lists verified fast support and its multiplier.
See `docs/internals/fast-service-tier.md` for provider evidence and account limits.


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
openai = "openai/gpt-6.1-sol"
anthropic = "anthropic/claude-opus-5-5"
```

The builtin choices above are ego policy. A stronger layer replaces only the
provider keys it names; omitting a key does not delete an inherited default.
Each value must be a full model slug belonging to its provider key.

When configuration has no selected model, interactive startup observes
credential presence and performs bounded Ollama discovery. Exactly one usable
provider with an available mapped default is selected and saved atomically
before execution. Otherwise startup opens the shared model picker; it names the sign-in remedy
above each unavailable provider's rows. An
explicit or configured model bypasses this first-run selection. Headless startup
with exactly one usable provider and an available mapped default uses that model
for the run, names it on stderr, and never persists it. With zero or several
usable providers, or no available default, headless startup refuses; the refusal
names detected providers and remedies using `--model` or `ego config set model`.

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
See `docs/internals/decisions/2026-10-11-current-generation-builtin-catalogue.md`.

The `openai` rows are shipped and selectable. The row speaks the public
Responses contract, and
`docs/internals/decisions/2026-09-07-a-published-contract-is-enough-to-offer-a-row.md`
admits a public row on a published contract with passing deterministic
fixtures. It amends
`docs/internals/decisions/2026-09-03-public-provider-wires-require-live-promotion.md`,
which required a sanitized recording of a live call first: no such recording
exists, and withholding the row made every operator's key useless against a
documented endpoint to avoid an error that announces itself — a wrong Responses
body is a `400` from OpenAI on the first call. `ego models` lists the row as
available once `OPENAI_API_KEY` resolves and as `missing_credential` when it
does not.
See `docs/internals/decisions/2026-09-07-a-builtin-catalogue-layer-below-system-and-user.md`.

`openai-codex/gpt-5.6-sol` is the current built-in subscription
default. It becomes available after `ego auth login openai-codex` stores its
OAuth credential; no user `models.toml` is needed. Its 128,000-token output
ceiling is the public model fact, while its 272,000-token context ceiling is
the smaller effective limit from the authenticated subscription catalogue, not the
1,050,000-token Platform API limit. This makes preflight conservative for the
actual subscription endpoint. The subscription has no built-in monetary rate;
it uses subscription metering. Its currently expressible reasoning default is
`low`; image input, opaque reasoning-state carryover, parallel tool calls, and
hard tool limits are not claimed for the private subscription contract.
See the subscription bootstrap ceiling decision in `docs/internals/decisions/`.

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
cache = "short"                    # "none" | "short" (5m) | "long" (1h; user config, or a trusted project)
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
(`docs/internals/decisions/2026-08-25-the-catalogue-is-a-snapshot-not-generation-authority.md`).
Only supplied rows come from this file, and they are rebuilt on every
invocation rather than cached at all; the cache holds only *discovered* rows,
from providers that enumerate themselves.

An obsolete discovery-cache schema is ignored at startup. Supplied facts and
bounded local discovery remain available, and an ordinary catalogue read can
rebuild the cache without `--refresh`. Startup does not replace the old bytes.
A corrupt or newer-schema cache still reports an error and is not overwritten.

### 7.3 Workspace memory

Memory is off. Turn it on with the `memory` row in `ego setup`,
`ego config set memory=true`, or `--memory` for one run. Only the user
configuration and the command line can set it; a project or local file that sets
`memory` is refused.

When on, each workspace has one Markdown file, `$EGO_HOME/memory/NAME-HASH.md`
(`NAME` is the workspace directory name, `HASH` is a digest of its full path),
limited to 16 KiB. It is user state outside every workspace write scope: only you
and the model's `memory` tool change it. The tool adds, replaces or removes one
single-line `- ` entry by its exact text, refuses a change that passes the limit,
and is refused in plan mode. Every other mode applies the call without a prompt;
the call is visible in the transcript.

The file is read at session start and at `/refresh` and appended to the system
prompt after the instruction files. A file over the limit is not sent and a note
says so. `--bare`, `--system` and a replacing session prompt send no memory.
`session.started` lists the file with source `memory`, its path, digest and size,
never its text. `/context` shows the system prompt sources, the memory state and
its size. With memory off, no memory file is read or written.

### 7.4 Language-server code intelligence

The `lsp` tool is off. Turn it on with the `lsp` row in `ego setup` or
`ego config set lsp=true`. It starts processes, so only the user configuration
can set it; a project or local file that sets `lsp` is refused.

When on, the model can call `lsp` with `operation` `definition` or `references`
and the 1-based `line` and `column` (in characters) of a symbol, or with
`diagnostics` and a file. The file must be inside the workspace. The answer
lists at most 50 entries and 16 KiB: `path:line:column` and the source line (cut
at 200 characters; source text only for files inside the workspace), or the
line, column, severity, source, code and message of each diagnostic, then a
count of the omitted entries. Diagnostics wait up to 20 seconds for the server to
publish them; when it does not, the answer says so and does not claim the file
is clean. A server that is still indexing can answer nothing.

ego chooses the server from the file extension and starts it at the first query
for that language: `rust-analyzer` (`.rs`), `gopls` (`.go`), `clangd`
(`.c`, `.h`, `.cc`, `.cpp`, `.hpp`), `pyright-langserver --stdio` (`.py`) and
`typescript-language-server --stdio` (`.ts`, `.tsx`, `.js`, `.jsx`). The model
cannot choose a command. Install the server on `PATH`; a missing server is a
tool error that names it. A sandbox that refuses the server's executable, as it
refuses any command outside the exec perimeter, is a tool error that names the
path to add to `exec_paths`. The server starts in the workspace with the `exec`
environment, through the same guarded process launch and sandbox as an MCP
stdio server, so it has the run's confinement, and the offline posture refuses
it. Its standard error is kept as session evidence. One server per command
runs while ego runs and is reused by later queries; a server that exited starts
again at the next query. ego passes its process ID, and the server's standard
input closes when ego exits. ego sends the file text
from disk before each query. The tool only reads, so plan mode admits it, and
it asks no permission question: turning it on is the authority.

### 7.5 Build output recipes

Live output is compacted without any recipe. The answer of a successful `exec`
call and the status tail of a background command (`exec_status`, the owner's
background status view) apply terminal semantics (a carriage return overwrites the
line) and fold each run of build progress lines (`Compiling`, `Building [`,
`Downloading`, and similar) into its last line and a count. The model answer then
points to `read_tool_result REF` for the complete output. If the remaining
output is long, including an unknown format, it shows a bounded tail excerpt
with an explicit omission notice. Earlier warnings and errors may be outside the
excerpt; read the full evidence to inspect them. Exit status remains visible.
A failed command folds its progress too (it never takes a recipe), and complete
stdout and stderr stay in the session evidence. The native terminal reports the background commands that settled
together (during a turn, or found at the next idle wake) as one bounded block,
failures first, with each process ID and its true exit, timeout or cancel state
and a short excerpt: three lines for a failure, one for anything else. Lines an
`exec_monitor` already delivered and build progress are not repeated, and an exit
code is the process's own (a wrapper that exits 0 after a child failed shows the
child's error line, not a success). The complete output stays in the session for
the model; settlement, cancellation and deadlines are unchanged. The learned recipes below are separate: they are opt-in, exact-argv
and replace a later successful foreground answer with a stored projection.

Build recipes are off. Turn them on with the `build_recipes` row in `ego setup`
or `ego config set build_recipes=true`. Only the user configuration can set the
key; a project or local file that sets `build_recipes` is refused. `--bare` and a
missing `$EGO_HOME` turn them off for one run.

When on, the model has the `build_recipe` tool. After it sees the output of a
repeated command, such as a build or a test run, it can save a recipe for that
exact argv. A recipe is data, not code:

- `argv`: the exact argument vector. Another argument, flag or order is another
  command, and the recipe does not apply to it.
- `require`: 1 to 8 literal format markers. Every later output must contain all
  of them.
- `omit`: 1 to 16 literal line rules, `prefix` or `contains`, for noise such as
  progress lines. There is no pattern language.

The tool checks the recipe on the stdout and/or stderr full-output references
that the model names, which must belong to this session. It stores the recipe
only when every marker is present and the compact answer is smaller than the
output, and
refuses it in plan mode. It runs no process and has no field that names a
command, a permission or a rewrite: it cannot change what `exec` runs or what
the policy admits. Saving the same argv again stores the next revision.

A later `exec` call of exactly that argv that exits 0 then gives the model a
compact answer instead of the usual head-and-tail previews: a header with the
recipe ID, its revision and the count of omitted lines, then for each stream its
full-output reference and every line no rule omits. A line that contains
`error`, `warn`, `fail`, `fatal`, `panic`, `exception` or `deprecat` (in any
ASCII case) is never omitted. The model
can read the complete output with `read_tool_result`; the stored stdout and
stderr evidence, the tool result blocks and the exit status do not change, and
the terminal shows the usual output.

The original answer is kept, with no recipe, when the command exits non-zero, is
killed or runs in the background; when a marker is missing; when the output is
not UTF-8, is larger than 4 MiB, or keeps more than 6 KiB of lines; when the
compact answer would not be smaller than the output; when the recipe is disabled; and when the recipe book or the
evidence cannot be read. The session journal records the answer that was sent,
so a resumed session replays that exact answer, also after the recipe changes
or is deleted.

Each workspace has one book, `$EGO_HOME/build-recipes/NAME-HASH.json` (named as
the memory file in §7.3), written atomically and readable only by its owner. It
holds at most 64 recipes and no output bytes. A book with an unknown version is
reported and never overwritten. No recipe is shared with another workspace.

`ego build-recipes` acts on the book of the current directory: `ls` lists each
recipe with its ID, revision, state, the checked and compact sizes, and its
argv; `show ID` prints one recipe as JSON; `disable ID` and `enable ID` stop and
restart its use; `rm ID` deletes it.

## 8. Why this beats the field (claims to defend in review)

Verified against the binaries, not marketing (`docs/internals/07-cli-prior-art.md`):

| Capability | ego | typical elsewhere |
| --- | --- | --- |
| Schema-validated output | `--schema` @file or inline, **re-prompt until valid**, validated object *is* the result | an inline-only or file-only schema flag, no retry, or the result in a side field |
| Executable agent files | shebang + vars + frontmatter | prompt commands that are REPL-only or not executable |
| Exit-code contract | 9 documented codes + SIGTERM contract | an undocumented table, or only 0 and 1 |
| Retry safety | delivery-certainty-aware `--retry` | blind retries everywhere |
| One event schema | jsonl = session file = ACP, versioned | several event shapes per tool, some undocumented |
| Headless trust | untrusted ⇒ bare, by default | an opt-in bare mode, or none |
| Permission grammar | one string, three surfaces, graduated modes on the CLI | per-surface syntaxes, or one auto flag |
| Embedding | `ego acp` (standard) | a standard ACP mode in some, bespoke modes in others |
| Cost control | `--budget '2.5$'` or `'400k'`, all modes | a budget flag that works in print mode only |
| Config discipline | strict by default, `-c` dotted overrides, profiles | opt-in strict config, or invalid config skipped silently |

Known secret environment values (names containing `TOKEN`, `KEY`, `SECRET`, or
`PASSWORD`) are masked as `[REDACTED]` in process output evidence and tool-result
text before model context and event publication. Exports apply the same exact
value masking to string values. Empty values are ignored; no token-pattern
heuristics run. Child environments retain their original values.

### Interactive session picker

On a human terminal, `/sessions` and `ego resume` without an ID open the saved
sessions across project roots, newest first. `/sessions` excludes the active
session. Rows show title, model, updated local time, recorded root, and summary.
Type to filter title, model, or summary, use Up/Down to select, Enter to resume,
and Esc to cancel. Missing roots remain visible as `[unavailable]` and cannot
be selected. `/history QUERY` searches the conversational FTS5 index and opens
the same picker with the matching sessions.

The next prompt uses the selected session's durable context and saved tool
selection. Every in-process session selection re-enters normal run admission,
including within the current root, so the target's runtime tool catalogue is
rebuilt. Source argv tool overrides do not replace a later picker target's tools.
Changing roots also rebuilds project instructions, config, trust and tool
confinement for the recorded root. Within the current root, the active model and
thinking selection stay in effect. Sessions with unfinished work require `ego resume ID`,
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

Without an explicit roots list, the strict perimeter includes only the workspace.
Use the existing user `roots` setting to grant additional paths; `roots=[]`
selects workspace-only. The workspace remains writable under
`sandbox="workspace"`, including within a read-only extra root.

For one run, `--allow-dir PATH:rw` grants an existing canonical absolute
folder read-write without changing stored roots. Omit `:rw` for read-only
access. Admission and system root ceilings still apply; this does not grant
execution access.

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
`ego config set 'roots=[{path="/shared/projects",access="read"}]'` and its `--profile`
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
  { path = "/shared/projects", access = "read-write" },
  { path = "~/work-notes/ORCHESTRATION.md", access = "read" },
]

[profile.open]
sandbox = "off"
roots = []
```

The tree allowlist is explicit: include only existing files/directories needed
by the agent (for example `~/.codex` or `~/.claude` instead of the single file).
`/shared/projects/AGENTS.md` is already covered by the writable tree. Selecting open
always prints `open-perimeter`: exec children have unrestricted filesystem
access. Permission mode remains separate; open is adjacent to yolo, not a mode
change.

Production capability probes need a temporary path outside every granted root.
If TMPDIR lies within a granted root, sandbox startup refuses with
`perimeter-probe-temp-overlap` rather than weakening verification or silently
falling back. In particular, a tree run needs TMPDIR outside `/shared/projects`.

### Bounded reads and automatic context recovery

The `read` tool accepts `{"files":[{"path":"src/main.rs","offset":0}]}`,
with one to sixteen entries, including for a single-file read. It resolves and
admits every path before reading any file; duplicate resolved targets are refused.
Results are ordered JSON items with their index, requested path and status. A
successful item includes content and `next_offset`; an execution failure includes
an error and does not omit later items.

The serialized text result, including metadata and JSON escaping, is bounded
to 32 KiB, with at most 2000 content lines per file. A truncated item names its
next original-byte offset. Continue with that item's path and offset, including
inside a long line; a later item can receive no content when an earlier item uses
the shared budget, so continue that item separately. UTF-8 characters are not
split at the page boundary. Even small files use the structured result. The
guarded filesystem read and its permission checks remain in effect; this is a
provider-visible output bound, not a filesystem memory bound.

### Reading image files

`read` recognises PNG and JPEG files by their bytes, not their extensions. It
validates them with the same image type as prompt attachments and sends them as
user image blocks after the tool answer. The JSON item contains `image_index`
(zero-based within this result), `media_type`, `width`, `height` and a null
`next_offset`, not binary `content`. Image offsets must be zero. Image bytes
have a separate 64 MiB total limit per read call; the 32 KiB text limit remains
in effect. Invalid images are reported as errors for their items. Image bytes
use the session blob store, so replay does not read the original file again.
The selected provider and model must support image input.

### Reading PDF files

`read` recognises a PDF by its first bytes (`%PDF-`), not by its extension. The
item contains the extracted text, for every provider; the same per-file line and
shared serialized-result bounds apply, and a continuation `offset` counts bytes of the extracted
text. Limits: a PDF file over 20 MiB (20971520 bytes) is refused with its size
and the limit; only the first 100 pages are extracted, and a trailing note names
the page total when pages are left out or a page has no extractable text. A
malformed PDF is reported as an error for that item. Scanned pages without a text layer give no
text. A PDF is not sent to the provider as a native document: PDF results contain extracted text only.

When a session is reopened after a process or host crash, attempts without a
durable settlement are recorded as interrupted (`Aborted`, `PossiblySent`).
A notice names each interrupted attempt and warns that its request may have
reached the provider. Ego does not replay that request automatically. The next
explicit user request can run in the same session. Recorded usage is retained;
missing usage remains unknown, not free. This also applies to ACP continuation
and clean-session compaction. Live attempts in the current process are not
interrupted by opening another turn coordinator over the same journal.

Durable CLI interactive sessions, including TUIC-managed terminal sessions,
automatically invoke the existing clean-session compaction operation at
`compaction.threshold` (default 90%) of the last measured provider context window. An ongoing tool turn yields before
another generation at that threshold. At the next idle boundary the host seeds
a new durable session, activates it, and continues from the checkpoint without
adding a synthetic human prompt. No source conversation tail is copied. Fresh
instructions, tool definitions, accounting, and auditable lineage use the same
transition as `/compact`.

Pending prompts, questions, or live attempts prevent the transition. A typed
provider context-limit error also schedules compaction, even when the provider
has not reported usage. Unknown occupancy remains unknown; it is not estimated.
Compaction fits its own bounded summarizer request independently of the source
occupancy, so recovery can start after exhaustion. The threshold is not an exact
fit guarantee for unsent growth. With the default, a 272000-token window triggers at 244800
measured tokens.

A failed checkpoint keeps the interactive source open with its failure cause and
an explicit `/compact` retry instruction. The shared provider retry policy owns
transient retries during preparation. After preparation fails, unchanged pressure
does not automatically restart it. A fresh ordinary prompt re-arms recovery;
manual `/compact` also retries it. Operator cancellation keeps the source open.
Quitting after a context refusal exits normally. Durable source history is
retained when recovery fails. A successor turn that is cut at the threshold again
after it measured usage compacts again and continues, as a headless run does; one
that overflows before it measured any usage keeps the overflow, and one that
finishes above the threshold waits for input. `/clear`, `/new` and `/fork` start
without the source's pending recovery. When the failed or
refused recovery belongs to a turn that yielded at the context limit, the task
cannot continue without input: on TUICommander the session sends OSC
7770 `error=context_overflow` and `state=error`, and keeps `error` until the
next turn starts, instead of reporting idle.

Headless `ego run` and `ego resume` use the same policy, threshold and
clean-session compaction, but only when you turn it on: `compaction.headless`
is off by default, set with the `compaction.headless` row in `ego setup` or
`ego config set compaction.headless=true`; only the user configuration sets it.
Off, a turn that ends with `context_overflow` stops the run with exit 4. On, when a turn ends with `context_overflow` (the
`compaction.threshold` yield before another tool generation, a known-full window, or a typed provider
context-limit error), the run compacts the source and continues in the
successor from its checkpoint, in the same process. The run budget, deadline
and `--max-turns` cover both sessions; the checkpoint generation is charged to
the source. The event stream carries
`{"type":"session_compacted","source":"…","source_seq":N,"target":"…"}`,
followed by the successor's `session_started`; the final summary names the
successor in `session`. A failed compaction keeps the typed `context_overflow`
settlement (exit 4) and names `ego compact SOURCE` as the retry. A successor
that overflows again before it reports any usage is not compacted again.
The `ci` profile keeps explicit recovery: its sanitized bundle covers one
session. A session with session roots compacts like any other: the successor
journals the same session roots in its seed, so it needs no new approval.

ACP keeps explicit recovery because it has no standard session-transition
contract: the client would keep prompting the source. Ephemeral sources cannot
compact.
Human terminal sessions use the same automatic policy as managed terminals, so
recovery does not depend on detecting a terminal's operator.

Durable CLI interactive sessions can also compact an idle session shortly
before its prompt cache expires. This is opt-in: turn it on in `ego setup` or
with `ego config set compaction.idle=true`. Compaction reads the full context;
while the cache is warm those reads cost the cache-read price, and after expiry
the next turn writes the full context again. If the operator never returns, the
compaction cost is spent and detail is dropped; that is why it is off by
default. The deadline is the session cache countdown: a TTL that ego sent, or
`cache_ttl_seconds` of the active profile for a provider-managed TTL, or the
session-local three-cycle estimate after confirmed reuse. Without a
known deadline (`cache=unknown`) this compaction never runs. It uses the same
transition as `/compact` without the confirmation prompt, and no task continues
after it: the successor waits for the operator.

```toml
[compaction]
threshold = 90             # default; percent (1-99) at which a turn compacts
idle = true                # default false
idle_margin_seconds = 30   # default; start this long before expiry
idle_threshold = 50        # default; percent (1-99) of the measured window
```

`threshold` and `idle_threshold` trade cost against detail. A lower value
compacts earlier: it keeps the context small and cheap to read again, but drops
more detail and spends a summarizer request sooner. A higher value keeps detail
longer but lets each cold-cache turn write a larger context. `ego config`
refuses a value outside 1-99 and an `idle_threshold` above `threshold`, so
lowering `threshold` below the default `idle_threshold` of 50 needs both keys.

Only user and system configuration set `[compaction]`; each field keeps lower
authority when unset. The compaction starts at `expires_at -
idle_margin_seconds` when the composer is idle: no queued prompt, pending
question, live request, pending interrupt, running background command, or
undelivered background agent, and the deadline belongs to the selected model. It does not run when the measured context is below
`idle_threshold` percent of the measured window or unknown, when the margin is
not shorter than the TTL, or after the deadline. Each deadline gets at most one
attempt. A failed attempt keeps the source open and waits for fresh operator
work, like pressure recovery. A 300-second TTL requested at 12:00:00 compacts
at 12:04:30; a 150000-token session in a 200000-token window qualifies, a
90000-token session does not. ACP and one-shot headless runs do not compact on
cache expiry.

### Request output and response spend

`ego run` and `ego compact` accept `--max-output-tokens N`, where `N` is a
positive integer. The same `max_output_tokens` key is accepted in configuration
and named profiles. Each request uses the smaller of the configured output cap
and the model output maximum. Without a cap, ego uses the model output maximum,
not the context window. System output caps cannot be relaxed by lower layers.
These output controls are provider request limits, not monetary reservations.
`ego compact --budget LIMIT` uses the same spend grammar and gate as `run`.

A monetary budget checks only recorded response spend. It admits while that
spend is less than or equal to the cap, then refuses the next request once spend
exceeds it. Actual provider cost, or actual usage priced against the captured
quote, is added after each response. There is no full-context or maximum-output
monetary hold and no reservation setting or compatibility flag.

A sequential stream can overshoot by one request. Concurrent parent and child
requests share the gate without serialization: requests already in flight when
the cap is crossed may still settle, but no new request is admitted afterward.
This is not a strict invoice ceiling.

Missing, offline, or unusable prices do not refuse generation. Unknown actual
cost stays unknown and adds no invented estimate to recorded spend. One durable
`cost_anomaly` event per session/model records that the actual cost is unknown.
Its optional `cause` is a closed, credential-safe source-failure category with
a recovery action. JSONL and ACP retain the event. Human text prints a cap
warning only when a monetary ceiling was configured; without one, the
statusline can still state `unpriced` without interrupting the turn.

### Checked session goals (2026-10-15)

An active goal drives work after each successfully completed turn. An independent,
tool-less small-model generation checks the objective against session evidence.
This generation uses the normal session budget gate, usage accounting and response
evidence store. A passing check records `check` attribution and evidence, including
for operator-owned goals. `/goal achieve` remains an operator attestation.

An unmet condition admits a durable `goal_continuation` event, never an
operator prompt. It carries only the checker's reason, not a copy of the objective.
History projection renders every such event, including older ones that recorded
the objective, as a user-role message at its chronological position that is framed
as automatic runtime input, not operator input or permission. The active objective
stays in exactly one leading system block, so a continuation does not change that
prefix. Five consecutive observations of the same normalized condition
block the goal. A changed condition resets that count. After two consecutive
continuation turns with no tool call and no new inbound event, ego waits for the
next inbound event instead of issuing another model turn or goal check. A call
whose admitted effect is a read-only inspection of one session process (the
built-in `exec_status` and `exec_monitor`) is not a tool call for this purpose;
its journaled result records that effect, and a record without it counts as
progress. An admitted inspection that fails still records its admitted effect;
a refused or unstarted call has no inspection exemption. Ready monitored output,
process settlement and owned-task completion are collected before the check, so
they reset the streak. An `exec_status` result also records the stdout and
stderr byte totals it read. A read whose totals exceed those of the previous
read of the same process (or the first read that saw any output) is new output
and resets the streak; the same output read again does not. The waiting receipt
names the no-progress reason; the goal remains active and unmet.
Operator input, host mail and background completion notifications reset this
streak through their normal inbound delivery. There is no default
turn allowance. `--goal-turns N` sets an explicit finite allowance when checks
first start; N can be zero to stop before automatic continuation. An allowance already stored in a
historical session remains finite, including an indistinguishable historical
20-turn default. Its provenance cannot be recovered from the stored number.
An omitted `--goal-turns` does not remove a saved allowance. There is no goal
extension command; replace the goal with a new operator objective to start a
fresh identity. The allowance and consumed count survive resume. Internal
continuations count toward `--max-turns`. Interactive `/goal TEXT` (or `/goal set TEXT`)
sets an operator-owned objective and starts its first work turn. `ego run --goal TEXT`
(also `ego resume ID --goal TEXT` and `ego continue ID --goal TEXT`)
sets an operator-owned objective. The `goal` tool creates or cancels a model-authored
objective, but cannot rewrite or replace any active goal. Text output shows a
`Goal (operator|automated): OBJECTIVE` receipt when a goal first appears and when
a new model-authored goal is created. Each automatic turn shows `Goal turn n: CONDITION`
without an allowance, or `Goal turn n of N: CONDITION` with an explicit or
historically saved allowance, before its work output. A goal-check stop shows the objective, turns used, stop
reason, last unmet condition and next actions: `/goal show`, `/goal achieve`,
`/goal block`. `--goal-turns N` only sets the initial allowance; it does not
change an existing one. These receipts remain visible with `--quiet`;
headless text sends them to stderr, not answer stdout. JSON/JSONL and ACP keep
their existing structured contracts.

The native status row includes `goal n` for an unlimited active goal, or
`goal n/N` for a saved finite allowance, including when a check stop retains
the active goal. The segment uses the existing
width-bounded renderer; a disabled status row stays disabled. Achieved and
blocked goals do not retain this active segment.

Provider, permission, pause, cancellation and budget stops do not admit automatic
work. Invalid checks and exhausted allowances emit a visible diagnostic and retain the active goal;
they never claim achievement. ACP session objective metadata exposes `goalStatus`,
`goalStop`, and `goalContinuationTurns` separately from work-turn completion.

`--goal-check-command '["/path/to/check", "arg"]'` selects a command evaluator
instead of a model. It receives objective and session evidence JSON on stdin
(the same text a model check reads). Opaque reasoning continuation state is omitted
from this evidence; ordinary model replay retains it unchanged. Readable assistant
text and tool results remain available to the checker. Session evidence never contains image bytes:
each image is the object
`{"image_evidence":{"reference":"SHA256:BYTES","media_type":"png","width":W,"height":H,"available_to_checker":false}}`,
where `reference` names the session-owned blob. Ego builds the descriptor from
the recorded image metadata and does not read the blob, so a missing image blob
gives the same descriptor and does not stop the check. The reference is informational;
ego never reads a reference that a checker returns. A descriptor does not show
what the image contains, so a visual criterion that rests only on an image is
unverified and is not evidence of success. It returns only
`{ "verdict": "achieved|continue|needs_operator|waiting", "reason": "evidence or unmet condition" }`,
plus `"image_dependent": false` on `achieved` when the evidence contains an
`image_evidence` descriptor. That field states that the success rests on
independently readable text or tool evidence, not on an image. An `achieved`
verdict over evidence with an image that omits the field, or sets it to `true`,
is not accepted: the goal blocks with the reason
`visual evidence unavailable to the goal check: REASON`, and the operator
decides. Audio follows the same rule: each clip is the object
`{"audio_evidence":{"reference":"SHA256:BYTES","media_type":"wav","available_to_checker":false}}`,
whose size does not depend on the clip length. A descriptor does not show what was
said, so an `achieved` verdict over evidence with audio needs `"audio_dependent": false`;
otherwise the goal blocks with the reason
`audio evidence unavailable to the goal check: REASON`.
Evidence without images or audio keeps the two-field contract.
The `verdict` is required and must be exactly one of those four values; the old
`achieved` boolean is not accepted. `achieved` requires evidence of success.
`continue` admits automatic work only when the agent can perform a useful,
authorized next action; its reason should identify that action. One item awaiting
login, approval, a decision or other operator input does not prevent continuing
independent authorized work. It does not authorize acting on the blocked item.
When no authorized useful action remains, `needs_operator` names the required
owner action, blocks the goal at once, records the reason and shows an actionable
stop receipt without consuming an automatic work turn. A blocked
goal is terminal: after resolving the blocker, `/goal set TEXT` starts a new
goal. The stop receipt states this; `/goal achieve` and `/goal block` require an
active goal.
`waiting` applies when no useful authorized action remains now and the next useful
action depends on already in-flight non-human work or an external event. Its reason
names that dependency. It records a stop receipt, keeps the goal active and starts
no automatic continuation turn. An inbound event can resume work, and the next
check can achieve the goal, continue productive work, wait again or request human
action. A mailbox or status poll with no new useful evidence is not independent
work while the dependency is pending. Independent useful authorized work still
requires `continue`.
Existing command evaluators can return the original three verdicts with unchanged
semantics and stdin. A command that always returns `continue` does not gain waiting
behavior automatically. Waiting depends on the evaluator's decision; it does not
classify foreign tool calls as productive or unproductive. The existing
two-empty-continuation idle guard remains unchanged. A premature waiting decision
can leave a goal idle until another inbound event; it does not arrange delivery
of that event. Model checks use the same contract and receive these instructions. The command writes
its verdict on stdout. Its exact exec authority must be granted by ordinary
policy; it uses the same confinement and cancellation scope, with a 60-second timeout. Command
checks do not incur model cost. Model checks remain accounted generations.
Stderr is not part of the verdict. If the command exits unsuccessfully, the
visible goal-check diagnostic includes at most 4096 UTF-8 bytes from the end of
its bounded stderr preview. An exec allow rule does not grant filesystem access:
the evaluator script and its inputs must be readable under the run confinement.


Usage report v6 is the current closed contract: its role lines are `main`,
`judge`, `goal_check`, `compaction`, `subagent`, `hook`, and `total`.
`goal_check` is required in v6 reports, including zero usage; no legacy-report
decoder or compatibility default is provided.
Cost `known` and `unknown` describe monetary spend only. Display-only standard
API estimates are separate `subscription_equivalent` totals by currency, with
missing or overflowing estimates in `subscription_unknown`. The text view labels
them `subscription-equivalent (standard API)` and never presents subscription
accounting zero as known monetary spend.

Usage JSON retains optional observed limits. A session can contain
`context_capacity: {tokens, source, observed_at}` from its latest
`ContextWindowUpdated` event. The source is `context_window_updated` and the
observation time is UTC epoch milliseconds. A missing or zero capacity is
omitted; the command does not look up a denominator in a different model's
catalogue or derive one from billed tokens.

Each session also has `context_attribution`, shared by `ego usage`, `--json` and
`/usage`. It ranks tools by estimated request-argument and model-visible result
text tokens. `source=durable_tool_events`, `provenance=utf8_bytes_div_4_ceil` and
`scope=local_session_requests_in_report_period` identify this local observation.
The estimate rounds each payload's UTF-8 byte length up at four bytes per token.
It excludes protocol framing, non-tool context and repeated prompt input; it is
not provider occupancy or billing, and does not change history or accounting.

Tool rows contain `request_estimated_tokens`, `result_estimated_tokens`,
`duplicate_read_estimated_tokens` and per-call `calls`. Each call includes `item`,
`request_seq`, `request_bytes`, `request_estimated_tokens`, and an optional
`result` with source `seq`, `bytes`, `estimated_tokens`, builtin/foreign
`provenance`, `ok` and `duplicate_of_request_seq`. Missing results remain null.
Unmatched completions are counted in `unmatched_results` rather than assigned
to a guessed tool. A period selects requests by their timestamp; earlier successful
reads can still supply a duplicate source link. Future events are excluded.

Duplicate reads require successful builtin `read`, `glob`, `grep`,
`read_tool_result` or `web_fetch` calls with equal parsed JSON arguments and equal
result text. Changed results, errors and other tools do not qualify. A duplicate
estimate includes that call's request and result and is a subset of the totals,
not extra tokens. It identifies repeated evidence, not a call that is necessarily
safe to skip; no call is suppressed.

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

`ego rm --expired --yes [--session-dir PATH]` runs session retention now. It
permanently removes every session that nobody opened or wrote for
`sessions.retention_days` days (30 by default), with its rows in
`history.sqlite`, then compacts that file. A session that is open, held, or
resumed within the period stays, and so does every session that a kept fork
or compacted session starts from. It prints each deleted session and the space
freed. The same pass runs in the background at start-up of `ego` and
`ego run`, at most once a day, and logs what it deletes.


## Markdown custom commands

Markdown files under `commands/` in the shared capability roots define `/name`
prompts. Project namespaces are `.ego`, `.agents`, `.claude`, then `.opencode`;
nearest trusted project wins over ancestors, user roots, and `/etc/ego/commands`.
User roots include `$EGO_HOME/commands`, `~/.agents/commands`,
`~/.claude/commands`, `~/.opencode/commands`, and `~/.config/opencode/commands`.
A nested `git/review.md` is `/git:review`. Built-ins and skill-name collisions
are refused visibly rather than shadowed.

`$ARGUMENTS` inserts raw arguments; `$1`, `$2`, and later positions use shell-like
argument grouping. `$@`, `${1:-default}`, `${@:-default}`, `${@:N}` and `${@:N:L}`
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
See [custom Markdown commands](docs/integrations/skills.md#custom-markdown-commands)
for usage. Ego publishes the resolved prompt as one unit. It cannot roll back
side effects from shell commands that already ran during expansion.

### MCP Tasks for long tool calls

MCP Tasks are off. Turn them on with the `mcp_tasks` row in `ego setup` or
`ego config set mcp_tasks=true`; only the user configuration sets it. A project or
local file that sets `mcp_tasks` is refused.

With the option on, an interactive human session declares the
`io.modelcontextprotocol/tasks` extension to HTTP MCP servers. A server may then
answer a `tools/call` with a task instead of a result. Ego answers the model's call
at once with the task id, the server and the tool, and the turn goes on. Ego asks the
server `tasks/get` for each live task when a generation starts, no more often than
every 500 ms and no less often than every 30 s (the server's suggested interval
sets the pace), and gives the model each result once, as context, when the task
completes, fails, is cancelled, expires or cannot be polled. While the session
waits at its prompt, ego also polls each live task when it is due; when one ends,
one generation starts on its own and carries the result, and a poll that finds the
task still working starts none.

Each poll runs under the grant that let the original call run, so a call you
approved is not asked about again at every poll. Ego polls and cancels only task
ids that its own call recorded; the model cannot name one. When a supported
form is ready, Ego presents it at the idle terminal and sends the accepted
answer to the original task. Other tasks can settle while the form is open.
Declined or unsupported input triggers a cancellation request. If the server
fails to acknowledge an update, Ego does not resend the answer and reports
that the server may have received it.

Tasks end with the session: when it ends (exit, `/new`, resume, fork or compaction)
ego asks each server to cancel every live task of that session, once, and `ego resume`
does not resume them. The model's receipt says so, so after a resume it treats the task as lost. Stdio and ACP servers never get the extension (each call
there starts its own server, so the task would end before the first poll), and
headless `ego run` and `ego acp` do not use it.

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

Checkpoints are on by default. Turn them off with the `checkpoints` row in `ego setup`,
`ego config set checkpoints=false`, or `--no-checkpoints` for one run. Only the user
configuration and the command line set the key; a project or local file that sets it is
refused. With checkpoints off, `write` and `edit` create no Git object and no ref, and
each result says that checkpoints are off, so nothing can be restored for those edits.

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

### Exact text edits

The builtin `edit` accepts `files`, an array of 1–16 distinct existing UTF-8
files. Each file has a `path`, 1–16 `chunks` with `old_string` and
`new_string`, and an optional `replace_all` flag:

```json
{"files":[{"path":"src/example.txt","chunks":[{"old_string":"before","new_string":"after"}]}]}
```

Each chunk matches the original file and its span must not overlap another
chunk. By default, each old string must occur exactly once. `replace_all=true`
requires one chunk and replaces all non-overlapping occurrences of its nonempty
old string. All reads and writes need admission before execution. Execution
stops on the first failure; earlier file changes are not rolled back.

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

- The published notebook-edit tool contract of another agent CLI:
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
Each command receives a newline-terminated JSON document on stdin with the same
`rate_limits` array as the v6 usage report. It contains no token or account email.
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

### Explicit provider HTTPS proxy

`ego run --proxy http://127.0.0.1:8080 "prompt"` uses an explicitly selected HTTP CONNECT proxy for provider HTTPS requests. The same options apply to interactive `ego` and `ego resume`; specify them again when resuming. `--proxy-ca FILE` adds one PEM CA certificate (at most 64 KiB) to this invocation's provider TLS trust, for example `ego --proxy http://127.0.0.1:8080 --proxy-ca ./inspection-ca.pem`. Certificate verification remains enabled.

The endpoint must use `http://` with no credentials, path, query, or fragment (default port 80). Local provider DNS admission must still succeed. The proxy socket is resolved and pinned through the run resolver. CONNECT retains the logical provider authority, Host, and TLS SNI. The explicitly trusted proxy controls downstream DNS and connection routing. A CA permits that proxy to inspect inference content and provider credentials.

Ambient `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, and `NO_PROXY` values do not select or bypass this proxy. There is no direct fallback on proxy failure. Offline mode refuses proxy use, including loopback proxies. HTTP provider endpoints and WebSocket inference are refused when `--proxy` is selected. These options do not proxy MCP, OAuth login, attachment downloads, served protocol hosts, or standalone commands such as `ego models` and `ego compact`.

### ACP child activity

ACP v1 clients can opt in to `clientCapabilities._meta.ego.childActivity.version=1`.
The agent then sends native child start, settlement, and roster retraction in
`session_info_update._meta.ego.childActivity`. Load/resume sends a current durable
child snapshot rather than replaying historical starts. Ordinary clients retain
the existing projection. This metadata does not create a human wait or a PTY.
See [ACP extensions](docs/integrations/acp-extensions.md#child-activity-negotiated-version-1).

### Subscription quota observations

`ego usage --json` emits version 6 with top-level `rate_limits`, including an
empty array when no current credential has an observation. Each entry contains
`provider`, opaque `credential`, `windows` (`name`, `used_percentage`, Unix-second
`resets_at`, `window_seconds`), `plan`, `credits`, Unix-millisecond `observed_at`,
and `source` (`passive` or `active`). Named-account entries also contain
`connection`, the stable account ID. Their `provider` is the configured vendor,
not the account ID or the OAuth adapter route: an OpenAI named account emits
`{"provider":"openai","connection":"account_b",...}`. Legacy entries keep their
existing provider route and omit `connection`. Active and passive observations
use the same identity. Quota is not token context occupancy or cost.

A session's `manifest.json` also projects optional `connection` from its durable
selection journal. It never infers an account from a display name or the current
global default. A full session selection snapshot replaces or clears the account;
a sparse run transaction without an account inherits the last selection. Fork
and compaction targets publish their own inherited account selection. Legacy or
unbound sessions omit the field. A reader must not substitute a legacy vendor
credential when this field identifies a named account.

Response observations are shared by all processes using one `EGO_HOME`, and the
newest observation wins per credential. Status templates can iterate
`rate_limits`; the configured `rate_limits` segment renders the selected
provider's windows. Command status providers receive the same array on stdin.
ACP publishes it under `_meta.ego.rate_limits` in `session_info_update`.

DEFERRED (2026-11-12): xAI quota has no recorded live response because the
available access credential expired. No xAI quota fields are inferred.

### Idle subscription quota reads

Run `ego auth usage --json` for all saved OAuth accounts, or
`ego auth usage anthropic --json` for one provider. This reads the recorded
non-generating usage endpoint with ego's own access token. It never invokes a
provider program, generates a message, refreshes a token, or signs in.

The output has `rate_limits` in the same v6 report shape and `errors` for explicit
provider issues (`auth_invalid`, `backoff_until`, or `unavailable`). An expired
access token reports `sign in again` without constructing the transport. The
error `retry_at`, when present, is Unix milliseconds. Empty or unsupported
credentials do not cause fallback reads of another program's account.

Use `ego auth usage --connection account_b --json` for a named account. Results
and errors carry `connection: "account_b"` and the configured vendor in `provider`;
provider selection filters by the configured vendor. If a requested named account has no configuration, its error retains
`connection` but omits `provider`: the vendor is unknown, not guessed from the ID.
Configured key accounts report `unavailable`; missing or expired OAuth credentials
report `auth_invalid` without borrowing another account. No secret contents or
editable account labels appear in this machine identity.

All processes on one `EGO_HOME` share a 300-second read reservation per access
credential. A fresh passive snapshot suppresses idle reads. Concurrent cold
followers return the previous snapshot, or an explicit reservation issue when
none exists; they do not start a second read. A 429 keeps the prior value and
honors Retry-After (seconds or HTTP date), never earlier than the normal cadence.
A 401/403 suppresses reads for that token until its value changes. Failed reads
never erase a prior observation. Network-off configuration sends no request.

Windows are classified by duration: 3–8 hours is `five_hour`, 5–9 days is
`seven_day`, 27–32 days is `monthly`; other durations retain a secondary label.
The idle read updates the same shared snapshots used by status and ACP.

### Native commands during active work

Native inspections are serviced by the host while a reply or goal check is running:
`/goal show`, `/name show`, `/hooks` (including details), `/skills` (including
`details` and `diagnose`), `/tools` (including details), `/mcp` inventory, `/pin`
listing, `/context`, `/doctor`, `/config`, `/usage` and `/tasks` inspection.
They do not become model prompts. Native mode staging, fast-tier selection,
future-session `/config set` edits and scoped `/tasks cancel ID` are also serviced
without waiting for the reply. These controls retain their validation and do not
rewrite an already admitted request. `/goal stop` pauses automatic continuation, not the
admitted reply, tool or child. Permission and question answers keep their own
input owner; an admitted inspection never answers those prompts. Catalogue
replacement, authentication, session transitions and context mutations retain
their effect-specific admission and do not change an already captured request.

The registry's native actions are classified by effect, not by their leading slash.
The same classification applies to typed commands, menus and shortcuts; opening a
second modal never steals an outstanding permission or question decision.

| Command/action forms | Effect and busy-time boundary |
| --- | --- |
| Help, Expand, Version | Local terminal output; no model request. |
| Context, Doctor, Usage, Config inspection/details | Host inspection of the current session/configuration/perimeter. Doctor probes do not block the active model drive. |
| Hooks; Skills list/details/diagnose; Tools list/details; MCP inventory | Display available catalogues without executing a hook/tool or replacing captured turn authority. Bare `/mcp` also inspects inventory while a permission or question modal remains open; it does not answer the modal. |
| Name show; Goal show | Inspect durable session metadata. |
| Goal stop | Cancel goal continuation without killing the admitted reply, tools or children or answering a modal. |
| Mode, Plan, Yolo | Stage supported mode selection for a future generation; do not reauthorize a tool already awaiting a decision. |
| Fast on/off | Validate and select the session's tier for future requests; current provider bytes are unchanged. |
| Config set | Save future-session defaults; the current session's captured configuration stays unchanged. |
| Tasks list/details/ID | Inspect only the session's admitted task/exec identities. |
| Tasks cancel ID | Explicit cancellation through the session-owned service; not arbitrary process termination. Nonempty task operations do not cross an open permission/question modal. |
| Pin list | Read durable pinned constraints. |
| Pin add, Unpin | Change context constraints only at admitted pending-work boundaries. |
| Side | Separate model question using captured turn context, with its own admission and no tools. |
| Retry | Expedite existing recovery; does not authorize another independent request. |
| Statusline | Terminal-owned display configuration; preserve its modal and draft owner. |
| Model, Thinking | During an active reply, select a future-generation model/effort through the existing account checks, pre/post hooks, optional saved defaults and durable model-switch owner. The captured request stays unchanged. A turn interrupt does not cancel an accepted switch; run cancellation remains authoritative. Other queued native commands wait for that lifecycle to settle; modal-open selection stays refused. Account changes require fresh session admission. |
| Permissions | List and Explain read the admitted rules during an active reply. Explicit Allow, Deny and Revoke publish atomically and stage policy for the next generation without changing captured turn authority. Remember retains its confirmation-modal admission; lifecycle effects retain separate admission. Root revocation retains its dedicated control path. |
| Name set/clear | Publish title-only changes during an active reply through the leased compare-and-append owner. Stale-sequence and uncertain-publication results remain explicit; no guessed-sequence retry or model prompt. |
| Goal set/resume/achieve/block | Durable goal mutations retain their session and pending-work admission; active goal identity/text is not silently replaced. |
| Skills invoke | Context/tool-producing invocation, not catalogue inspection. |
| Skills rescan | Catalogue replacement; retain captured-generation/resource-root boundaries. |
| MCP login/toggle/add/context | Authentication, registry publication or admitted foreign context execution; not inventory. |
| Login, Setup | Native credential/configuration workflows needing their own modal owner; no implicit identity rebinding. |
| Init | Deterministic instruction proposal and guarded write with confirmation; not a model prompt. Busy Init/Setup shortcuts show the same explicit refusal as their typed forms. |
| Export | Render sanitized or local Markdown/JSON/JSONL during an active reply from the existing committed-prefix reader. Output goes to this transcript, with no file write or follow mode; an unfinished reply is not included. Existing sanitizer policy and modal admission remain unchanged. |
| Sessions, history, Resume | Session browser/selection can activate another session; it is not a transcript-only read. Preserve writer and root admission. |
| New, Clear, Fork, Compact, Refresh | Session/context/instruction transitions retain explicit pending-work, capture and writer boundaries. A request does not create a second in-flight compaction. |
| Quit | Terminal/session lifecycle control with draft/queue confirmation and cleanup. |

Model-busy availability does not bypass effect authorization. Some lifecycle/modal
workflows still require an idle owner; receiving a command is not evidence that its
mutation has completed. Backgrounding foreground exec, interrupt, draft stash/recall,
image admission and accepted prompt submission use their separate native owner paths.
Shell escapes require an idle terminal.
