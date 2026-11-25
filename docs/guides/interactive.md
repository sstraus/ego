# Interactive use

Read this page in the [hosted user guide](https://ego.tuicommander.com/docs/guides/interactive.html).

The terminal lets you keep a conversation running while inspecting state,
changing selections, or giving the current work new direction. Local controls
are handled by the harness; they are not sent as model prompts.

Start with `ego` or `ego PROMPT`, optionally selecting `-m PROVIDER/MODEL` and
`--thinking LEVEL`. The workspace trust prompt precedes project admission.
Reasoning selection refuses unsupported values rather than silently substituting.

## Startup summary

The interactive startup shows the selected model, authentication method and
credential source, permission mode, and sandbox with their configuration
sources. A saved subscription credential is distinct from an external access
token or API key. The summary does not log in, renew credentials, or verify
subscription entitlement. It never prints credential values.

A filter row appears only when a filter is configured for the selected provider
or globally. A provider declaration overrides the global declaration, including
when the provider filter is disabled. `/config details` shows the full selected
filter command.

Directory grants show their `ro` or `rw` access. Attachment grants have their
own group: their read-only scope does not describe workspace access. Each group
previews at most three directories; `/config perimeter` shows every full path.
Long paths use distinct shortened labels or numbered directory references.
The sandbox-off security warning remains visible. MCP attention groups affected
server names, counts each server once, and points to `/mcp` for details and controls.

Startup key hints contain at most four items. Below 60 columns, only Escape and
help remain. `/help` lists the complete key registry, including queue, steering,
draft, and background actions. `--plain` retains append-only startup diagnostics;
`--quiet` omits the summary but does not hide security diagnostics.

## Actions by task

| Task | Actions |
| --- | --- |
| Help and identity | `/help`, `/version`, `/quit` |
| Model and reasoning | `/model`, `/model QUERY` (unique available name fragment, session only), `/thinking`, `/fast on\|off`; model-picker shortcuts are shown by `/help` |
| Policy | `/mode`, `/plan`, `/yolo`, `/permissions` |
| State and health | `/config`, `/config perimeter`, `/config roots`, `/config roots revoke PATH`, `/config set KEY TOML_VALUE`, `/doctor`, `/usage`, `/statusline`, `/context` (instruction files and memory in the system prompt) |
| Conversation | `/sessions`, `/resume [ID]`, `/continue [ID]`, `/history QUERY`, `/new`, `/name`, `/goal`, `/fork [message-id]`, `/compact`, `/refresh` |
| Standing constraints | `/pin [TEXT\|list]` keeps a constraint outside the summary; `/unpin NUMBER` removes one |
| Side question during a turn | `/side QUESTION` |
| Tools and extensions | `/tools`, `/expand`, `/mcp`, `/skills`, `/hooks`, `/login [PROVIDER]` (built-in OAuth login) |
| Project instructions | `/init`; see [project instructions](memory.md) |
| Background execution and retry | `/tasks [tN\|ID\|cancel tN]`, `/detach` (return input from an attached child; alias of `/tasks`), `/retry` |
| Conversation and export | `/clear`, `/export [local\|sanitized] [md\|json\|jsonl]` |

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

`/clear` clears the composer and starts a fresh conversation without confirmation.
The previous session remains on disk and can be resumed. `/expand` pages the
retained last-tool output. `/export` prints to this transcript, not a file, and
reports its disclosure and format. Its default differs from the `ego export`
command: `/export` is sanitized, while `ego export` is local (unsanitized) unless
you pass `--sanitize`, and writes to stdout or to `--file`. The default is sanitized Markdown containing a fenced JSON evidence bundle;
explicit `json` selects the JSON bytes without that wrapper. `/refresh` applies current instructions through a
new prompt epoch when source work is idle.

`/name [show|set TEXT|clear]` reads or changes display metadata.
`/goal [TEXT|show|set TEXT|achieve|resume|block REASON]` inspects or changes the
persistent goal. Bare `/goal` shows it, and `/goal TEXT` is the same as
`/goal set TEXT`. Setting new text
while another goal is active replaces the active goal and starts fresh work;
the old goal remains in the session history. A goal that the model set pauses
when its automatic continuation allowance (20 turns by default) is used up;
`/goal resume` grants it another 20 turns. It refuses for any other goal.
The same operations exist for scripts as
`ego resume ID --goal-operation show|set=TEXT|achieve|resume|block=REASON`.
The model can set a goal only when none is active. See
[goals](automation.md#goals) for goal checks and limits.
`/tools`, `/skills`, and `/doctor` show concise inventories; append `details`
for complete entries and evidence. `/skills diagnose` shows admission diagnostics.
`/resume [ID]` and `/continue [ID]` resume sessions; `/history QUERY` searches them.
These aliases support completion. Unknown slash spellings are refused locally.
Add a leading space to send literal prompt text that starts with `/`.

## Shell commands and file references

A single-line prompt that begins with `!` runs the rest of the line as a local
shell command when you press Enter, for example `! git status`. On a terminal
with color enabled, the `!` turns red as soon as you type the space. Ego does
not ask the model. The command uses the same exec permission, approval, sandbox and deny rules as the
model's commands. The command and its output become context for your next
prompt. Type `!!` to send text that starts with a literal `!` to the model.
A multiline prompt is never a shell command. While a turn is running, a `!`
command is refused and stays in the editor; submit it again when the turn ends.
Only text you type in the terminal runs this way: text from the model, a tool
or a replay never does. Headless and ACP prompts have no `!` escape.

Type `@` to pick a workspace file and insert its path; see the
[composer file picker](composer-file-picker.md).

## Files, PDFs and diffs

The `read` tool also reads PDF files: it returns the extracted text within the
usual limits of 2000 lines per file and 32 KiB per result, and a longer text
continues from the next offset. A PDF larger than 20 MiB is
refused, only the first 100 pages are extracted (the result says how many were
left out), and a malformed PDF is a tool error. The PDF is not sent to the
provider as a document.

Every file change shows a diff under its tool row: the added and removed line
counts and the first twelve diff rows. `/expand` shows the full diff. A new file
shows as added lines. Ego keeps a copy of the earlier content of at most 64 KiB
for this; above that size, for a file that is not text, and for a protected
credential file, the row says that the diff is omitted. The earlier content is
saved in the session and is not sent to the model.

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

Escape requests interruption while busy. Already queued explicit requests remain
queued. Native background commands, agents and MCP tasks that the stopped turn
started lose automatic wake authority; background work of earlier, completed
turns keeps it. Late results of the stopped turn cannot restart its work even after a newer
explicit request completes. Results remain available to inspections and later
explicit requests. Newly admitted background work can still notify normally.
An interrupt racing turn completion returns control instead of waiting on an idle
coordinator.
Ctrl+C is the interrupt path, and Ctrl+D exits from the idle driver. Pending permission/question selectors own
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

## Setup settings

`ego setup` and `/setup` list saved settings with short value previews. Select
`Search settings` to filter by key or description; clear the search to see all
settings. The `roots` row shows a count instead of a long serialized list.
Open it to select a saved root, add a root, or inspect the roots captured by
this session. Path and access edits affect only the selected saved entry. `Unset`
removes the entire saved roots value and inherits a lower layer; it is not the
same as removing one entry. Edits remain staged until `Review and save` is
confirmed. A running session keeps its captured file perimeter.

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

`/login` lists the built-in OAuth provider IDs. `/login PROVIDER` authorizes
one of these providers in the running session. For providers that support both
methods, select a device code or a loopback browser callback. Browser login
requires the callback to reach this host; forward the callback port when using
SSH. Press Ctrl-C to cancel an active login. Credentials are saved in the
normal auth store and are read on the next model turn. Configured credential
commands and auth plugins are not managed by `/login`; use `ego auth login`
for those sources.

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
