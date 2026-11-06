# Configuration reference

Configuration selects defaults and boundaries. It does not turn project content
into operator authority, grant a credential, or prove that a sandbox is available.
Unknown keys and invalid types fail with their source; they are not ignored.

## Files, precedence, and trust

| File/source | Role |
| --- | --- |
| `/etc/ego/config.toml` | System defaults and non-relaxable ceilings |
| `$EGO_HOME/config.toml` (default `~/.ego/config.toml`) | Operator defaults and named profiles |
| `<workspace>/.ego/config.toml` | Committed project defaults, after trust |
| `<workspace>/.ego/config.local.toml` | Local project defaults, after trust; keep it out of Git |
| `EGO_MODEL`, `EGO_PROFILE` | Invocation environment selection |
| CLI flags and `--set K=TOML_VALUE` | Invocation overrides, under system ceilings |

Precedence is flags > environment > project-local > project > user > system.
An executable agent file contributes its own invocation configuration; see
[automation](../guides/automation.md). Selected profile fields overlay their
own layer. Headless/untrusted/bare resolution discards project layers and ambient
instructions. `--instructions FILE` explicitly selects instructions for headless
work; it does not trust project configuration.

`ego config ls` shows stored managed values. `ego config ls --effective` shows
current untrusted perimeter resolution. `/config` shows the interactive session's
captured settings; `/config perimeter` shows its admitted roots and confinement.
User `mode` and `sandbox` values apply without a source-selection prompt.
Omitting the corresponding flags retains those values, even `yolo` and `off`.
Saving a default affects future sessions. Use live `/model`, `/thinking`, or
`/mode` controls to change current session selections.

## Destination policy and declared profiles

`ego config ls --effective` (also `--json`), `ego doctor`, and the session
`/config perimeter` view include `destination_policy`. This is a read-only
snapshot of the admitted startup `net(...)` declarations, not a second policy
engine. It shows allow and deny predicates, their authoritative source, other
sources that carry the same rule, and the selected profile. It excludes
untrusted or discarded project declarations. The selected profile is context;
a rule's source names its config layer. Use `ego rules ls` and `ego rules explain`
for current rule authority after live rule changes. Saved profile edits require
a new session.

Declare a destination grant profile with the existing permissions vocabulary:

```toml
[profile.docs]
network = "on"
mode = "default"

[profile.docs.permissions]
allow = ["net(docs.example.com:443)", "net(*.docs.example.com:443)"]
deny = ["net(blocked.docs.example.com)"]
```

Select it with `--profile docs` or `EGO_PROFILE=docs`. A wildcard subdomain
predicate does not include the apex domain, so the example declares both.
Profile permissions replace the layer's whole permission block; system denies
still bind. A deny wins over an allow. **This grant list is not a default-deny
firewall.** Uncovered destinations still follow mode and approval policy.
Do not add `deny = ["net(*)"]` as an allowlist fallback: that also denies the
listed destinations. `network="off"` or `--offline` remains a separate boundary
and cannot be lifted by a destination grant.

The view explains these enforcement boundaries:

- Guarded tool HTTP effects use `net(...)` authorization. These rules do not
  describe every ego HTTP client, provider endpoint, or a remote server's
  downstream activity.
- Each guarded effect uses fresh DNS evidence bound to its logical host and
  selected socket address. Literal addresses skip DNS. Redirects are one hop
  per authorization; the next destination needs fresh authorization and DNS
  validation, not the first destination's grant.
- Native web requests reject special-use literal addresses and DNS answers on
  each hop. A user `net(...)` allow that names hosts (a name, a `*.` pattern,
  an address or a CIDR block such as `net(192.168.1.0/24)`) admits loopback,
  LAN and shared-space addresses for the matching destinations, and
  `web_private_network = true` admits them for every call. Bare `net(*)` and
  project rules do not. See [tools](../guides/tools.md). An authorized foreign
  HTTP effect can use private destinations, including a local MCP endpoint.
- Destination rules do not filter child-process sockets. Child offline
  isolation needs a measured backend `offline` capability. Inspection reports
  `not_checked` until there is probe evidence; this is not a guarantee.
- macOS uses Seatbelt. Linux offline requires a supported namespace backend
  and successful capability probes. Unsupported enforcement fails closed.
  Sandbox `off` alone does not provide network isolation.

The shared view does not run DNS, open sockets, probe child processes, or make
provider calls. Existing effect-level checks remain in the guarded transport;
capability evidence remains in the execution backend.

## Every config.toml key

The source schema for base/profile, MCP, auth, and UI fields is
[layer.rs](../../crates/ego-core/src/config/layer.rs). Resolution and inheritance
are in [resolve.rs](../../crates/ego-core/src/config/resolve.rs). “Absent” below
means this layer supplies no override; defaults are the final behavior when no
higher or lower layer supplies a value.

| Key | Accepted value | Default / applicability |
| --- | --- | --- |
| `model` | Full `provider/model` string | No fixed model; interactive first-run selection, explicit/configured model in headless |
| `max_output_tokens` | Positive integer | 32,768 generated tokens per request by default, capped by the model maximum, or by half the context window when the model declares no maximum; an explicit value replaces the default; system limits cannot be relaxed by lower layers |
| `thinking` | `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max` | Model-declared default; unknown requires explicit selection |
| `mode` | `plan`, `default`, `auto`, `accept-edits`, `yolo` (legacy input alias: `edits`) | Trusted interactive with no configured mode uses `auto`; ordinary unresolved policy uses `default` |
| `sandbox` | `ro`, `workspace`, `adaptive`, `off` | `adaptive` when absent on macOS, `workspace` elsewhere; effective inspection reports the platform default reason; explicit selections retain their meaning, system ceilings bind, mode is independent |
| `max_mode` | Same mode vocabulary | Absent = no system maximum; system-only, does not select a mode |
| `roots` | Array of `{path="...", access="read"\|"read-write"}` | Under strict levels, no extra roots are granted by default. `[]` clears configured roots, while adaptive derived roots remain. System declarations cap additional roots |
| `exec_paths` | Array of paths | Empty; extra read/executable roots, under system root bounds |
| `grants` | `{roots=[...], exec_paths=[...], allow=["tool(...)"]}` | Project and local layers only; absent = no proposal. See [Project grant proposals](#project-grant-proposals) |
| `exec.env` | `inherit`, `core`, `allowlist` | `inherit` |
| `exec.env_allowlist` | Array of environment names | Empty; used only with `allowlist` |
| `network` | `on`, `off` | `on`; explicit `--offline` cannot be cleared by this setting |
| `web_private_network` | Boolean | False (block on); user-only. `true` lets `web_fetch` reach every loopback, LAN and shared-space (Tailscale `100.x`) address of an admitted call. Unspecified, broadcast and multicast addresses stay refused. A project or local layer that sets it is a configuration error |
| `permissions.judge` | Full model slug or `none` | Absent prefers a supplied same-provider review model, otherwise the active model; `none` disables advisory generation |
| `permissions.allow` | Array of canonical predicates | Empty contribution |
| `permissions.deny` | Array of canonical predicates | Empty contribution; system denies remain immutable |
| `budget` | String such as `"20k"`, `"400k"`, or `"2.5$"` | No configured ceiling; system money/token limits clamp requests independently |
| `bare` | Boolean | False when trusted; untrusted resolution is bare regardless |
| `checkpoints` | Boolean | True; user/system and command line only. False stops `write` and `edit` from saving private Git checkpoints in the worktree; `--no-checkpoints` does it for one run |
| `tools` | Array of tool names | Unset: every default tool. User/system and command line only; same names as `--tools`, `[]` is `--no-tools`; `--tools` overrides it for one run |
| `cache` | `none`, `short`, `long` | `short`; project and local layers may select `long` only in a trusted workspace; the trust question names the file |
| `compaction.threshold` | Integer 1–99 | 90; user/system only; percent of the measured window at which a turn yields and the session compacts. `compaction.idle_threshold` may not exceed it |
| `compaction.idle` | Boolean | False; user/system only. True compacts an idle interactive session once per known prompt-cache deadline ([CLI.md](../../CLI.md), automatic compaction) |
| `compaction.idle_margin_seconds` | Positive integer | 30; user/system only; seconds before the cache deadline at which idle compaction starts |
| `compaction.idle_threshold` | Integer 1–99 | 50; user/system only; minimum measured context, in percent of the measured window |
| `recap.enabled` | Boolean | False; user/system only (a row in `ego setup`). True shows a short recap before the first interactive prompt after an absence; the recap is one model call, booked on the main model, and is not added to the history ([interactive guide](../guides/interactive.md)) |
| `recap.after_minutes` | Positive integer | 30; user/system only; minutes without a completed turn after which the next prompt gets a recap |
| `task.model` | Model reference | Unset: task children use the parent's model. A named agent's `model` wins ([CLI.md](../../CLI.md), named tasks) |
| `task.effort` | `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max` | Unset: task children use the parent's effort. A named agent's `effort` wins |
| `profile.NAME` | Profile table | No selected profile without `--profile`/`EGO_PROFILE` |
| `auth.PROVIDER` | Credential-command/identity-header table | No override |
| `ui.statusline.template` | MiniJinja template string | Compact default; `{{ prompt }}` disables optional status |
| `ui.statusline.providers.NAME` | `{argv=[...], timeout_ms=250}` | No commands by default; at most four, timeout 1–1000 ms |
| `ui.keybindings` | Map of action names to key-chord arrays | User-only shortcut overrides; an empty array removes an action's shortcuts |
| `ui.vim` | Boolean | False; user-only Vim composer preference, also selected by `--vim` |
| `mcp.servers.NAME` | Complete tagged server entry (optional `enabled = false` detaches it from session start) or tombstone | No configured external server |
| `instructions.shared_root` | Absolute shared ancestor directory; user layer only | Absent: discover from the repository root; grants no sandbox authority |
| `instructions.exclude_roots` | Array of absolute subtree directories; user layer only | Empty; repositories inside an excluded subtree do not inherit shared instructions |
| `instructions.max_files` | Integer ≥ 1; user layer only | 8; caps loaded instruction files and distinct import attempts per run. The 64 KiB per-file and 256 KiB total limits are fixed |
| `hooks` | Ordered array of hook entries | Empty contribution; hooks combine across admitted sources |
| `sessions.compress` | Boolean; user layer only | True; false stops the background cold storage job, and compressed sessions stay readable. `ego setup` has a row for it and `--no-session-compression` stops the job for one run |
| `sessions.compress_after_days` | Positive integer; user layer only | 7; a session with no write for this many days is cold |

`permissions` is a whole-block override, not a merge of individual lists.
System denies and built-in credential-read protection survive replacement.
An explicit user read rule can lift the built-in credential deny. Guarded read
and grep then expose only assignment key names for `.env*` and AWS INI
`credentials`/`config` files. Comments, sections, and values are omitted; grep
matches the projected keys. Other protected formats and malformed assignments
fail closed (read refuses, grep skips). Exec refuses protected paths declared
in argv, including resolved symlink targets, for foreground and background
calls. This check does not analyze dynamic program code or sanitize arbitrary
process output.
Terminal permanent approvals carry a unique revision linked to their session receipt. Publication records pending intent, atomically saves the root, then commits the receipt. Startup admits the root only while that matching committed receipt exists; an interrupted publication grants no access. Removing or changing that configured root invalidates the saved answer; a later addition of the same path requires fresh execution-policy approval when resuming the old session.

The first `ego roots add` writes an explicit list of additional roots. Removing all stored roots leaves an explicit empty list. Use
`ego config unset roots` to restore absence. Extra execution grants require
`exec_paths` in the user/system file or invocation flags; `ego roots` cannot
express them. Use `ego config set 'exec_paths=["/absolute/path"]'` to edit `exec_paths`. Restart or resume the
session to apply changed perimeter settings.

Extra roots use canonical paths. A nested read-only root restricts built-in
writes below a writable root or workspace. macOS Seatbelt also denies native
writes and execution there. Linux Landlock cannot subtract ancestor grants;
ego reports that the writable ancestor wins for exec children without exiting.
The read-only boundary still applies to built-in tools. See
[permissions](../guides/permissions.md).

`exec.env="core"` retains only present PATH, HOME, USER, LOGNAME, SHELL, TMPDIR,
TMP, TEMP, LANG, LC_ALL, LC_CTYPE, SYSTEMROOT, WINDIR, COMSPEC, and PATHEXT.
An empty allowlist keeps no environment variables. This setting affects built-in
exec children, not provider credentials, hook declarations, or MCP environments.

The default status template is `{{ compact }}\n{{ prompt }}`. The prompt must
render once on its own line. MiniJinja `{% filter priority(integer) %}` blocks drop as
whole units in ascending priority order when a row is too wide. Text before the
prompt goes above the buffer, and text after it goes below. Critical operational
state remains visible even if omitted. See [interactive use](../guides/interactive.md)
for terminal controls. The unreleased
`segments` key is no longer accepted.

Status provider commands run once per trusted native interactive prompt epoch,
under the current execution policy and confinement, not while painting. A command
that needs more authority is omitted without asking a model or human. Output must
be one UTF-8 line, at most 1024 bytes, with no stderr or truncation. Failures are
diagnostic notes. Source: [status providers](../../crates/ego-cli/src/status_provider.rs).

Cold session storage: an interactive or headless run starts one background
pass per process over its session store, never on the turn path. The pass
compresses each cold session: `events.jsonl`, `manifest.json` and every
`evidence/*.blob` larger than 4 KiB become zstd files with a `.zst` suffix;
smaller files stay plain because compression saves no disk block. On real
sessions the allocated disk space falls about four times (3.9x). A held lease, a lease heartbeat newer than the threshold, or a
held `events.lock` keeps a session plain. A writer that starts during the pass
wins, and the pass keeps the original files. Each file goes to a temporary file,
fsync and rename before the plain file is removed, so a crash leaves a readable
session. Resume, history, search, evidence and export read compressed sessions
the same as plain ones. Resuming a compressed session restores plain
`events.jsonl` and `manifest.json` before the first new event; evidence blobs
stay compressed. The pass uses one thread, rests three times its work time, runs at nice +10 on
Linux (macOS and Windows keep normal priority, because only a process-wide call
exists there), and
writes only debug log lines. CI runs and ACP hosts do not start it. Older ego
versions cannot read compressed sessions.

## Project grant proposals

A project cannot declare `roots`, `exec_paths` or a `tool(<server>/*)` permission
itself: those keys stay owner-only and fail at load in a project or local layer.
To ask for them, the project writes a `[grants]` table in `.ego/config.toml` or
`.ego/config.local.toml`:

```toml
[grants]
roots = [{ path = "/srv/shared", access = "read-write" }]
exec_paths = ["/opt/tools/bin"]
allow = ["tool(docs/*)"]
```

Only these three keys exist. `roots` paths are absolute or start with `~/`,
`exec_paths` are absolute, and `allow` takes MCP tool rules only (`tool(server/name)`
or `tool(server/*)`). Any other table or key refuses the file. A system, user, env,
agent-file or flags layer cannot carry `[grants]`.

**Approval.** The workspace trust question lists each proposed root, executable
path and MCP grant, then you answer once. The answer is stored in the workspace
trust ledger against the exact text of the proposal, so it is the receipt. An
unchanged proposal is never asked about again. Any added, removed or edited
entry changes the executable surface and asks again, showing what changed.
`--trust-workspace` is the same owner approval given on the command line.

**Effect.** An approved proposal adds to your own roots, `exec_paths` and
allow rules; it never replaces them. Grants from a proposal show the project
layer as their origin. Without trust, under `--bare`, or in headless runs with no
stored answer, the whole project layer is discarded and `/config` names
`grants` among the discarded settings.

**Retained protections.** System root bounds, `max_mode`, sandbox ceilings and
system `deny` rules bind approved grants like any other root or rule; a proposed
root outside a system bound stops the run with the bound's message. Credential
floors, explicit denies and the sandbox still apply.

**Recovery.** To revoke, delete the `[grants]` table or answer no to the next
question. To forget an approval, remove the workspace entry from the trust
ledger under `$EGO_HOME`; the next launch asks again.

## Profiles and managed edits

A profile accepts `model`, `max_output_tokens`, `thinking`, `mode`, `sandbox`, `roots`, `exec`,
`exec_paths`, `network`, `permissions`, `budget`, and `mcp`.
It does not accept base-only `bare`, `cache`, `web_private_network`, `max_mode`, `auth`, `ui`, or `hooks`.
Only `profile.ci` accepts `session_dir` and `artifact_dir`, both absent by default.

The `ci` profile activates explicitly, never from ambient `CI`. It requires
headless run/resume, explicit instructions, a non-interactive prompt/credential,
and pre-existing absolute session/artifact directories with real final components.
Workspace and roots must be pairwise disjoint, including containment. It publishes
one sanitized run artifact per admitted terminal outcome; an admission refusal
publishes none.

The CLI managed key set is intentionally smaller than the full file schema:

| Target | Keys accepted by `config set/get/unset` |
| --- | --- |
| User base | `model`, `thinking`, `mode`, `sandbox`, `roots`, `exec_paths`, `budget`, `bare`, `permissions.judge`, `ui.statusline.template`, `ui.keybindings`, `ui.vim`, `exec.env`, `exec.env_allowlist`, `network`, `web_private_network`, `compaction.idle`, `compaction.idle_margin_seconds`, `compaction.idle_threshold`, `memory`, `checkpoints`, `tools`, `sessions.compress`, `cache`, `instructions.max_files`, `auth.PROVIDER.headers`, `auth.PROVIDER.credential_command` |
| User profile | Same minus `bare`, `web_private_network`, all `ui.*` keys, and both `auth.PROVIDER.*` keys |
| User `ci` profile | Profile keys plus `session_dir`, `artifact_dir` |

Authentication edits replace the selected provider's complete headers map or
credential command. They are base-only. `config ls`, `config get`, and mutation
receipts show `<redacted>` for both fields, including JSON output. Header names
and values use the same validation as file edits; forbidden overrides fail
without publishing a change.

Use `config edit` for other file fields; it selects `VISUAL`, then `EDITOR`,
creates a private sibling candidate, and validates the complete TOML before
atomic publication. It does not evaluate the editor string as shell code.
Concurrent edits or invalid content preserve a named recovery candidate.
Mutations preserve unrelated keys, comments, and order; unchanged operations
publish nothing. Source: [managed vocabulary](../../crates/ego-core/src/config/vocabulary.rs).

```sh
ego config set 'mode="plan"'
ego config get mode
ego config ls --effective --json
ego config unset mode
```

## MCP configuration keys

Each `mcp.servers.NAME` entry is replaced as a whole by a stronger layer;
credentials are stored separately. `removed=true` masks an inherited entry.
An invocation-scoped JSON entry cannot be a tombstone.

| Transport | Keys and defaults |
| --- | --- |
| `stdio` | Required `transport="stdio"`, `command`; `args=[]`, `cwd` absent, `inherit_env=[]` |
| `http` | Required `transport="http"`, `url`; `profile="current"` (or explicit `legacy-sse`), `auth="none"` (or `oauth`), `allow_insecure_loopback=false` |
| Tombstone | `removed=true`, with no transport fields |

HTTP URLs reject embedded credentials. HTTP without TLS needs the explicit
per-server `allow_insecure_loopback = true` opt-in, which admits any named host,
loopback or owner network; it is invalid for HTTPS. See
[plaintext HTTP on the owner network](../integrations/mcp.md#plaintext-http-on-the-owner-network). Explicit inherited variable names
are admission facts, not arbitrary environment values in TOML.
See [MCP](../integrations/mcp.md) for connection and login behavior.

## Authentication keys

`auth.PROVIDER.credential_command` is an optional program for login/refresh;
its stdout carries a secret result. `auth.PROVIDER.credential_timeout_secs`
(1–3600, default 120) bounds each call, so a slow browser login can finish; the
program and its descendants are still stopped on timeout, cancellation or the run
deadline. When the program fails, ego shows its exit code and up to 4 KiB of its
stderr. The request it received, its own stdout and credential-shaped values are
masked; other text it prints is shown as written, so a helper must not print
secrets it invented. Remove the key to restore the default. `auth.PROVIDER.headers` is a string map,
empty by default, for non-secret identity overrides. Case-insensitive empty
values remove a declared header. `authorization`, `x-api-key`, `content-type`,
`accept`, `openai-beta`, and `chatgpt-account-id` are forbidden overrides.
Credentials belong in the configured credential source or private `auth.json`,
not in either TOML file. See [providers](../guides/providers.md).

## Hook keys

The canonical [hook schema](../../crates/ego-core/src/hook.rs) accepts:

| Key | Meaning / default |
| --- | --- |
| `event` | Required lifecycle event; list in [hooks](../integrations/hooks.md) |
| `matcher` | Optional exact lifecycle-native matcher; absent matches the event |
| `once` | False; true only for invocation-scoped skill hooks |
| `on_failure` | `block`; `allow` continues a pre-effect gate when its handler fails to start, times out or crashes, and records the failure. Refused on post-effect events |
| `handler.type` | `command`, `http`, `mcp_tool`, or `prompt` |
| Command fields | Required `command`; optional `args`, `shell`; argv form when `args` is present |
| HTTP fields | Required `url`; `headers=[]` pairs; optional `allowed_env_vars` |
| MCP fields | Required `server`, `tool`, `input` |
| Prompt fields | Required `prompt`; optional `model` |
| `handler.timeout` | Positive integer seconds; 30 for prompt, 600 for other kinds; other kinds use 30 on UserPromptSubmit/PreModelSwitch/PostModelSwitch/Interrupt |

The closed TOML/skill domain schema must not be confused with historical
Claude-shaped compatibility examples. Unknown fields, including fields absent
from the current handler schema, are errors. No handler grants authority.

## Every models.toml field

Model facts are separate from invocation configuration. Layers are compiled-in
catalogue < `/etc/ego/models.toml` < `$EGO_HOME/models.toml`, merged per slug.
Project model facts are never loaded. Supplied rows need a complete capabilities
object; stronger rows replace that model's facts rather than patching fields.
Source: [facts.rs](../../crates/ego-cli/src/facts.rs),
[capabilities.rs](../../crates/ego-core/src/generator/capabilities.rs).

| Path | Type / default |
| --- | --- |
| `default_model.PROVIDER` | Full slug belonging to the provider; overrides only that provider's first-run default |
| `model."PROVIDER/MODEL".capabilities.schema_version` | Required integer `5` |
| `...max_context_tokens`, `...max_output_tokens` | Optional positive ceilings; absent stays unknown |
| `...max_tools` | Optional tool-count ceiling |
| `...supports_parallel_tool_calls` | Boolean contract fact |
| `...reasoning_carryover` | `unsupported`, `same_provider_only`, `portable` |
| `...reasoning_state_dialect` | Optional named dialect for same-provider state |
| `...reasoning_efforts` | `"unknown"`, `"any"`, or `{only=[...]}` |
| `...default_reasoning_effort` | Optional supported effort; absent unknown |
| `...cancel` | `unsupported`, `cooperative`, `immediate` |
| `...cache` | `none`, `short`, `long` |
| `...transport` | `sse` or `web_socket`; Codex uses WebSocket, with guarded SSE fallback. `auto`, `web_transport`, and `websocket` are not accepted spellings |
| `...audio_input` | `unknown` (default), `unsupported`, or `supported_wav`; exact provider/model support is still required |
| `...image_input` | `"unknown"` default, `"unsupported"`, or supported table below |
| `model."PROVIDER/MODEL".pricing` | Optional quote table; absence is unpriced, not free |
| `retired."PROVIDER/MODEL".replacement` | Required replacement slug |
| `retired."PROVIDER/MODEL".retired` | Required date string; runtime tombstone consultation uses the builtin table |

Transport spellings come from
[`Transport`](../../crates/ego-core/src/generator/capabilities.rs); the
[Codex mapping](../../crates/ego-ai/src/wire/openai_codex_responses.rs) selects
its transport. A capability declaration is not a switch that changes another
provider's wire contract.

A supported image table has `kind="supported"`, `accepted_media_types` from
`png`, `jpeg`, `webp`, `gif`, and optional `max_images`, `max_bytes_per_image`,
`max_total_bytes`, `max_width`, `max_height`, `max_pixels`. Omitted limits stay
unknown; product ceilings still bind. Do not invent limits to make a request fit.

A pricing table requires `revision` and `currency`, with optional decimal-string
per-token rates `uncached_input`, `cache_read`, `cache_write`,
`cache_write_extended`, `output`. Missing categories remain unpriced. No built-in
prices are shipped. Monetary limits use recorded known response spend;
unknown prices or actual costs do not become zero or block generation. Unpriced
work has no finite monetary bound, and admitted responses can cross a cap. See
[cost and capability facts](../guides/providers.md#capability-and-cost-facts).
`ego models --json` exposes the resolved capabilities and provenance.

## Environment and private state

| Name/path | Purpose |
| --- | --- |
| `EGO_HOME` | User state/config root; default `~/.ego` |
| `EGO_MODEL`, `EGO_PROFILE` | Invocation defaults |
| `OLLAMA_HOST` | Ollama endpoint; default localhost:11434, normalized to `/v1/` |
| `OPENAI_BASE_URL`, `ANTHROPIC_BASE_URL` | API endpoint overrides; provider-owned request paths remain fixed |
| API key variables | [Provider table](../guides/providers.md) |
| `OPENAI_CODEX_ACCESS_TOKEN` | Externally managed access token for `openai-codex`; takes precedence over the stored login and disables ego-owned refresh |
| `EGO_LOG`, `EGO_LOG_FILE` | Opt-in diagnostic filtering/destination; see [troubleshooting](../guides/troubleshooting.md) |
| `VISUAL`, `EDITOR` | External config editor selection |
| `$EGO_HOME/sessions` | Default durable session store; invocation `--session-dir` can override |
| `$EGO_HOME/auth.json` | Private provider credentials; never publish or inspect it for troubleshooting output |

Ego does not import `.claude/settings.json` or its permission policy. Trusted
`.claude/skills` directories are recognized as skill sources, alongside the
other roots listed in [skills](../integrations/skills.md).

### Externally managed Codex access tokens

Set `OPENAI_CODEX_ACCESS_TOKEN` in the environment of the ego process that
uses `openai-codex`. For a host-managed ACP connection, put this name and its
value in that connection's process environment before the host starts
`ego acp` (the host's `POST /acp/connections` `env` field). Use one process per
credential identity; this is not a session JSON field or an ACP login token.

A nonblank value takes precedence over `oauth:openai-codex`. Ego does not
open the stored login, invoke a credential command, or refresh this token.
The token must contain the Codex account claim used by the existing login
path. An invalid token fails without a fallback to stored credentials. An
expired or revoked token that receives HTTP 401/403 ends generation with the
typed `Refused` authentication failure; the host must obtain a new token and
start a new process. An unset or whitespace-only value keeps the existing
stored-login and refresh behavior. Other providers are not affected.

Supply only the access token, never a refresh token. Do not put it in argv,
config files, session prompts, or diagnostic reports. Credential headers and
credential debug output are secret-safe; provider error diagnostics mask
reflected credential values before events and receipts are built. The
existing environment-secret masking also recognizes this `TOKEN` name for
tool output and exports. This does not protect against a provider that
intentionally encodes a secret in its output.

### Atomic managed batches

`ego config capabilities` returns versioned JSON with `atomic_batch: true` and
`max_operations: 64`. `ego config batch [--profile NAME] --set KEY=TOML_VALUE ...
--unset KEY ...` validates 1 to 64 managed operations before one atomic publication.
The default target is the user base; `--profile` selects one exact user profile.
Per-operation targets, duplicate keys, unknown keys, and invalid values are rejected.
Unrelated keys, profiles, and comments are preserved. Clients must use this owner
operation, not edit the owner's files or chain single-key writes.

Batch stdout is one JSON object: `{"v":1,"changed":true,"publication":"durable"}`.
`publication` is `not_published`, `durable`, or `durability_uncertain`. The last
state means the new values are authoritative but the final durability barrier
failed: read again, do not retry blindly. Failed validation publishes nothing.

### Revision-conditional publication

`ego config snapshot [--profile NAME]` returns the stored `ls --json` envelope
with an additional opaque `revision` string. Values and revision are captured
under the owner lock. Secret values remain redacted. The first snapshot creates
a private random revision key beside the user configuration; it does not set any
configuration values. Treat revision tokens as opaque and do not edit this key.

`ego config batch [--profile NAME] --if-revision TOKEN --set KEY=TOML_VALUE ...`
compares the latest revision and publishes under the same cross-process lock as
single-key, roots, rules, MCP, setup, and editor publication. A revision binds the
whole owner document and exact base/profile target; even an unrelated document
edit invalidates it. Reads do not resolve inherited values. Existing `get` and
`ls` output contracts are unchanged. A single conditional edit is a one-operation
batch. `ego config capabilities` advertises `compare_and_swap: true`.

A stale or wrong-target token returns exit 1 and JSON stdout
`{"v":1,"code":"conflict","persisted":false,"next_action":"snapshot"}`.
Read a fresh snapshot, recompute the change, and retry with its revision; never
retry the stale token or fall back to an unconditional write. Successful writes
use the batch receipt above; obtain the next revision with a fresh snapshot.
Revisions use a private keyed digest and do not disclose secret configuration.

## Shared instruction hierarchy

Edit `$EGO_HOME/config.toml` (`~/.ego/config.toml` by default) to opt into a
shared hierarchy. This is separate from file access: declare `roots` when the
sandbox must also reach files outside the workspace.

```toml
roots = [{ path = "/shared/projects", access = "read-write" }]

[instructions]
shared_root = "/shared/projects"
exclude_roots = ["/shared/projects/scratch"]
```

Only repositories inside the canonical shared root inherit its instructions.
A symlinked `AGENTS.md` is read when its canonical target resolves. A target
inside the discovery scope loads like any in-tree file. A target outside it
(for example `AGENTS.md -> ~/shared/RULES.md`) is an external instruction: it is
withheld, with a notice naming the target, until the workspace trust answer
approves that exact target path and SHA-256 digest; a changed target needs a new
answer. Links in a parent directory, dangling links and link cycles stay
refused, as do oversize files, import cycles and files past `max_files`.
To recover, answer the trust question again (or pass `--trust-workspace`), or
replace the link with a regular file.
Exclusions follow canonical paths, including symlinks. A missing shared root or
invalid link refuses instruction discovery; a missing exclusion is inert.
Project configuration cannot widen this user-owned discovery boundary. Use
`ego config edit` to edit these keys; the typed `config set` vocabulary does not
include instruction paths.

### Terminal root grants

A `Session` or `Permanent` answer to the terminal root question takes effect
when the asking turn ends: ego re-admits the same session in the same process
with the root. No configuration edit or restart is necessary. `/config roots`
lists session roots, permanent roots and owner sandbox grants. See
[permissions](../guides/permissions.md) for the effective-perimeter preview,
exec reach and ceilings.

### Revoke a terminal root grant

Use `/config roots revoke EXACT_PATH` in the human terminal that owns the session. Use the canonical path shown in the grant receipt. Revocation cancels the active turn and stops its child processes, records the revoked path, and rebuilds the same session through normal admission. The terminal stays open. Subsequent turns and child spawns use the narrower authority; a fresh resume does not restore the revoked session grant. A permanent grant also removes the matching root from the selected user-profile configuration.

A revoked path can still be named when its filesystem object has disappeared. Revoking an unknown grant reports a local error and does not alter authority. Rebuilding uses a new effect owner; it does not modify an existing OS guard.

## Provider protocol filter

For a one-file installation and a generic offline example, see the
[protocol-filter guide](../integrations/protocol-filter.md).

A user can install one reviewed executable that transforms final HTTP generation
bytes for one provider. This is opt-in user configuration in
`$EGO_HOME/config.toml`; other layers cannot declare a filter:

```toml
[auth.example.protocol_filter]
command = "/absolute/path/to/my-filter"
identity_headers = ["x-request-identity"]
timeout_ms = 1000
enabled = true
```

Install the one executable at that absolute path and make it executable. A script
must name its interpreter with a valid shebang. Python is optional; Ego requires
no addon runtime. Remove this table or set `enabled = false` to disable the filter.
Restart a session to load changed configuration. Ego ships no provider identity
algorithm and makes no subscription compatibility claim.

Version 1 includes journal-backed, write-once session variables. Stdin carries
`state: {revision, values}` and stdout requires `expected_revision` and
`state_writes`. New values are persisted before HTTP dispatch; a failed write,
stale revision or attempted replacement fails closed. The same session retains
values on resume or continue; new sessions and executable paths have separate
state. See [session variable examples and limits](../integrations/protocol-filter.md#capture-a-session-variable-once).

For each actual generation attempt Ego starts the executable without a shell or
arguments for a bounded request exchange, then starts a persistent response
process. It transforms incoming bytes before provider decoding, including tool
arguments. The response process uses sequenced JSON lines and a final EOF
exchange; the timeout applies to each exchange, excluding provider idle time.
See the [response contract](../integrations/protocol-filter.md#response-stream-exchange). CLI and ACP use the
same generation path. Preflight, login, refresh, discovery and MCP do not invoke
it. Version 1 rejects WebSocket mappings that rewrite their outbound body.

Stdin contains one JSON object:

```json
{"version":1,"direction":"request","provider":"example","model":"model","session_id":"uuid","generation_id":"uuid","attempt_id":"uuid","body_base64":"e30=","state":{"revision":0,"values":{}}}
```

`body_base64` is the exact normally encoded request body. `session_id` is the
active session UUID supplied by the journal owner. Attempt
and generation UUIDs come from the admitted attempt. There is no inferred first
prompt or previous-response field. An addon that requires these fields must
refuse unsupported requests. The filter returns exactly one JSON object:

```json
{"version":1,"body_base64":"eyJleGFtcGxlIjp0cnVlfQ==","identity_headers":[{"name":"x-request-identity","value":"example"}],"expected_revision":0,"state_writes":{}}
```

Unknown fields, duplicate fields, trailing output, missing fields and other
versions fail. Ego sends returned bytes unchanged and applies only declared
identity headers, replacing existing public values by name. Ego retains its
credential headers. Authentication, cookies, routing, framing, content negotiation,
protocol-version and beta headers are forbidden even if allowlisted. Header names
are case insensitive; duplicate output names fail. Values must be ASCII HTTP
field values, at most 4096 bytes. Names are limited to 128 bytes, with at most
16 headers. The HTTP transport derives framing and request admission digests from
the final body. Cache-prefix observations use the transformed bytes.

Input/output bodies are limited to 16 MiB. Each JSON envelope is limited to
22,435,160 bytes including base64 expansion. The command timeout is 1..1000 ms
(default 1000), further constrained by the run deadline. Process-tree cleanup
adds a bounded grace. Child stdout is bounded; stderr is discarded. Cancellation,
timeout, command failure and invalid output stop generation before HTTP dispatch;
Ego never silently falls back to the unfiltered request.

This executable is trusted user code with ordinary OS access. Ego clears the
inherited environment and sends no credentials, request headers or URL through
the child contract. The body contains conversation content and may itself be
sensitive. Unix children receive only a fixed `/usr/bin:/bin` PATH;
Windows children receive OS bootstrap paths. Do not put credentials in the
executable path or in addon diagnostics. Environment isolation does not prevent
trusted code from reading files or making its own network requests.

## Generation loop limits

Ego accepts at most 128 tool calls per response, including at most 8 calls with
identical tool names and normalized JSON arguments. It accepts at most 4,096
calls across one turn. Exceeding a limit stops the generation and refuses every
call in that response before dispatch; earlier completed responses keep their
effects. The diagnostic is a typed `invalid` error and the headless run exits 7 with structured reason `guard`.

Repeating the same tool and normalized JSON arguments in another response of
the same turn gives the model a loop warning on occurrence 3, advising a blocking
wait or backoff. Occurrence 8 with an unchanged result stops the turn. A changed model-visible
result resets that fingerprint count as progress; the total turn budget still
bounds polling with ever-changing results. Normalization ignores JSON whitespace and object
key order, including nested objects; string contents and array order remain
significant. These call limits are fixed and reset for the next user turn.

Use `max_output_tokens = 16384` in configuration or
`--max-output-tokens 16384` to replace the 32,768-token response default.
The supported model maximum still caps an explicit setting. Provider mappings
send the ceiling using their supported wire field. The subscription Responses
mapping has no server-side output-limit field; the tool-call limits still apply.

Tool arguments containing an exact ASCII `<|controlDIGITS|>` model token are
refused before dispatch, including JSON-escaped spellings. This uses the same
typed local guard outcome (exit 7, structured reason `guard`). Similar ordinary
punctuation and model narration are preserved. Token rejection covers the whole
response, so no sibling call from that response executes.
