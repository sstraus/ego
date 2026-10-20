# Hooks

Hooks add lifecycle checks and source-tagged feedback. They are subtractive:
a hook can block or escalate work, but cannot grant a permit or relax a sandbox,
system deny, offline boundary, or budget. mdkb session warmup and prompt-specific recall use a
separate context source, not a `SessionStart` or `UserPromptSubmit` hook result. See
[the prompt-context decision](../internals/decisions/2026-10-17-prompt-recall-context.md).

`ego hooks ls` inspects system/user hooks without loading project configuration.
`/hooks` inspects the active session's admitted catalogue, including trusted
project and invocation-scoped skill hooks. Neither command installs a hook.

## Optional mdkb context

Set the top-level `mdkb_prompt_command` key in user configuration to the absolute
path of your mdkb executable. For example, use the path returned by `command -v mdkb`:

```toml
mdkb_prompt_command = "/absolute/path/to/mdkb"
```

The source is disabled by default. Project configuration cannot enable it.
It calls `mdkb hook session-start` once before the first admitted prompt of a run
and `mdkb hook user-prompt-submit` for each admitted prompt, with the real ego
session ID and workspace path. A successful durable warmup is reused on resume.
Empty or failed warmup is not retried across interactive turns in the same active
session epoch. A new process/resume can retry it.
After tool execution, `mdkb hook post-tool-use` receives settled input and the
model-visible result. Only the matching event's
`hookSpecificOutput.additionalContext` string is admitted as source-tagged,
untrusted user-role data. It never becomes a lifecycle decision or permission.

The warmup deadline is 2500 ms; recall and tool observation deadlines are 1500 ms.
Each request and response is limited to 64 KiB. Over-limit data is rejected,
not silently shortened into a successful tool outcome. Admitted context is limited
to 16 KiB per contribution. A denied command, timeout, invalid reply or execution
failure means absent context, not a blocked prompt. Normal guarded process policy
still applies: this setting does not grant execution permission. The
`MDKB_DISTILLER_ACTIVE` environment marker disables this source to avoid recursion.
Remove the key to disable it; unrelated lifecycle hooks are not changed.

**Stop episode delivery is deferred (2026-10-20).** The current mdkb Stop hook
requires a transcript file path and does not accept an inline ego episode.
Ego does not invoke a Stop command or deliver episodes for prior settlement/mining.
This data source has best-effort delivery, not replay-safe external receipts.

## Declare a hook

Use ordered `[[hooks]]` entries in admitted εgo TOML configuration, or a skill's
`hooks` sequence. εgo does not import `.claude/settings.json` or plugin hook maps.
The canonical shape has `event`, optional `matcher`, `once`, and a tagged
`handler`; every field/default is in [configuration](../reference/configuration.md).

```toml
[[hooks]]
event = "PreToolUse"
matcher = "exec"
[hooks.handler]
type = "command"
command = "/absolute/path/to/your-check"
args = []
timeout = 30
```

Replace the program path with your real checker. This is a declaration shape,
not an included executable. Exact argv is used when `args` is present; otherwise
an explicitly supported interpreter spelling is required for shell behavior.

Handlers are `command`, `http`, `mcp_tool`, or `prompt`. They use existing guarded
effects, egress, credentials, deadlines, cancellation, evidence, and budget owners.
Prompt handlers receive no tools. Unsupported handlers/fields fail by name; old
compatibility worksheets are not the current closed TOML schema.

## Lifecycle events

The current enum names:

- `SessionStart`, `InstructionsLoaded`, `UserPromptSubmit`, `UserPromptExpansion`
- `PreToolUse`, `PermissionRequest`, `PermissionDenied`, `PostToolUse`, `PostToolUseFailure`, `PostToolBatch`
- `PreModelSwitch`, `PostModelSwitch`, `SubagentStart`, `SubagentStop`
- `Stop`, `StopFailure`, `PreCompact`, `PostCompact`, `Elicitation`, `ElicitationResult`, `SessionEnd`

Event-native matchers are exact, not arbitrary JavaScript regular expressions.
The configuration source preserves order: system, user, trusted project, then
active skills. A later source does not replace an earlier hook.

## Failure and replay

Admission validates a complete bounded candidate and retains last-good on reload
failure. Pre-effect handler failure/malformed response/timeout blocks rather than
letting work proceed. Post-effect failure cannot undo an effect that already ran;
it becomes evidence. Durable started/settled records prevent blind replay after
ambiguous delivery for lifecycle hooks. The separate mdkb context source is
best effort: it has no durable external-delivery receipt or cross-process deduplication.

`once=true` is meaningful only for invocation-scoped skill hooks. It is consumed
at durable invocation start, including failure, timeout, or uncertainty; it is
not a success-only retry trigger.

Sources: [hook domain/schema](../../crates/ego-core/src/hook.rs),
[event vocabulary](../../crates/ego-core/src/event/hook.rs),
[OS adapters](../../crates/ego-tools/src/hook.rs), and
[hook lifecycle tests](../../crates/ego-cli/tests/hook_lifecycle.rs).
The [compatibility research](../internals/21-claude-hook-parity.md) is retained
with its dated claims; use the current source schema for accepted fields.
