# Interactive use

The terminal lets you keep a conversation running while inspecting state,
changing selections, or giving the current work new direction. Local controls
are handled by the harness; they are not sent as model prompts.

Start with `ego` or `ego PROMPT`, optionally selecting `-m PROVIDER/MODEL` and
`--thinking LEVEL`. The workspace trust prompt precedes project admission.
Reasoning selection refuses unsupported values rather than silently substituting.

## Actions by task

| Task | Actions |
| --- | --- |
| Help and identity | `/help`, `/version`, `/quit` |
| Model and reasoning | `/model`, `/thinking`; model-picker shortcuts are shown by `/help` |
| Policy | `/mode`, `/plan`, `/yolo`, `/permissions` |
| State and health | `/config`, `/config perimeter`, `/config set KEY TOML_VALUE`, `/doctor`, `/usage`, `/statusline` |
| Conversation | `/sessions`, `/history`, `/new`, `/name`, `/goal`, `/fork [message-id]`, `/compact`, `/refresh` |
| Tools and extensions | `/tools`, `/expand`, `/mcp`, `/skills`, `/hooks` |
| Local presentation/export | `/clear`, `/export [local\|sanitized] [md\|json\|jsonl]` |

Some actions require idle state or a registered owner; a disabled action names
its reason. Exact registered slash commands execute with one Enter. Partial
completion does not submit. Slash text in bracketed paste remains ordinary
prompt text; it does not execute a pasted control command.

`/clear` clears composer/menu state, not durable history. `/expand` pages the
retained last-tool output. `/refresh` applies current instructions through a
new prompt epoch when source work is idle.

## Input while work is running

The live composer preserves drafts and queues accepted prompts. Steering and
ordinary queued prompts are distinct. Local inspection and model-picker actions
can remain responsive during streaming; model changes take effect at a generation
boundary, never by mutating an already-sent request.

Escape requests interruption while busy; Ctrl+C is the interrupt path, and
Ctrl+D exits from the idle driver. Pending permission/question selectors own
Enter/Escape until answered and preserve the underlying draft/queue.
Use the shortcuts shown by this binary's `/help`; terminal Meta/function key
support differs across platforms.

## Status and state

The width-aware status row shows selected model/effort/mode, context occupancy,
queue/drafts/interrupt, workspace/Git, and trust as configured. It uses whole
segments under width pressure. A missing monetary rate is not a zero-cost run.
An explicitly empty status segment list disables the row.

`/config` reports captured startup sources and clamps. `/config set` writes user
defaults for future sessions, preserving the current session's independent live
model/mode/budget/confinement authorities. `/config perimeter` projects admitted
roots and backend facts. See [configuration](../reference/configuration.md).

`/sessions` browses durable work; [sessions](sessions.md) explains resume,
completed-message forks, checkpoints, export, and usage ownership. A compact/fork
receipt identifies the successor before input moves to that session.

Sources: [action registry](../../crates/ego-cli/src/repl/actions.rs),
[input keys](../../crates/ego-cli/src/repl/keys.rs),
[interactive driver](../../crates/ego-cli/src/repl/mod.rs), and
[interactive surface tests](../../crates/ego-cli/tests/surfaces.rs).
