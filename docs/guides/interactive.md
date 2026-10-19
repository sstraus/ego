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
| Model and reasoning | `/model`, `/thinking`, `/fast on\|off`; model-picker shortcuts are shown by `/help` |
| Policy | `/mode`, `/plan`, `/yolo`, `/permissions` |
| State and health | `/config`, `/config perimeter`, `/config set KEY TOML_VALUE`, `/doctor`, `/usage`, `/statusline` |
| Conversation | `/sessions`, `/history`, `/new`, `/name`, `/goal`, `/fork [message-id]`, `/compact`, `/refresh` |
| Tools and extensions | `/tools`, `/expand`, `/mcp`, `/skills`, `/hooks` |
| Instructions and memory | `/init`, `/memory`; see [instructions and memory](memory.md) |
| Background execution and retry | `/tasks [ID \| cancel ID]`, `/retry` |
| Local presentation/export | `/clear`, `/export [local\|sanitized] [md\|json\|jsonl]` |

Some actions require idle state or a registered owner; a disabled action names
its reason. Exact registered slash commands execute with one Enter. Enter on a
selected slash completion inserts and submits that command. Tab does not submit.
Slash text in bracketed paste remains ordinary prompt text; it does not execute
a pasted control command.

`/clear` clears composer/menu state and confirms that transcript, context, and
durable history are unchanged. `/expand` pages the
retained last-tool output. `/export` prints to this transcript, not a file, and
reports its disclosure and format. The default remains a sanitized JSON evidence
bundle in a markdown fence; export serialization is unchanged. `/refresh` applies current instructions through a
new prompt epoch when source work is idle.

`/name [show|set TEXT|clear]` reads or changes display metadata.
`/goal [show|set TEXT|achieve|block REASON]` inspects or changes the persistent goal.
`/tools`, `/skills`, and `/doctor` show concise inventories; append `details`
for complete entries and evidence. `/skills diagnose` shows admission diagnostics.
`/resume [ID]` and `/continue [ID]` resume sessions; `/history QUERY` searches them.
These aliases support completion. Unknown slash spellings are refused locally.
Add a leading space to send literal prompt text that starts with `/`.

## Input while work is running

The live composer preserves drafts and queues accepted prompts. Steering and
ordinary queued prompts are distinct. Up on an empty busy composer withdraws
the last queued prompt, including its images, for editing. Enter queues the
complete edited prompt again. Withdrawal is durable, so resume does not deliver
the old prompt. Local inspection and model-picker actions
can remain responsive during streaming; model changes take effect at a generation
boundary, never by mutating an already-sent request. `/help` answers during a turn.
Local commands that need the coordinator show a persistent pending receipt and
run once after the current turn settles, including when Escape interrupts it.

Escape requests interruption while busy; Ctrl+C is the interrupt path, and
Ctrl+D exits from the idle driver. Pending permission/question selectors own
Enter/Escape until answered and preserve the underlying draft/queue.
Use the shortcuts shown by this binary's `/help`; terminal Meta/function key
support differs across platforms.

## Image drafts

Ctrl+V reads the OS pasteboard. An image adds a removable chip to the unsent
draft; text inserts at the cursor. An entire bracketed paste that names an
existing supported image file also adds a chip. Image drafting remains available
while work runs and does not change the active provider request.

Enter admits text and images as one prompt. A refused image leaves the draft
available for correction and resubmission, without queueing a text-only partial
prompt. Images follow their text through saved drafts, session changes and queue
recall. Idle Escape removes the newest image before its other idle behavior;
while busy, Escape remains the interruption action. Slow image admission or
model discovery can delay keyboard handling, including Escape. Permission and
question controls can still be serviced during that wait.

The model's image capability and size/count limits apply before submission.
Each draft also has product limits of 100 images and 64 MiB of encoded image
bytes. File size is checked at paste time and exact bytes are checked again at
submission. A missing or unsupported image path remains ordinary text.

## Setup model authority

`/setup PROMPT` uses a separately resolved model from persistent user/profile
configuration, not the live session model selected by flags or `/model`.
If that interpreter model is absent or unavailable, setup explains the reason
and uses its deterministic inventory. Esc cancels without configuration writes.

## MCP servers

`/mcp` lists configured servers with their live state. `/mcp toggle NAME` turns
one server off or on for this session without changing its configuration.
`/mcp add NAME --stdio -- COMMAND ARG...` and `/mcp add NAME --http URL` use
the same typed configuration transaction as `ego mcp add`. Quote arguments
that contain spaces. MCP changes require an idle turn and use the current
session confinement; they do not grant new process or network authority.

## Status and state

The width-aware status row shows selected model/effort/mode, context occupancy,
queue/drafts/interrupt, workspace/Git, and trust as configured. It uses whole
segments under width pressure. A missing monetary rate is not a zero-cost run.
Configure `ui.statusline.template` with MiniJinja to place status above or below
the editor. The template must render `{{ prompt }}` once on its own line.
`/statusline` offers presets; its Off preset stores only `{{ prompt }}`.
Active queue, draft and interrupt state remains visible even with optional
status disabled. The old `segments` configuration key is not supported.
The `●` after the Git branch means there are local changes, untracked files, or
conflicts; it is not an activity indicator. An approximate subscription cost is
an API-equivalent estimate, not a charge to the subscription account.

Successful cache hits do not print a notice on every generation. A warning can
appear when a provider previously reported reuse for the same marked input but
then reports no reused input. Missing usage data is not treated as a cache miss.

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
