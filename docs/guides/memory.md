# Project instructions and memory

Read this page in the [hosted user guide](https://ego.tuicommander.com/docs/guides/memory.html).

Project instructions enter model context through workspace trust and prompt
admission. Workspace memory is a separate, opt-in file that the model can add to
(see [Workspace memory](#workspace-memory)).

## Initialize project instructions

In a trusted interactive workspace, run `/init` to review a proposal for
`AGENTS.md` and a relative `CLAUDE.md` symlink to it. Confirm only after reading
the proposed changes. Existing instruction content is preserved. An independent
`CLAUDE.md`, or a link to another target, requires explicit reconciliation first.

Initialization still needs write authority. It does not grant trust or widen
confinement. After an accepted change, use `/refresh` when idle to request ordinary
instruction admission in a new prompt epoch. Do not assume an already-sent
provider request sees edited files.

## Workspace memory

Memory is off. Turn it on with the `memory` row in `ego setup`,
`ego config set memory=true`, or `--memory` for one run. Only the user
configuration and the command line can set it.

When on, each workspace has one Markdown file under `$EGO_HOME/memory/`, limited
to 16 KiB, outside every workspace write scope. Only you and the model's `memory`
tool change it: the tool adds, replaces or removes one single-line `- ` entry and
is refused in plan mode. The file is read at session start and at `/refresh` and
appended to the system prompt after the instruction files. `/context` shows the
memory state. The full contract is [CLI.md §7.3](../../CLI.md).

## External memory tools

Ego also works with a memory store that you run yourself. Run your memory CLI
directly, or configure an external integration through
[generic hooks](../integrations/hooks.md). The external handler must implement
ego's hook input and result schema; hook JSON of other agents is not imported.

Source: [instruction initialization](../../crates/ego-cli/src/instruction_init.rs).
