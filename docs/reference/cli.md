# ego command reference

Generated from the clap definitions in `crates/ego-cli/src/cli.rs`.
Do not edit: `crates/ego-cli/tests/reference.rs` regenerates and compares it.

`ego PROMPT` with no subcommand starts an interactive session.

## `ego`

```text
Usage: ego [OPTIONS] [PROMPT] [COMMAND]
```

Run an agent, from one command line.

- `ego run` — Run an agent from a prompt, headless.
- `ego resume` — Continue a stored session.
- `ego continue` — Continue a stored session.
- `ego fork` — Fork a session at its tip or after the completed turn containing a message.
- `ego rm` — Delete a stored session (history is retained for descendants).
- `ego compact` — Compact a session into its deterministic successor.
- `ego export` — Export a session locally or as a structurally sanitized bundle.
- `ego ls` — List sessions (this workspace; --all for every one).
- `ego usage` — Report usage, context and cost for this workspace.
- `ego models` — List the shared model catalogue.
- `ego hooks` — Inspect the admitted system and user hook catalogue.
- `ego doctor` — Report a read-only redacted health snapshot of this installation.
- `ego checkpoint` — Preview or selectively restore session-owned Git file checkpoints.
- `ego setup` — Configure saved user settings in an interactive terminal.
- `ego acp` — Serve the Agent Client Protocol on stdio or an endpoint.
- `ego a2a` — Serve the Agent2Agent protocol on a local or loopback endpoint.
- `ego auth` — Manage subscription credentials.
- `ego mcp` — Manage configured MCP servers.
- `ego config` — Manage stored user configuration.
- `ego roots`
- `ego rules` — Inspect and manage persistent permission rules.
- `ego mcp-server` — Serve scoped ego runs over MCP on stdio.
- `ego worktree` — Inspect or clean up a retained local worktree.

- `--allow <RULE>` — Allow rule, e.g. 'read(src/**)'. Repeatable Repeatable.
- `--append-system <APPEND_SYSTEM>` — Append to the system prompt. `@file` reads it from a file
- `--artifact-dir <ARTIFACT_DIR>` — Override the sanitized run-artifact root for profile `ci`
- `--attach <IMAGE_PATH|HTTPS_IMAGE_URL>` — Add exact image bytes as ordered context before the prompt text Repeatable.
- `--bare` — No ambient discovery, even in a trusted workspace Default: `false`. Conflicts: `--instructions`.
- `--budget <BUDGET>` — Hard cap: '2.5$' or '400k' tokens
- `--context-clear` — Permit the model to discard conversation history using clear Default: `false`.
- `--context-interval <CONTEXT_INTERVAL>` — Minimum completed turns between model-requested transitions Default: `10`.
- `--continua` — Resume the latest saved session in this workspace without the terminal picker Default: `false`. Conflicts: `<PROMPT>`, `--file`, `--session`, `--session-id`, `--worktree`.
- `--deny <RULE>` — Deny rule, e.g. 'write(**)'. Repeatable Repeatable.
- `-C <DIR>` — Run as if started in DIR
- `--fallback <P/M>` — Ordered provider/model candidate used after a delivery-safe failure Repeatable.
- `-f, --file <FILE>` — Read the instruction and its invocation configuration from an agent file Conflicts: `<PROMPT>`.
- `--goal <GOAL>` — Set a checked session objective
- `--goal-check-command <GOAL_CHECK_COMMAND>` — Evaluator argv as a JSON string array; reads evidence JSON on stdin
- `--goal-turns <GOAL_TURNS>` — Maximum automatic goal continuation turns Default: `20`.
- `-h, --help` — Print help
- `--instructions <INSTRUCTIONS>` — Use this instruction file instead of discovering one
- `--max-output-tokens <MAX_OUTPUT_TOKENS>` — Maximum generated tokens per request
- `--max-turns <MAX_TURNS>` — Cap the turns this run may take
- `--mcp <NAME|@FILE>` — Select an effective server by name, even one marked enabled = false, or attach a strict invocation-scoped JSON MCP server Repeatable.
- `--mode <MODE>` — Permission mode: plan|default|auto|accept-edits|yolo (alias: edits)
- `-m, --model <MODEL>` — provider/model slug, e.g. ollama/qwen3
- `--no-color` — Never emit ANSI colour, whatever the terminal claims to support Default: `false`.
- `--no-context-tool` — Disable model-requested context transitions Default: `false`.
- `--no-tools` — Offer no tools at all Default: `false`. Conflicts: `--tools`.
- `--offline` — No non-loopback egress from ego's own HTTP transport Default: `false`.
- `--output <OUTPUT>` — text|json|jsonl Values: `text`, `json`, `jsonl`.
- `--output-style <NAME>` — Select a trusted prompt-only style: concise, explanatory, or NAME from EGO_HOME/output-styles/NAME.md
- `--plain` — Append plain output without terminal styling or live retry updates Default: `false`.
- `--plan` — Shorthand for --mode plan Default: `false`.
- `--profile <PROFILE>` — Layer [profile.NAME] from config
- `--proxy <http://HOST[:PORT]>` — Explicit HTTP CONNECT proxy for provider HTTPS (no ambient proxy discovery)
- `--proxy-ca <FILE>` — Additional PEM trust anchor for provider HTTPS through --proxy only
- `-q, --quiet` — Suppress progress and diagnostics on stderr; stdout is unaffected Default: `false`.
- `--read-only-dir <DIR>` — Grant reads of one existing canonical absolute upload directory
- `--retry <RETRY>` — Additional delivery-safe attempts for each candidate
- `--sandbox <SANDBOX>` — Filesystem confinement: ro|workspace|adaptive|off; off = no confinement, credentials included
- `--schema <SCHEMA>` — JSON Schema the final answer must validate against; `@file` or inline
- `--schema-retries <SCHEMA_RETRIES>` — How many times to re-prompt on a schema mismatch Default: `2`.
- `--session <SESSION>` — Named session: created or attached
- `--session-dir <SESSION_DIR>` — Override the store location
- `--session-id <SESSION_ID>` — A session id supplied by the caller
- `-c, --set <K=V>` — Dotted-path config override, value parsed as TOML Repeatable.
- `--system <SYSTEM>` — Replace the system prompt. `@file` reads it from a file
- `--thinking <THINKING>` — off|minimal|low|medium|high|xhigh|max
- `--timeout <TIMEOUT>` — Wall clock, e.g. '90s' or '10m'
- `--tools <TOOLS>` — Restrict the builtin tools to this comma-separated list Repeatable.
- `--trust-workspace` — Trust this launch's workspace without asking; remember the answer Default: `false`. Conflicts: `--bare`.
- `--var <KEY=VALUE>` — Supply one declared agent-file template variable. Requires --file Repeatable. Conflicts: `<PROMPT>`.
- `-V, --version` — Print version
- `--vim` — Use Vim normal/insert editing in the interactive composer Default: `false`.
- `--worktree <BRANCH>` — Create a retained managed worktree on this explicitly approved new branch Conflicts: `--session`, `--session-id`.
- `--yolo` — Shorthand for --mode yolo Default: `false`.
- `<PROMPT>` — The prompt. `-` reads it from stdin

## `ego run`

```text
Usage: ego run [OPTIONS] [PROMPT]
```

Run an agent from a prompt, headless.

- `--allow <RULE>` — Allow rule, e.g. 'read(src/**)'. Repeatable Repeatable.
- `--append-system <APPEND_SYSTEM>` — Append to the system prompt. `@file` reads it from a file
- `--artifact-dir <ARTIFACT_DIR>` — Override the sanitized run-artifact root for profile `ci`
- `--attach <IMAGE_PATH|HTTPS_IMAGE_URL>` — Add exact image bytes as ordered context before the prompt text Repeatable.
- `--bare` — No ambient discovery, even in a trusted workspace Default: `false`. Conflicts: `--instructions`.
- `--budget <BUDGET>` — Hard cap: '2.5$' or '400k' tokens
- `--context-clear` — Permit the model to discard conversation history using clear Default: `false`.
- `--context-interval <CONTEXT_INTERVAL>` — Minimum completed turns between model-requested transitions Default: `10`.
- `--deny <RULE>` — Deny rule, e.g. 'write(**)'. Repeatable Repeatable.
- `-C <DIR>` — Run as if started in DIR
- `--fallback <P/M>` — Ordered provider/model candidate used after a delivery-safe failure Repeatable.
- `-f, --file <FILE>` — Read the instruction and its invocation configuration from an agent file Conflicts: `<PROMPT>`.
- `--goal <GOAL>` — Set a checked session objective
- `--goal-check-command <GOAL_CHECK_COMMAND>` — Evaluator argv as a JSON string array; reads evidence JSON on stdin
- `--goal-turns <GOAL_TURNS>` — Maximum automatic goal continuation turns Default: `20`.
- `-h, --help` — Print help
- `--instructions <INSTRUCTIONS>` — Use this instruction file instead of discovering one
- `--max-output-tokens <MAX_OUTPUT_TOKENS>` — Maximum generated tokens per request
- `--max-turns <MAX_TURNS>` — Cap the turns this run may take
- `--mcp <NAME|@FILE>` — Select an effective server by name, even one marked enabled = false, or attach a strict invocation-scoped JSON MCP server Repeatable.
- `--mode <MODE>` — Permission mode: plan|default|auto|accept-edits|yolo (alias: edits)
- `-m, --model <MODEL>` — provider/model slug, e.g. ollama/qwen3
- `--no-color` — Never emit ANSI colour, whatever the terminal claims to support Default: `false`.
- `--no-context-tool` — Disable model-requested context transitions Default: `false`.
- `--no-tools` — Offer no tools at all Default: `false`. Conflicts: `--tools`.
- `--offline` — No non-loopback egress from ego's own HTTP transport Default: `false`.
- `--output <OUTPUT>` — text|json|jsonl Values: `text`, `json`, `jsonl`.
- `--output-style <NAME>` — Select a trusted prompt-only style: concise, explanatory, or NAME from EGO_HOME/output-styles/NAME.md
- `--plain` — Append plain output without terminal styling or live retry updates Default: `false`.
- `--plan` — Shorthand for --mode plan Default: `false`.
- `--profile <PROFILE>` — Layer [profile.NAME] from config
- `--proxy <http://HOST[:PORT]>` — Explicit HTTP CONNECT proxy for provider HTTPS (no ambient proxy discovery)
- `--proxy-ca <FILE>` — Additional PEM trust anchor for provider HTTPS through --proxy only
- `-q, --quiet` — Suppress progress and diagnostics on stderr; stdout is unaffected Default: `false`.
- `--read-only-dir <DIR>` — Grant reads of one existing canonical absolute upload directory
- `--retry <RETRY>` — Additional delivery-safe attempts for each candidate
- `--sandbox <SANDBOX>` — Filesystem confinement: ro|workspace|adaptive|off; off = no confinement, credentials included
- `--schema <SCHEMA>` — JSON Schema the final answer must validate against; `@file` or inline
- `--schema-retries <SCHEMA_RETRIES>` — How many times to re-prompt on a schema mismatch Default: `2`.
- `--session <SESSION>` — Named session: created or attached
- `--session-dir <SESSION_DIR>` — Override the store location
- `--session-id <SESSION_ID>` — A session id supplied by the caller
- `-c, --set <K=V>` — Dotted-path config override, value parsed as TOML Repeatable.
- `--system <SYSTEM>` — Replace the system prompt. `@file` reads it from a file
- `--thinking <THINKING>` — off|minimal|low|medium|high|xhigh|max
- `--timeout <TIMEOUT>` — Wall clock, e.g. '90s' or '10m'
- `--tools <TOOLS>` — Restrict the builtin tools to this comma-separated list Repeatable.
- `--trust-workspace` — Trust this launch's workspace without asking; remember the answer Default: `false`. Conflicts: `--bare`.
- `--var <KEY=VALUE>` — Supply one declared agent-file template variable. Requires --file Repeatable. Conflicts: `<PROMPT>`.
- `--worktree <BRANCH>` — Create a retained managed worktree on this explicitly approved new branch Conflicts: `--session`, `--session-id`.
- `--yolo` — Shorthand for --mode yolo Default: `false`.
- `<PROMPT>` — The prompt. `-` reads it from stdin

## `ego resume`

```text
Usage: ego resume [OPTIONS] [ID] [PROMPT]
```

Continue a stored session.

- `--allow <RULE>` — Allow rule, e.g. 'read(src/**)'. Repeatable Repeatable.
- `--answer <QUESTION_ID=JSON>` — Settle a pending question. Repeat for multiple sibling questions Repeatable.
- `--append-system <APPEND_SYSTEM>` — Append to the system prompt. `@file` reads it from a file
- `--artifact-dir <ARTIFACT_DIR>` — Override the sanitized run-artifact root for profile `ci`
- `--attach <IMAGE_PATH|HTTPS_IMAGE_URL>` — Add exact image bytes as ordered context before the prompt text Repeatable.
- `--bare` — No ambient discovery, even in a trusted workspace Default: `false`. Conflicts: `--instructions`.
- `--budget <BUDGET>` — Hard cap: '2.5$' or '400k' tokens
- `--context-clear` — Permit the model to discard conversation history using clear Default: `false`.
- `--context-interval <CONTEXT_INTERVAL>` — Minimum completed turns between model-requested transitions Default: `10`.
- `--deny <RULE>` — Deny rule, e.g. 'write(**)'. Repeatable Repeatable.
- `-C <DIR>` — Run as if started in DIR
- `--fallback <P/M>` — Ordered provider/model candidate used after a delivery-safe failure Repeatable.
- `-f, --file <FILE>` — Read the instruction and its invocation configuration from an agent file Conflicts: `<PROMPT>`.
- `--goal <GOAL>` — Set a checked session objective
- `--goal-check-command <GOAL_CHECK_COMMAND>` — Evaluator argv as a JSON string array; reads evidence JSON on stdin
- `--goal-operation <GOAL_OPERATION>` — Read or change goal metadata without starting a turn: show, set=TEXT, extend=N, achieve, block=REASON Conflicts: `--name`, `<PROMPT>`, `--answer`, `--file`, `--var`, `--attach`, `--model`, `--offline`, `--proxy`, `--proxy-ca`, `--sandbox`, `--thinking`, `--read-only-dir`, `--system`, `--bare`, `--append-system`, `--output-style`, `--instructions`, `--tools`, `--no-tools`, `--allow`, `--deny`, `--profile`, `--trust-workspace`, `--ephemeral`, `--artifact-dir`, `--schema`, `--schema-retries`, `--retry`, `--fallback`, `--budget`, `--max-output-tokens`, `--goal`, `--no-context-tool`, `--context-clear`, `--context-interval`, `--goal-turns`, `--goal-check-command`, `--max-turns`, `--timeout`, `--mcp`, `--mode`, `--plan`, `--yolo`, `--set`, `--quiet`, `--no-color`, `--plain`, `--vim`, `--session`, `--session-id`, `--worktree`.
- `--goal-turns <GOAL_TURNS>` — Maximum automatic goal continuation turns Default: `20`.
- `-h, --help` — Print help
- `--instructions <INSTRUCTIONS>` — Use this instruction file instead of discovering one
- `--max-output-tokens <MAX_OUTPUT_TOKENS>` — Maximum generated tokens per request
- `--max-turns <MAX_TURNS>` — Cap the turns this run may take
- `--mcp <NAME|@FILE>` — Select an effective server by name, even one marked enabled = false, or attach a strict invocation-scoped JSON MCP server Repeatable.
- `--mode <MODE>` — Permission mode: plan|default|auto|accept-edits|yolo (alias: edits)
- `-m, --model <MODEL>` — provider/model slug, e.g. ollama/qwen3
- `--name <NAME>` — Read or change the session title without starting a turn: show, set=TEXT, clear Conflicts: `--goal-operation`, `<PROMPT>`, `--answer`, `--file`, `--var`, `--attach`, `--model`, `--offline`, `--proxy`, `--proxy-ca`, `--sandbox`, `--thinking`, `--read-only-dir`, `--system`, `--bare`, `--append-system`, `--output-style`, `--instructions`, `--tools`, `--no-tools`, `--allow`, `--deny`, `--profile`, `--trust-workspace`, `--ephemeral`, `--artifact-dir`, `--schema`, `--schema-retries`, `--retry`, `--fallback`, `--budget`, `--max-output-tokens`, `--goal`, `--no-context-tool`, `--context-clear`, `--context-interval`, `--goal-turns`, `--goal-check-command`, `--max-turns`, `--timeout`, `--mcp`, `--mode`, `--plan`, `--yolo`, `--set`, `--quiet`, `--no-color`, `--plain`, `--vim`, `--session`, `--session-id`, `--worktree`.
- `--no-color` — Never emit ANSI colour, whatever the terminal claims to support Default: `false`.
- `--no-context-tool` — Disable model-requested context transitions Default: `false`.
- `--no-tools` — Offer no tools at all Default: `false`. Conflicts: `--tools`.
- `--offline` — No non-loopback egress from ego's own HTTP transport Default: `false`.
- `--output <OUTPUT>` — text|json|jsonl Values: `text`, `json`, `jsonl`.
- `--output-style <NAME>` — Select a trusted prompt-only style: concise, explanatory, or NAME from EGO_HOME/output-styles/NAME.md
- `--plain` — Append plain output without terminal styling or live retry updates Default: `false`.
- `--plan` — Shorthand for --mode plan Default: `false`.
- `--profile <PROFILE>` — Layer [profile.NAME] from config
- `--proxy <http://HOST[:PORT]>` — Explicit HTTP CONNECT proxy for provider HTTPS (no ambient proxy discovery)
- `--proxy-ca <FILE>` — Additional PEM trust anchor for provider HTTPS through --proxy only
- `-q, --quiet` — Suppress progress and diagnostics on stderr; stdout is unaffected Default: `false`.
- `--read-only-dir <DIR>` — Grant reads of one existing canonical absolute upload directory
- `--retry <RETRY>` — Additional delivery-safe attempts for each candidate
- `--sandbox <SANDBOX>` — Filesystem confinement: ro|workspace|adaptive|off; off = no confinement, credentials included
- `--schema <SCHEMA>` — JSON Schema the final answer must validate against; `@file` or inline
- `--schema-retries <SCHEMA_RETRIES>` — How many times to re-prompt on a schema mismatch Default: `2`.
- `--session-dir <SESSION_DIR>` — Override the store location
- `-c, --set <K=V>` — Dotted-path config override, value parsed as TOML Repeatable.
- `--system <SYSTEM>` — Replace the system prompt. `@file` reads it from a file
- `--thinking <THINKING>` — off|minimal|low|medium|high|xhigh|max
- `--timeout <TIMEOUT>` — Wall clock, e.g. '90s' or '10m'
- `--tools <TOOLS>` — Restrict the builtin tools to this comma-separated list Repeatable.
- `--trust-workspace` — Trust this launch's workspace without asking; remember the answer Default: `false`. Conflicts: `--bare`.
- `--var <KEY=VALUE>` — Supply one declared agent-file template variable. Requires --file Repeatable. Conflicts: `<PROMPT>`.
- `--vim` — Use Vim normal/insert editing in the interactive composer Default: `false`.
- `--yolo` — Shorthand for --mode yolo Default: `false`.
- `<ID>` — Session UUID; omit it for the terminal picker or headless latest in this workspace
- `<PROMPT>` — The prompt. `-` reads it from stdin

`ego continue` shares the saved-session admission and options of `ego resume`.
In the terminal use `/resume [ID]` or `/continue`. Current execution authority
is measured before effects: narrower authority is automatic; potentially
wider authority requires explicit human consent. Headless callers cannot
approve wider authority.

With terminal stdin and stdout, `ego resume ID "next task"` opens the
interactive seat, submits the task once, and accepts further messages.
Explicit `--output` (including `text`), stdin `-`, `--answer`, an agent file,
or profile `ci` selects the headless driver. Redirected stdin or stdout
also selects headless execution. `EGO_PROFILE=ci` has the same effect.

Headless execution never asks for permission: an unresolved approval exits 3.
Pre-approve only the required operations in trusted CI profile configuration:

```toml
[profile.ci]
model = "ollama/qwen3"
session_dir = "/absolute/ci-sessions"
artifact_dir = "/absolute/ci-artifacts"

[profile.ci.permissions]
allow = ["exec(seq 1 3)"]
```

Pre-create both roots outside the workspace and outside each other.
Supply a non-interactive provider credential when the model requires one.
Then run `ego resume ID "next task" --profile ci --instructions ci.md`.
Allow rules grant policy approval; sandbox and system ceilings still apply.

## `ego continue`

```text
Usage: ego continue [OPTIONS] [ID] [PROMPT]
```

Continue a stored session.

- `--allow <RULE>` — Allow rule, e.g. 'read(src/**)'. Repeatable Repeatable.
- `--answer <QUESTION_ID=JSON>` — Settle a pending question. Repeat for multiple sibling questions Repeatable.
- `--append-system <APPEND_SYSTEM>` — Append to the system prompt. `@file` reads it from a file
- `--artifact-dir <ARTIFACT_DIR>` — Override the sanitized run-artifact root for profile `ci`
- `--attach <IMAGE_PATH|HTTPS_IMAGE_URL>` — Add exact image bytes as ordered context before the prompt text Repeatable.
- `--bare` — No ambient discovery, even in a trusted workspace Default: `false`. Conflicts: `--instructions`.
- `--budget <BUDGET>` — Hard cap: '2.5$' or '400k' tokens
- `--context-clear` — Permit the model to discard conversation history using clear Default: `false`.
- `--context-interval <CONTEXT_INTERVAL>` — Minimum completed turns between model-requested transitions Default: `10`.
- `--deny <RULE>` — Deny rule, e.g. 'write(**)'. Repeatable Repeatable.
- `-C <DIR>` — Run as if started in DIR
- `--fallback <P/M>` — Ordered provider/model candidate used after a delivery-safe failure Repeatable.
- `-f, --file <FILE>` — Read the instruction and its invocation configuration from an agent file Conflicts: `<PROMPT>`.
- `--goal <GOAL>` — Set a checked session objective
- `--goal-check-command <GOAL_CHECK_COMMAND>` — Evaluator argv as a JSON string array; reads evidence JSON on stdin
- `--goal-operation <GOAL_OPERATION>` — Read or change goal metadata without starting a turn: show, set=TEXT, extend=N, achieve, block=REASON Conflicts: `--name`, `<PROMPT>`, `--answer`, `--file`, `--var`, `--attach`, `--model`, `--offline`, `--proxy`, `--proxy-ca`, `--sandbox`, `--thinking`, `--read-only-dir`, `--system`, `--bare`, `--append-system`, `--output-style`, `--instructions`, `--tools`, `--no-tools`, `--allow`, `--deny`, `--profile`, `--trust-workspace`, `--ephemeral`, `--artifact-dir`, `--schema`, `--schema-retries`, `--retry`, `--fallback`, `--budget`, `--max-output-tokens`, `--goal`, `--no-context-tool`, `--context-clear`, `--context-interval`, `--goal-turns`, `--goal-check-command`, `--max-turns`, `--timeout`, `--mcp`, `--mode`, `--plan`, `--yolo`, `--set`, `--quiet`, `--no-color`, `--plain`, `--vim`, `--session`, `--session-id`, `--worktree`.
- `--goal-turns <GOAL_TURNS>` — Maximum automatic goal continuation turns Default: `20`.
- `-h, --help` — Print help
- `--instructions <INSTRUCTIONS>` — Use this instruction file instead of discovering one
- `--max-output-tokens <MAX_OUTPUT_TOKENS>` — Maximum generated tokens per request
- `--max-turns <MAX_TURNS>` — Cap the turns this run may take
- `--mcp <NAME|@FILE>` — Select an effective server by name, even one marked enabled = false, or attach a strict invocation-scoped JSON MCP server Repeatable.
- `--mode <MODE>` — Permission mode: plan|default|auto|accept-edits|yolo (alias: edits)
- `-m, --model <MODEL>` — provider/model slug, e.g. ollama/qwen3
- `--name <NAME>` — Read or change the session title without starting a turn: show, set=TEXT, clear Conflicts: `--goal-operation`, `<PROMPT>`, `--answer`, `--file`, `--var`, `--attach`, `--model`, `--offline`, `--proxy`, `--proxy-ca`, `--sandbox`, `--thinking`, `--read-only-dir`, `--system`, `--bare`, `--append-system`, `--output-style`, `--instructions`, `--tools`, `--no-tools`, `--allow`, `--deny`, `--profile`, `--trust-workspace`, `--ephemeral`, `--artifact-dir`, `--schema`, `--schema-retries`, `--retry`, `--fallback`, `--budget`, `--max-output-tokens`, `--goal`, `--no-context-tool`, `--context-clear`, `--context-interval`, `--goal-turns`, `--goal-check-command`, `--max-turns`, `--timeout`, `--mcp`, `--mode`, `--plan`, `--yolo`, `--set`, `--quiet`, `--no-color`, `--plain`, `--vim`, `--session`, `--session-id`, `--worktree`.
- `--no-color` — Never emit ANSI colour, whatever the terminal claims to support Default: `false`.
- `--no-context-tool` — Disable model-requested context transitions Default: `false`.
- `--no-tools` — Offer no tools at all Default: `false`. Conflicts: `--tools`.
- `--offline` — No non-loopback egress from ego's own HTTP transport Default: `false`.
- `--output <OUTPUT>` — text|json|jsonl Values: `text`, `json`, `jsonl`.
- `--output-style <NAME>` — Select a trusted prompt-only style: concise, explanatory, or NAME from EGO_HOME/output-styles/NAME.md
- `--plain` — Append plain output without terminal styling or live retry updates Default: `false`.
- `--plan` — Shorthand for --mode plan Default: `false`.
- `--profile <PROFILE>` — Layer [profile.NAME] from config
- `--proxy <http://HOST[:PORT]>` — Explicit HTTP CONNECT proxy for provider HTTPS (no ambient proxy discovery)
- `--proxy-ca <FILE>` — Additional PEM trust anchor for provider HTTPS through --proxy only
- `-q, --quiet` — Suppress progress and diagnostics on stderr; stdout is unaffected Default: `false`.
- `--read-only-dir <DIR>` — Grant reads of one existing canonical absolute upload directory
- `--retry <RETRY>` — Additional delivery-safe attempts for each candidate
- `--sandbox <SANDBOX>` — Filesystem confinement: ro|workspace|adaptive|off; off = no confinement, credentials included
- `--schema <SCHEMA>` — JSON Schema the final answer must validate against; `@file` or inline
- `--schema-retries <SCHEMA_RETRIES>` — How many times to re-prompt on a schema mismatch Default: `2`.
- `--session-dir <SESSION_DIR>` — Override the store location
- `-c, --set <K=V>` — Dotted-path config override, value parsed as TOML Repeatable.
- `--system <SYSTEM>` — Replace the system prompt. `@file` reads it from a file
- `--thinking <THINKING>` — off|minimal|low|medium|high|xhigh|max
- `--timeout <TIMEOUT>` — Wall clock, e.g. '90s' or '10m'
- `--tools <TOOLS>` — Restrict the builtin tools to this comma-separated list Repeatable.
- `--trust-workspace` — Trust this launch's workspace without asking; remember the answer Default: `false`. Conflicts: `--bare`.
- `--var <KEY=VALUE>` — Supply one declared agent-file template variable. Requires --file Repeatable. Conflicts: `<PROMPT>`.
- `--vim` — Use Vim normal/insert editing in the interactive composer Default: `false`.
- `--yolo` — Shorthand for --mode yolo Default: `false`.
- `<ID>` — Session UUID; omit it for the terminal picker or headless latest in this workspace
- `<PROMPT>` — The prompt. `-` reads it from stdin

`ego continue` shares the saved-session admission and options of `ego resume`.
In the terminal use `/resume [ID]` or `/continue`. Current execution authority
is measured before effects: narrower authority is automatic; potentially
wider authority requires explicit human consent. Headless callers cannot
approve wider authority.

With terminal stdin and stdout, `ego resume ID "next task"` opens the
interactive seat, submits the task once, and accepts further messages.
Explicit `--output` (including `text`), stdin `-`, `--answer`, an agent file,
or profile `ci` selects the headless driver. Redirected stdin or stdout
also selects headless execution. `EGO_PROFILE=ci` has the same effect.

Headless execution never asks for permission: an unresolved approval exits 3.
Pre-approve only the required operations in trusted CI profile configuration:

```toml
[profile.ci]
model = "ollama/qwen3"
session_dir = "/absolute/ci-sessions"
artifact_dir = "/absolute/ci-artifacts"

[profile.ci.permissions]
allow = ["exec(seq 1 3)"]
```

Pre-create both roots outside the workspace and outside each other.
Supply a non-interactive provider credential when the model requires one.
Then run `ego resume ID "next task" --profile ci --instructions ci.md`.
Allow rules grant policy approval; sandbox and system ceilings still apply.

## `ego fork`

```text
Usage: ego fork [OPTIONS] <SOURCE>
```

Fork a session at its tip or after the completed turn containing a message.

- `--at <ITEM_ID>` — (undocumented)
- `-C <DIR>` — Run as if started in DIR
- `-h, --help` — Print help
- `--output <OUTPUT>` — text|json|jsonl Values: `text`, `json`, `jsonl`.
- `--session-dir <SESSION_DIR>` — Override the store location
- `<SOURCE>` — (undocumented) Required.

## `ego rm`

```text
Usage: ego rm [OPTIONS] <SOURCE>
```

Delete a stored session (history is retained for descendants).

- `-C <DIR>` — Run as if started in DIR
- `--force` — Acknowledge an uncertain prior run; does not replace deletion confirmation and is not a general override Default: `false`.
- `-h, --help` — Print help
- `--session-dir <SESSION_DIR>` — Override the store location
- `--yes` — Supply deletion confirmation without prompting; required without terminal stdin Default: `false`.
- `<SOURCE>` — Stored session ID to delete; history needed by descendants is retained Required.

## `ego compact`

```text
Usage: ego compact [OPTIONS] <SOURCE>
```

Compact a session into its deterministic successor.

- `--budget <BUDGET>` — Hard spend ceiling, as for run
- `-C <DIR>` — Run as if started in DIR
- `-h, --help` — Print help
- `--max-output-tokens <MAX_OUTPUT_TOKENS>` — Maximum generated tokens per request
- `--output <OUTPUT>` — text|json|jsonl Values: `text`, `json`, `jsonl`.
- `--request-id <REQUEST_ID>` — (undocumented)
- `--session-dir <SESSION_DIR>` — Override the store location
- `<SOURCE>` — (undocumented) Required.

## `ego export`

```text
Usage: ego export [OPTIONS] <SOURCE>
```

Export a session locally or as a structurally sanitized bundle.

- `--after <SEQ>` — Only emit transcript events after this session-local sequence
- `-C <DIR>` — Run as if started in DIR
- `--file <PATH>` — Write to PATH instead of stdout; its parent must exist. Refuse symlinks, non-regular targets and existing files with different bytes; identical bytes are accepted
- `--follow` — Follow committed local transcript events until stopped Default: `false`.
- `--format <FORMAT>` — Transcript format; JSONL is stdout-only and cannot be combined with --file Default: `md`. Values: `md`, `json`, `jsonl`, `transcript`.
- `-h, --help` — Print help
- `--sanitize` — Remove private content structurally; exports are unsanitized by default Default: `false`.
- `--session-dir <SESSION_DIR>` — Override the store location
- `<SOURCE>` — Stored session ID to export Required.

## `ego ls`

```text
Usage: ego ls [OPTIONS]
```

List sessions (this workspace; --all for every one).

- `--all` — Every session in the store, not just this workspace's Default: `false`.
- `-C <DIR>` — Run as if started in DIR
- `-h, --help` — Print help
- `--output <OUTPUT>` — text|json|jsonl Values: `text`, `json`, `jsonl`.
- `--session-dir <SESSION_DIR>` — Override the store location

## `ego usage`

```text
Usage: ego usage [OPTIONS]
```

Report usage, context and cost for this workspace.

- `--all-roles` — Keep role rows that recorded no activity in the text table Default: `false`.
- `-C <DIR>` — Run as if started in DIR
- `-h, --help` — Print help
- `--json` — Emit the exact versioned usage-report object Default: `false`.
- `--period <PERIOD>` — all|Ns|Nm|Nh Default: `all`.
- `--session-dir <SESSION_DIR>` — Override the store location

## `ego models`

```text
Usage: ego models [OPTIONS]
```

List the shared model catalogue.

- `-h, --help` — Print help
- `--json` — Emit the exact versioned snapshot object Default: `false`.
- `--provider <PROVIDER>` — Restrict enumeration and output to one admitted provider
- `--refresh` — Refresh the selected discoverable provider slices Default: `false`.

## `ego hooks`

```text
Usage: ego hooks <COMMAND>
```

Inspect the admitted system and user hook catalogue.

- `ego hooks ls` — List the admitted hook catalogue without changing configuration

- `-h, --help` — Print help

## `ego hooks ls`

```text
Usage: ego hooks ls [OPTIONS]
```

List the admitted hook catalogue without changing configuration

- `-h, --help` — Print help
- `--json` — Emit the exact versioned hook-listing object Default: `false`.

## `ego doctor`

```text
Usage: ego doctor [OPTIONS]
```

Report a read-only redacted health snapshot of this installation.

- `-h, --help` — Print help
- `--json` — Emit the exact versioned health snapshot Default: `false`.

## `ego checkpoint`

```text
Usage: ego checkpoint [OPTIONS] <SESSION> <COMMAND>
```

Preview or selectively restore session-owned Git file checkpoints.

- `ego checkpoint preview` — List checkpoint IDs, paths, effects, and external-change conflicts as JSON
- `ego checkpoint restore` — Restore one selected checkpoint; external changes are never forced
- `ego checkpoint close` — Close checkpoint ownership and delete the session's private Git ref

- `-C, --directory <DIRECTORY>` — (undocumented) Default: `.`.
- `-h, --help` — Print help
- `<SESSION>` — (undocumented) Required.

## `ego checkpoint preview`

```text
Usage: ego checkpoint <SESSION> preview
```

List checkpoint IDs, paths, effects, and external-change conflicts as JSON

- `-h, --help` — Print help

## `ego checkpoint restore`

```text
Usage: ego checkpoint <SESSION> restore <CHECKPOINT>
```

Restore one selected checkpoint; external changes are never forced

- `-h, --help` — Print help
- `<CHECKPOINT>` — (undocumented) Required.

## `ego checkpoint close`

```text
Usage: ego checkpoint <SESSION> close
```

Close checkpoint ownership and delete the session's private Git ref

- `-h, --help` — Print help

## `ego setup`

```text
Usage: ego setup [OPTIONS]
```

Configure saved user settings in an interactive terminal.

- `-C, --directory <DIRECTORY>` — Resolve effective settings for this workspace
- `-h, --help` — Print help
- `--profile <PROFILE>` — Edit one user profile rather than the base user settings

## `ego acp`

```text
Usage: ego acp [OPTIONS]
```

Serve the Agent Client Protocol on stdio or an endpoint.

- `--allow <RULE>` — Allow rule, e.g. 'read(src/**)'. Repeatable Repeatable.
- `--allow-public-bind` — Serve on an address reachable from outside this machine Default: `false`.
- `--append-system <APPEND_SYSTEM>` — Append to the system prompt. `@file` reads it from a file
- `--bare` — No ambient discovery, even in a trusted workspace Default: `false`. Conflicts: `--instructions`.
- `--connection-lifetime <CONNECTION_LIFETIME>` — Maximum lifetime of one listener connection: 90s, 10m or 2h (runtime default: 24h)
- `--deny <RULE>` — Deny rule, e.g. 'write(**)'. Repeatable Repeatable.
- `-C <DIR>` — Run as if started in DIR
- `-h, --help` — Print help
- `--instructions <INSTRUCTIONS>` — Use this instruction file instead of discovering one
- `--listen <LISTEN>` — Serve on a socket path or address instead of stdio
- `--max-connections <MAX_CONNECTIONS>` — Maximum simultaneous listener connections (runtime default: 32)
- `--max-ingress-bytes <MAX_INGRESS_BYTES>` — Maximum cumulative bytes read from one listener connection (runtime default: 134217728 bytes)
- `--no-tools` — Offer no tools at all Default: `false`. Conflicts: `--tools`.
- `--output-style <NAME>` — Select a trusted prompt-only style: concise, explanatory, or NAME from EGO_HOME/output-styles/NAME.md
- `--profile <PROFILE>` — Layer [profile.NAME] from config
- `--read-only-dir <DIR>` — Grant reads of one existing canonical absolute upload directory
- `--sandbox <SANDBOX>` — The weakest confinement a session may select: ro|workspace|adaptive|off; off = no confinement, credentials included
- `--session-dir <SESSION_DIR>` — Override the store location
- `--system <SYSTEM>` — Replace the system prompt. `@file` reads it from a file
- `--tools <TOOLS>` — Restrict the builtin tools to this comma-separated list Repeatable.

Prompt flags use the same composition as `ego run`. For example:

```sh
ego acp --system @agent-prompt.txt --append-system "Use concise answers" --bare
```

`--bare` and `--instructions` conflict. ACP hosts can also pass per-session
`_meta.ego.systemPrompt` (`mode` and `text`) and `_meta.ego.bare` in `session/new`.
See [session initialization prompt](../integrations/acp.md#session-initialization-prompt)
for the wire example and launch-default precedence.

## `ego a2a`

```text
Usage: ego a2a [OPTIONS] --listen <LISTEN>
```

Serve the Agent2Agent protocol on a local or loopback endpoint.

- `--allow-public-bind` — Serve on an address reachable from outside this machine Default: `false`.
- `-C <DIR>` — Run as if started in DIR
- `-h, --help` — Print help (see more with '--help')
- `--listen <LISTEN>` — Where to serve: a socket path, or an address such as `127.0.0.1:8080` Required.
- `--sandbox <SANDBOX>` — The weakest confinement a session may select: ro|workspace|adaptive|off; off = no confinement, credentials included
- `--session-dir <SESSION_DIR>` — Override the store location

## `ego auth`

```text
Usage: ego auth <COMMAND>
```

Manage subscription credentials.

- `ego auth login` — Authenticate and store a provider credential
- `ego auth logout` — Remove a stored provider credential
- `ego auth status` — Show credential type and expiry, never token values

- `-h, --help` — Print help

## `ego auth login`

```text
Usage: ego auth login [OPTIONS] [PROVIDER]
```

Authenticate and store a provider credential

- `--device` — Prefer device-code login when the provider offers it Default: `false`.
- `-h, --help` — Print help
- `<PROVIDER>` — (undocumented)

## `ego auth logout`

```text
Usage: ego auth logout [PROVIDER]
```

Remove a stored provider credential

- `-h, --help` — Print help
- `<PROVIDER>` — (undocumented)

## `ego auth status`

```text
Usage: ego auth status [PROVIDER]
```

Show credential type and expiry, never token values

- `-h, --help` — Print help
- `<PROVIDER>` — (undocumented)

## `ego mcp`

```text
Usage: ego mcp <COMMAND>
```

Manage configured MCP servers.

- `ego mcp add` — Add or replace one complete server entry
- `ego mcp rm` — Remove, mask, or reveal one configured server entry
- `ego mcp ls` — List configured and admitted MCP state
- `ego mcp login` — Authorize a public OAuth client with an explicit registration strategy
- `ego mcp reload` — Reconcile every configured server, or one named server

- `-h, --help` — Print help

## `ego mcp add`

```text
Usage: ego mcp add <NAME> --stdio [--cwd <DIR>] [--inherit-env <NAME>]... -- <COMMAND> [ARG]...
       ego mcp add <NAME> --http <URL> [--auth <none|oauth>] [--allow-insecure-loopback]
```

Add or replace one complete server entry

- `--allow-insecure-loopback` — Consent to plaintext HTTP for an exact loopback host Default: `false`. Conflicts: `--stdio`.
- `--auth <AUTH>` — HTTP authentication mode Values: `none`, `oauth`. Conflicts: `--stdio`.
- `--cwd <DIR>` — Working directory for the stdio child Conflicts: `--http`.
- `-h, --help` — Print help
- `--http <URL>` — Configure a Streamable HTTP transport Conflicts: `--stdio`.
- `--inherit-env <NAME>` — Environment variable inherited by the stdio child. Repeatable Repeatable. Conflicts: `--http`.
- `--profile <PROFILE>` — Explicit HTTP wire profile; never selected by fallback Values: `current`, `legacy-sse`. Conflicts: `--stdio`.
- `--stdio` — Configure a subprocess transport. Its command follows literal `--` Default: `false`. Conflicts: `--http`.
- `<NAME>` — (undocumented) Required.

## `ego mcp rm`

```text
Usage: ego mcp rm <NAME>
```

Remove, mask, or reveal one configured server entry

- `-h, --help` — Print help
- `<NAME>` — (undocumented) Required.

## `ego mcp ls`

```text
Usage: ego mcp ls [OPTIONS]
```

List configured and admitted MCP state

- `-h, --help` — Print help
- `--json` — Emit the typed machine-readable projection Default: `false`.

## `ego mcp login`

```text
Usage: ego mcp login [OPTIONS] <NAME>
```

Authorize a public OAuth client with an explicit registration strategy

- `--account <ACCOUNT>` — (undocumented)
- `--callback-port <CALLBACK_PORT>` — Loopback port registered with the authorization server (0 selects a free port) Default: `0`.
- `--cimd <CIMD>` — Published HTTPS client metadata document URL Conflicts: `--client-id`, `--dcr`.
- `--client-id <CLIENT_ID>` — Operator-issued public client ID Conflicts: `--cimd`, `--dcr`.
- `--dcr` — Explicitly use issuer-advertised dynamic registration Default: `false`. Conflicts: `--client-id`, `--cimd`.
- `-h, --help` — Print help
- `--issuer <ISSUER>` — (undocumented)
- `--refresh` — Refresh an existing issuer/resource/account-bound credential without interaction Default: `false`.
- `<NAME>` — (undocumented) Required.

## `ego mcp reload`

```text
Usage: ego mcp reload [NAME]
```

Reconcile every configured server, or one named server

- `-h, --help` — Print help
- `<NAME>` — (undocumented)

## `ego config`

```text
Usage: ego config <COMMAND>
```

Manage stored user configuration.

- `ego config capabilities` — Inspect managed owner transaction capabilities as JSON
- `ego config snapshot` — Read stored values and their opaque owner revision as JSON
- `ego config batch` — Atomically publish a bounded set of typed managed edits
- `ego config get` — Read one exact stored value
- `ego config set` — Store one strictly typed TOML value
- `ego config unset` — Remove one exact stored value
- `ego config ls` — List the exact stored managed projection
- `ego config edit` — Edit the complete user document in the selected external editor

- `-h, --help` — Print help

## `ego config capabilities`

```text
Usage: ego config capabilities
```

Inspect managed owner transaction capabilities as JSON

- `-h, --help` — Print help

## `ego config snapshot`

```text
Usage: ego config snapshot [OPTIONS]
```

Read stored values and their opaque owner revision as JSON

- `-h, --help` — Print help
- `--profile <NAME>` — (undocumented)

## `ego config batch`

```text
Usage: ego config batch [OPTIONS]
```

Atomically publish a bounded set of typed managed edits

- `-h, --help` — Print help
- `--if-revision <REVISION>` — (undocumented)
- `--profile <NAME>` — (undocumented)
- `--set <KEY=TOML_VALUE>` — (undocumented) Repeatable.
- `--unset <KEY>` — (undocumented) Repeatable.

## `ego config get`

```text
Usage: ego config get [OPTIONS] <KEY>
```

Read one exact stored value

- `-h, --help` — Print help
- `--json` — (undocumented) Default: `false`.
- `--profile <NAME>` — (undocumented)
- `<KEY>` — (undocumented) Required.

## `ego config set`

```text
Usage: ego config set [OPTIONS] <KEY=TOML_VALUE>
```

Store one strictly typed TOML value

- `-h, --help` — Print help
- `--profile <NAME>` — (undocumented)
- `<KEY=TOML_VALUE>` — (undocumented) Required.

## `ego config unset`

```text
Usage: ego config unset [OPTIONS] <KEY>
```

Remove one exact stored value

- `-h, --help` — Print help
- `--profile <NAME>` — (undocumented)
- `<KEY>` — (undocumented) Required.

## `ego config ls`

```text
Usage: ego config ls [OPTIONS]
```

List the exact stored managed projection

- `--effective` — Show the effective run perimeter instead of stored values Default: `false`.
- `-h, --help` — Print help
- `--json` — (undocumented) Default: `false`.
- `--profile <NAME>` — (undocumented)

## `ego config edit`

```text
Usage: ego config edit
```

Edit the complete user document in the selected external editor

- `-h, --help` — Print help

## `ego roots`

```text
Usage: ego roots <COMMAND>
```

- `ego roots add` — Add or update one user perimeter root
- `ego roots rm` — Remove a root by its stored spelling
- `ego roots ls` — List roots stored in the selected user target

- `-h, --help` — Print help

## `ego roots add`

```text
Usage: ego roots add [OPTIONS] <PATH>
```

Add or update one user perimeter root

- `--access <ACCESS>` — (undocumented) Default: `read`. Values: `read`, `read-write`.
- `-h, --help` — Print help
- `--profile <PROFILE>` — (undocumented)
- `<PATH>` — (undocumented) Required.

## `ego roots rm`

```text
Usage: ego roots rm [OPTIONS] <PATH>
```

Remove a root by its stored spelling

- `-h, --help` — Print help
- `--profile <PROFILE>` — (undocumented)
- `<PATH>` — (undocumented) Required.

## `ego roots ls`

```text
Usage: ego roots ls [OPTIONS]
```

List roots stored in the selected user target

- `-h, --help` — Print help
- `--json` — (undocumented) Default: `false`.
- `--profile <PROFILE>` — (undocumented)

## `ego rules`

```text
Usage: ego rules <COMMAND>
```

Inspect and manage persistent permission rules.

- `ego rules ls` — List the effective system and selected user rules
- `ego rules allow` — Persist one allow predicate
- `ego rules deny` — Persist one deny predicate
- `ego rules revoke` — Remove one predicate ID from both selected user lists
- `ego rules remember` — Derive persistent rules from one prepared builtin call
- `ego rules explain` — Explain one prepared builtin call without running it

- `-h, --help` — Print help

## `ego rules ls`

```text
Usage: ego rules ls [OPTIONS]
```

List the effective system and selected user rules

- `-h, --help` — Print help
- `--json` — (undocumented) Default: `false`.
- `--profile <NAME>` — (undocumented)

## `ego rules allow`

```text
Usage: ego rules allow [OPTIONS] <RULE>
```

Persist one allow predicate

- `-h, --help` — Print help
- `--profile <NAME>` — (undocumented)
- `<RULE>` — (undocumented) Required.

## `ego rules deny`

```text
Usage: ego rules deny [OPTIONS] <RULE>
```

Persist one deny predicate

- `-h, --help` — Print help
- `--profile <NAME>` — (undocumented)
- `<RULE>` — (undocumented) Required.

## `ego rules revoke`

```text
Usage: ego rules revoke [OPTIONS] <ID>
```

Remove one predicate ID from both selected user lists

- `-h, --help` — Print help
- `--profile <NAME>` — (undocumented)
- `<ID>` — (undocumented) Required.

## `ego rules remember`

```text
Usage: ego rules remember [OPTIONS] <EFFECT> <TOOL> <ARGS_JSON>
```

Derive persistent rules from one prepared builtin call

- `-C <DIR>` — (undocumented)
- `-h, --help` — Print help
- `--profile <NAME>` — (undocumented)
- `--yes` — (undocumented) Default: `false`.
- `<ARGS_JSON>` — (undocumented) Required.
- `<EFFECT>` — (undocumented) Required.
- `<TOOL>` — (undocumented) Required.

## `ego rules explain`

```text
Usage: ego rules explain [OPTIONS] <TOOL> <ARGS_JSON>
```

Explain one prepared builtin call without running it

- `-C <DIR>` — (undocumented)
- `-h, --help` — Print help
- `--json` — (undocumented) Default: `false`.
- `--profile <NAME>` — (undocumented)
- `<ARGS_JSON>` — (undocumented) Required.
- `<TOOL>` — (undocumented) Required.

## `ego mcp-server`

```text
Usage: ego mcp-server [OPTIONS] --max-concurrent-runs <MAX_CONCURRENT_RUNS> --retained-requests <RETAINED_REQUESTS> --max-resource-bytes <MAX_RESOURCE_BYTES>
```

Serve scoped ego runs over MCP on stdio.

- `-C <DIR>` — Run as if started in DIR
- `-h, --help` — Print help
- `--max-concurrent-runs <MAX_CONCURRENT_RUNS>` — (undocumented) Required.
- `--max-resource-bytes <MAX_RESOURCE_BYTES>` — (undocumented) Required.
- `--retained-requests <RETAINED_REQUESTS>` — (undocumented) Required.
- `--session-dir <SESSION_DIR>` — Override the store location

## `ego worktree`

```text
Usage: ego worktree <COMMAND>
```

Inspect or clean up a retained local worktree.

- `ego worktree show` — Inspect a durable local worktree receipt without starting a model
- `ego worktree cleanup` — Remove only a clean worktree whose commits are integrated; retain its branch

- `-h, --help` — Print help

## `ego worktree show`

```text
Usage: ego worktree show <RECEIPT>
```

Inspect a durable local worktree receipt without starting a model

- `-h, --help` — Print help
- `<RECEIPT>` — (undocumented) Required.

## `ego worktree cleanup`

```text
Usage: ego worktree cleanup <RECEIPT>
```

Remove only a clean worktree whose commits are integrated; retain its branch

- `-h, --help` — Print help
- `<RECEIPT>` — (undocumented) Required.

