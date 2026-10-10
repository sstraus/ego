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
Saving a default affects future sessions. Use live `/model`, `/thinking`, or
`/mode` controls to change current session selections.

## Every config.toml key

The source schema for base/profile, MCP, auth, and UI fields is
[layer.rs](../../crates/ego-core/src/config/layer.rs). Resolution and inheritance
are in [resolve.rs](../../crates/ego-core/src/config/resolve.rs). “Absent” below
means this layer supplies no override; defaults are the final behavior when no
higher or lower layer supplies a value.

| Key | Accepted value | Default / applicability |
| --- | --- | --- |
| `model` | Full `provider/model` string | No fixed model; interactive first-run selection, explicit/configured model in headless |
| `thinking` | `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max` | Model-declared default; unknown requires explicit selection |
| `mode` | `plan`, `default`, `auto`, `edits`, `yolo` | Trusted interactive with no configured mode uses `auto`; ordinary unresolved policy uses `default` |
| `sandbox` | `ro`, `workspace`, `adaptive`, `off` | `adaptive` when absent on macOS, `workspace` elsewhere; effective inspection reports the platform default reason; explicit selections retain their meaning, system ceilings bind, mode is independent |
| `max_mode` | Same mode vocabulary | Absent = no system maximum; system-only, does not select a mode |
| `roots` | Array of `{path="...", access="read"\|"read-write"}` | Under strict levels, absent declaration adds read-write `~/Gits` when present. Adaptive omits that implicit root; `[]` clears configured roots, while adaptive derived roots remain. System declarations cap additional roots |
| `exec_paths` | Array of paths | Empty; extra read/executable roots, under system root bounds |
| `exec.env` | `inherit`, `core`, `allowlist` | `inherit` |
| `exec.env_allowlist` | Array of environment names | Empty; used only with `allowlist` |
| `network` | `on`, `off` | `on`; explicit `--offline` cannot be cleared by this setting |
| `orchestration.mode` | `observe`, `active` | `observe`; see [orchestration](../integrations/orchestration.md) |
| `permissions.judge` | Full model slug | Absent; auto analysis can escalate without a configured judge |
| `permissions.allow` | Array of canonical predicates | Empty contribution |
| `permissions.deny` | Array of canonical predicates | Empty contribution; system denies remain immutable |
| `budget` | String such as `"20k"`, `"400k"`, or `"2.5$"` | No configured ceiling; system money/token limits clamp requests independently |
| `bare` | Boolean | False when trusted; untrusted resolution is bare regardless |
| `cache` | `none`, `short`, `long` | `short`; `long` requires explicit user config |
| `profile.NAME` | Profile table | No selected profile without `--profile`/`EGO_PROFILE` |
| `auth.PROVIDER` | Credential-command/identity-header table | No override |
| `ui.statusline.segments` | Ordered array of supported segment names | Built-in order below; `[]` disables the row |
| `mcp.servers.NAME` | Complete tagged server entry or tombstone | No configured external server |
| `hooks` | Ordered array of hook entries | Empty contribution; hooks combine across admitted sources |

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
Extra roots use canonical paths; conflicting nested read-only/read-write roots
are refused. See [permissions](../guides/permissions.md).

`exec.env="core"` retains only present PATH, HOME, USER, LOGNAME, SHELL, TMPDIR,
TMP, TEMP, LANG, LC_ALL, LC_CTYPE, SYSTEMROOT, WINDIR, COMSPEC, and PATHEXT.
An empty allowlist keeps no environment variables. This setting affects built-in
exec children, not provider credentials, hook declarations, or MCP environments.

The default status order is `model`, `effort`, `mode`, `context`, `cost`, `queue`, `drafts`,
`interrupt`, `cwd`, `git`, `trust`. Unknown and
duplicate names are errors. Width pressure drops whole segments. Source:
[status configuration](../../crates/ego-cli/src/repl/seat.rs).

## Profiles and managed edits

A profile accepts `model`, `thinking`, `mode`, `sandbox`, `roots`, `exec`,
`exec_paths`, `network`, `orchestration`, `permissions`, `budget`, and `mcp`.
It does not accept base-only `bare`, `cache`, `max_mode`, `auth`, `ui`, or `hooks`.
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
| User base | `model`, `thinking`, `mode`, `sandbox`, `roots`, `budget`, `bare`, `permissions.judge`, `ui.statusline.segments`, `exec.env`, `exec.env_allowlist`, `network` |
| User profile | Same minus `bare` and `ui.statusline.segments` |
| User `ci` profile | Profile keys plus `session_dir`, `artifact_dir` |

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
| `http` | Required `transport="http"`, `url`; `auth="none"` (or `oauth`), `allow_insecure_loopback=false` |
| Tombstone | `removed=true`, with no transport fields |

HTTP URLs reject embedded credentials. HTTP without TLS needs the explicit
insecure-loopback opt-in and a loopback URL. Explicit inherited variable names
are admission facts, not arbitrary environment values in TOML.
See [MCP](../integrations/mcp.md) for connection and login behavior.

## Authentication keys

`auth.PROVIDER.credential_command` is an optional program for login/refresh;
its stdout carries a secret result. `auth.PROVIDER.headers` is a string map,
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
| `handler.type` | `command`, `http`, `mcp_tool`, or `prompt` |
| Command fields | Required `command`; optional `args`, `shell`; argv form when `args` is present |
| HTTP fields | Required `url`; `headers=[]` pairs; optional `allowed_env_vars` |
| MCP fields | Required `server`, `tool`, `input` |
| Prompt fields | Required `prompt`; optional `model` |
| `handler.timeout` | Positive integer seconds; 30 for prompt, 600 for other kinds; other kinds use 30 on UserPromptSubmit/PreModelSwitch/PostModelSwitch |

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
| `...transport` | `sse` (the only implemented transport) |
| `...image_input` | `"unknown"` default, `"unsupported"`, or supported table below |
| `model."PROVIDER/MODEL".pricing` | Optional quote table; absence is unpriced, not free |
| `retired."PROVIDER/MODEL".replacement` | Required replacement slug |
| `retired."PROVIDER/MODEL".retired` | Required date string; runtime tombstone consultation uses the builtin table |

A supported image table has `kind="supported"`, `accepted_media_types` from
`png`, `jpeg`, `webp`, `gif`, and optional `max_images`, `max_bytes_per_image`,
`max_total_bytes`, `max_width`, `max_height`, `max_pixels`. Omitted limits stay
unknown; product ceilings still bind. Do not invent limits to make a request fit.

A pricing table requires `revision` and `currency`, with optional decimal-string
per-token rates `uncached_input`, `cache_read`, `cache_write`,
`cache_write_extended`, `output`. Missing categories remain unpriced. No built-in
prices are shipped. Monetary limits require enough known rates for admission.
`ego models --json` exposes the resolved capabilities and provenance.

## Environment and private state

| Name/path | Purpose |
| --- | --- |
| `EGO_HOME` | User state/config root; default `~/.ego` |
| `EGO_MODEL`, `EGO_PROFILE` | Invocation defaults |
| `OLLAMA_HOST` | Ollama endpoint; default localhost:11434, normalized to `/v1/` |
| `OPENAI_BASE_URL`, `ANTHROPIC_BASE_URL` | API endpoint overrides; provider-owned request paths remain fixed |
| API key variables | [Provider table](../guides/providers.md) |
| `EGO_LOG`, `EGO_LOG_FILE` | Opt-in diagnostic filtering/destination; see [troubleshooting](../guides/troubleshooting.md) |
| `VISUAL`, `EDITOR` | External config editor selection |
| `TUIC_SOCKET` | Native orchestration connection |
| `$EGO_HOME/sessions` | Default durable session store; invocation `--session-dir` can override |
| `$EGO_HOME/auth.json` | Private provider credentials; never publish or inspect it for troubleshooting output |

No implicit `.claude/settings.json` or `.claude/skills` alias exists. See
[integrations](../integrations/README.md) for the corresponding εgo sources.
