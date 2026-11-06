# Interactive use

Read this page in the [hosted user guide](https://sstraus.github.io/ego/guides/interactive.html).

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
| State and health | `/config`, `/config perimeter`, `/config roots`, `/config set KEY TOML_VALUE`, `/doctor`, `/usage`, `/statusline` |
| Conversation | `/sessions`, `/history`, `/new`, `/name`, `/goal`, `/fork [message-id]`, `/compact`, `/refresh` |
| Side question during a turn | `/side QUESTION` |
| Tools and extensions | `/tools`, `/expand`, `/mcp`, `/skills`, `/hooks` |
| Project instructions | `/init`; see [project instructions](memory.md) |
| Background execution and retry | `/tasks [ID \| cancel ID]`, `/retry` |
| Local presentation/export | `/clear`, `/export [local\|sanitized] [md\|json\|jsonl]` |

## Recap after an absence

Turn on `recap.enabled` (a row in `ego setup`, or
`ego config set recap.enabled=true`) to get a short recap when you come back.
When the last turn completed at least `recap.after_minutes` ago (default 30),
the first prompt you send shows a recap of the session, at most five lines,
before that prompt runs. The absence counts across restarts, so a session
resumed the next day also gets one. The recap is one model call on the
session's model and context. A usage line follows it and `/usage` counts it.
It is not added to the history and runs no tool. If it fails, ego says so and
runs the prompt. It is off by default, and a project or local configuration
file cannot turn it on.

Some actions require idle state or a registered owner; a disabled action names
its reason. Exact registered slash commands execute with one Enter. Enter on a
selected slash completion inserts and submits that command. Tab does not submit.
Slash text in bracketed paste remains ordinary prompt text; it does not execute
a pasted control command.

`/clear` clears composer/menu state and confirms that transcript, context, and
durable history are unchanged. `/expand` pages the
retained last-tool output. `/export` prints to this transcript, not a file, and
reports its disclosure and format. The default is sanitized Markdown containing a fenced JSON evidence bundle;
explicit `json` selects the JSON bytes without that wrapper. `/refresh` applies current instructions through a
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
ordinary queued prompts are distinct. Up on the first line of a busy composer
withdraws the last queued prompt, including its images, for editing. Any current
text and images are saved as a draft (Ctrl+S restores the latest saved draft
when the composer is empty). Repeated Up withdraws the previous queued prompt.
On later composer lines, Up moves the cursor; without queued prompts it keeps
normal history navigation. Enter queues the complete edited prompt again.
Withdrawal is durable, so resume does not deliver the old prompt.

Ctrl+Enter while busy steers all queued prompts in order, followed by any text
and images in the composer. An empty composer does not add a blank prompt.
This uses the same durable interrupt and delivery path as Escape: delivery
occurs at the next safe boundary, not by interrupting an effect mid-flight.
Idle Ctrl+Enter with no queued prompts still inserts a newline. Local inspection and model-picker actions
can remain responsive during streaming; model changes take effect at a generation
boundary, never by mutating an already-sent request. `/help` answers during a turn.
Local commands that need the coordinator show a persistent pending receipt and
run once after the current turn settles, including when Escape interrupts it.

Escape requests interruption while busy; Ctrl+C is the interrupt path, and
Ctrl+D exits from the idle driver. Pending permission/question selectors own
Enter/Escape until answered and preserve the underlying draft/queue.
Use the shortcuts shown by this binary's `/help`; terminal Meta/function key
support differs across platforms.

## Keyboard preferences

The default composer uses Emacs editing. Start `ego --vim` or save
`ego config set 'ui.vim=true'` for Vim editing. `--vim` overrides a saved false
value; headless runs and prompted resumes refuse this terminal-only flag.
Keyboard preferences come from the user file, not project configuration or
session policy.

Override action shortcuts by saving a map, for example:

```sh
ego config set 'ui.keybindings={model=["Ctrl+P","F2"],expand=[]}'
```

An empty array removes that action's shortcuts. `ego config unset ui.keybindings`
restores defaults. Read the current binary's `/help` for active shortcuts.
Invalid chords and conflicting action shortcuts are refused. Source:
[keyboard preferences](../../crates/ego-cli/src/config.rs) and
[action registry](../../crates/ego-cli/src/repl/actions.rs).

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

`/mcp` lists configured servers with their live state. `/mcp login NAME` opens
the configured HTTP server's OAuth flow and reconnects it after successful login,
without leaving the session. Escape cancels the pending login. See
[MCP login](../integrations/mcp.md#remote-oauth-login) for registration choices
and the remaining live-verification boundary. `/mcp toggle NAME` turns
one server off or on for this session without changing its configuration.
`/mcp add NAME --stdio -- COMMAND ARG...` and `/mcp add NAME --http URL` use
the same typed configuration transaction as `ego mcp add`. Quote arguments
that contain spaces. MCP changes require an idle turn and use the current
session confinement; they do not grant new process or network authority.

## Status and state

The width-aware status row shows selected model/effort/mode, context occupancy,
steering/next-turn counts, drafts/interrupt, workspace/Git, and trust as configured.
The counts read `steer N · next N`: messages that steer the running turn, then
messages queued for the next turn. Both counts appear when either is nonzero;
the field is omitted when both are zero. It uses whole
segments under width pressure. A missing monetary rate is not a zero-cost run.
Configure `ui.statusline.template` with MiniJinja to place status above or below
the editor. The template must render `{{ prompt }}` once on its own line.
`/statusline` offers presets; its Off preset stores only `{{ prompt }}`.
Active queue, draft and interrupt state remains visible even with optional
status disabled. The old `segments` configuration key is not supported.
The `●` after the Git branch means there are local changes, untracked files, or
conflicts; it is not an activity indicator. An approximate subscription cost is
an API-equivalent estimate, not a charge to the subscription account.

Running in-turn tasks and named subagents have their own cyan activity rows,
separate from the main working row. A settled child no longer appears as running.
Each live `task(background=true)` child in the attached session also has its own
row, also while the parent is idle. The row shows the name, current activity,
elapsed time, and provider-reported output tokens of that child. Settlement
removes the row. A display failure does not cancel the child or change its
spending authority. `/tasks` still lists and cancels durable background tasks.

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
[interactive surface tests](../../crates/ego-cli/tests/it/surfaces.rs).
