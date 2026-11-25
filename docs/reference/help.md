# CLI help from a verified binary

Captured from `ego 0.4.14` on 2026-11-19. Binary SHA-256: `e144c6c1ebe03f1bf47ad7786f5ecafe973cf4ecabb86da8d5207b813de3f1bf`.

This is a dated help-only snapshot, not a separate command specification. The [generated reference](cli.md) follows the source parser. For your installed binary, use `ego COMMAND SUBCOMMAND --help`. The historical `ego auth migrate` shown below has since been removed; see [multiple accounts](../guides/providers.md#multiple-accounts) for current login and recovery.

## `ego --help`

```text
ego 0.4.14 — an agent you run from a terminal.

Usage:
  ego [OPTIONS] [PROMPT]
  ego <COMMAND> [OPTIONS]

Interactive:
  ego -m ollama/qwen3

Commands:
  run            Run an agent from a prompt, headless
  resume         Continue a session; terminal picker, otherwise latest in this workspace
  continue       Continue a session; terminal picker, otherwise latest in this workspace
  fork           Fork a session at its tip or after a completed turn
  rewind         Restore a session's files and fork it after a completed turn
  rm             Delete a stored session
  compact        Compact a session into its deterministic successor
  export         Export a session or sanitized bundle
  ls             List sessions (this workspace; --all for every one)
  usage          Report usage, context and cost for this workspace
  models         List the shared model catalogue
  hooks          Inspect the admitted hook catalogue
  doctor         Print a read-only redacted health snapshot
  checkpoint     Preview and selectively restore session-owned file edits
  setup          Review provider readiness and configure saved user settings
  acp            Serve the Agent Client Protocol on stdio or an endpoint
  a2a            Serve Agent2Agent discovery information
  auth           Manage subscription credentials
  mcp            Manage configured MCP servers
  config         Manage stored user configuration
  roots          Manage perimeter roots
  rules          Inspect and manage permission rules
  mcp-server     Serve scoped ego runs over MCP on stdio
  worktree       Inspect or safely remove a retained local worktree
  build-recipes  Inspect or delete learned build-output recipes

      --worktree <BRANCH>
          Create a retained managed worktree on this explicitly approved new branch
  -f, --file <FILE>
          Read the instruction and its invocation configuration from an agent file
      --var <KEY=VALUE>
          Supply one declared agent-file template variable. Requires --file
      --attach <IMAGE_PATH|HTTPS_IMAGE_URL>
          Add exact image bytes as ordered context before the prompt text
  -m, --model <MODEL>
          provider/model slug, e.g. ollama/qwen3
      --account <N>
          Select an account by its stable number within the provider
      --offline
          No non-loopback egress from ego's own HTTP transport
      --proxy <http://HOST[:PORT]>
          Explicit HTTP CONNECT proxy for provider HTTPS (no ambient proxy discovery)
      --proxy-ca <FILE>
          Additional PEM trust anchor for provider HTTPS through --proxy only
      --sandbox <SANDBOX>
          Filesystem confinement: ro|workspace|adaptive|off; off = no confinement, credentials included
      --thinking <THINKING>
          off|minimal|low|medium|high|xhigh|max
      --read-only-dir <DIR>
          Grant reads of one existing canonical absolute upload directory
      --system <SYSTEM>
          Replace the system prompt. `@file` reads it from a file
      --bare
          No ambient discovery, even in a trusted workspace
      --append-system <APPEND_SYSTEM>
          Append to the system prompt. `@file` reads it from a file
      --output-style <NAME>
          Select a trusted prompt-only style: concise, explanatory, or NAME from EGO_HOME/output-styles/NAME.md
      --instructions <INSTRUCTIONS>
          Use this instruction file instead of discovering one
      --tools <TOOLS>
          Restrict the builtin tools to this comma-separated list
      --no-tools
          Offer no builtin tools; run/resume also disable MCP. ACP still permits host-admitted MCP tools
      --no-hooks
          Run with every hook off for this invocation; configuration is unchanged
      --allow <RULE>
          Allow rule, e.g. 'read(src/**)'. Repeatable
      --deny <RULE>
          Deny rule, e.g. 'write(**)'. Repeatable
      --profile <PROFILE>
          Layer [profile.NAME] from config
      --trust-workspace
          Trust this launch's workspace without asking; remember the answer
      --trust-development-programs
          Trust this workspace and future edits to its current named programs; remember the grant
      --session <SESSION>
          Named session: created or attached
      --session-id <SESSION_ID>
          A session id supplied by the caller
      --session-dir <SESSION_DIR>
          Override the store location
  -C <DIR>
          Run as if started in DIR
      --output <OUTPUT>
          text|json|jsonl [possible values: text, json, jsonl]
      --artifact-dir <ARTIFACT_DIR>
          Override the sanitized run-artifact root for profile `ci`
      --schema <SCHEMA>
          JSON Schema the final answer must validate against; `@file` or inline
      --schema-retries <SCHEMA_RETRIES>
          How many times to re-prompt on a schema mismatch [default: 2]
      --retry <RETRY>
          Additional delivery-safe attempts for each candidate
      --fallback <P/M>
          Ordered provider/model candidate used after a delivery-safe failure
      --budget <BUDGET>
          Hard cap: '2.5$' or '400k' tokens
      --max-output-tokens <MAX_OUTPUT_TOKENS>
          Maximum generated tokens per request
      --goal <GOAL>
          Set a checked session objective
      --no-context-tool
          Disable model-requested context transitions
      --context-clear
          Permit the model to discard conversation history using clear
      --context-interval <CONTEXT_INTERVAL>
          Minimum completed turns between model-requested transitions [default: 10]
      --goal-turns <GOAL_TURNS>
          Optional maximum automatic goal continuation turns
      --goal-check-command <GOAL_CHECK_COMMAND>
          Evaluator argv as a JSON string array; reads evidence JSON on stdin
      --max-turns <MAX_TURNS>
          Cap the turns this run may take
      --timeout <TIMEOUT>
          Wall clock, e.g. '90s' or '10m'
      --mcp <NAME|@FILE>
          Select an effective server by name, even one marked enabled = false, or attach a strict invocation-scoped JSON MCP server
      --mcp-only
          Attach only the servers that --mcp selects; enabled configured servers stay detached
      --allow-dir <PATH[:ro|rw]>
          Allow this run to reach one existing canonical absolute directory, read-only unless `:rw`. Not stored; `ego roots add` stores a root
      --mode <MODE>
          Permission mode: plan|default|auto|accept-edits|yolo (alias: edits)
      --plan
          Shorthand for --mode plan
      --yolo
          Shorthand for --mode yolo
  -c, --set <K=V>
          Dotted-path config override, value parsed as TOML
  -q, --quiet
          Suppress progress and diagnostics on stderr; stdout is unaffected
      --no-color
          Never emit ANSI colour, whatever the terminal claims to support
      --plain
          Append plain output without terminal styling or live retry updates
      --vim
          Use Vim normal/insert editing in the interactive composer
      --memory
          Keep a memory file for this workspace that the model can add to
      --no-session-compression
          Do not compress old sessions in the background during this run
      --no-checkpoints
          Do not save file checkpoints before writes during this run
  -h, --help
          Print help

`ego <COMMAND> --help` describes one of them.
Start Ollama and run `ollama pull qwen3` before the local example above.
```

## `ego run --help`

```text
Run an agent from a prompt, headless.

Usage: ego run [OPTIONS] [PROMPT]

Arguments:
  [PROMPT]  The prompt. `-` reads it from stdin

Options:
      --worktree <BRANCH>
          Create a retained managed worktree on this explicitly approved new branch
  -f, --file <FILE>
          Read the instruction and its invocation configuration from an agent file
      --var <KEY=VALUE>
          Supply one declared agent-file template variable. Requires --file
      --attach <IMAGE_PATH|HTTPS_IMAGE_URL>
          Add exact image bytes as ordered context before the prompt text
  -m, --model <MODEL>
          provider/model slug, e.g. ollama/qwen3
      --account <N>
          Select an account by its stable number within the provider
      --offline
          No non-loopback egress from ego's own HTTP transport
      --proxy <http://HOST[:PORT]>
          Explicit HTTP CONNECT proxy for provider HTTPS (no ambient proxy discovery)
      --proxy-ca <FILE>
          Additional PEM trust anchor for provider HTTPS through --proxy only
      --sandbox <SANDBOX>
          Filesystem confinement: ro|workspace|adaptive|off; off = no confinement, credentials included
      --thinking <THINKING>
          off|minimal|low|medium|high|xhigh|max
      --read-only-dir <DIR>
          Grant reads of one existing canonical absolute upload directory
      --system <SYSTEM>
          Replace the system prompt. `@file` reads it from a file
      --bare
          No ambient discovery, even in a trusted workspace
      --append-system <APPEND_SYSTEM>
          Append to the system prompt. `@file` reads it from a file
      --output-style <NAME>
          Select a trusted prompt-only style: concise, explanatory, or NAME from EGO_HOME/output-styles/NAME.md
      --instructions <INSTRUCTIONS>
          Use this instruction file instead of discovering one
      --tools <TOOLS>
          Restrict the builtin tools to this comma-separated list
      --no-tools
          Offer no builtin tools; run/resume also disable MCP. ACP still permits host-admitted MCP tools
      --no-hooks
          Run with every hook off for this invocation; configuration is unchanged
      --allow <RULE>
          Allow rule, e.g. 'read(src/**)'. Repeatable
      --deny <RULE>
          Deny rule, e.g. 'write(**)'. Repeatable
      --profile <PROFILE>
          Layer [profile.NAME] from config
      --trust-workspace
          Trust this launch's workspace without asking; remember the answer
      --trust-development-programs
          Trust this workspace and future edits to its current named programs; remember the grant
      --session <SESSION>
          Named session: created or attached
      --session-id <SESSION_ID>
          A session id supplied by the caller
      --session-dir <SESSION_DIR>
          Override the store location
  -C <DIR>
          Run as if started in DIR
      --output <OUTPUT>
          text|json|jsonl [possible values: text, json, jsonl]
      --artifact-dir <ARTIFACT_DIR>
          Override the sanitized run-artifact root for profile `ci`
      --schema <SCHEMA>
          JSON Schema the final answer must validate against; `@file` or inline
      --schema-retries <SCHEMA_RETRIES>
          How many times to re-prompt on a schema mismatch [default: 2]
      --retry <RETRY>
          Additional delivery-safe attempts for each candidate
      --fallback <P/M>
          Ordered provider/model candidate used after a delivery-safe failure
      --budget <BUDGET>
          Hard cap: '2.5$' or '400k' tokens
      --max-output-tokens <MAX_OUTPUT_TOKENS>
          Maximum generated tokens per request
      --goal <GOAL>
          Set a checked session objective
      --no-context-tool
          Disable model-requested context transitions
      --context-clear
          Permit the model to discard conversation history using clear
      --context-interval <CONTEXT_INTERVAL>
          Minimum completed turns between model-requested transitions [default: 10]
      --goal-turns <GOAL_TURNS>
          Optional maximum automatic goal continuation turns
      --goal-check-command <GOAL_CHECK_COMMAND>
          Evaluator argv as a JSON string array; reads evidence JSON on stdin
      --max-turns <MAX_TURNS>
          Cap the turns this run may take
      --timeout <TIMEOUT>
          Wall clock, e.g. '90s' or '10m'
      --mcp <NAME|@FILE>
          Select an effective server by name, even one marked enabled = false, or attach a strict invocation-scoped JSON MCP server
      --mcp-only
          Attach only the servers that --mcp selects; enabled configured servers stay detached
      --allow-dir <PATH[:ro|rw]>
          Allow this run to reach one existing canonical absolute directory, read-only unless `:rw`. Not stored; `ego roots add` stores a root
      --mode <MODE>
          Permission mode: plan|default|auto|accept-edits|yolo (alias: edits)
      --plan
          Shorthand for --mode plan
      --yolo
          Shorthand for --mode yolo
  -c, --set <K=V>
          Dotted-path config override, value parsed as TOML
  -q, --quiet
          Suppress progress and diagnostics on stderr; stdout is unaffected
      --no-color
          Never emit ANSI colour, whatever the terminal claims to support
      --plain
          Append plain output without terminal styling or live retry updates
      --memory
          Keep a memory file for this workspace that the model can add to
      --no-session-compression
          Do not compress old sessions in the background during this run
      --no-checkpoints
          Do not save file checkpoints before writes during this run
  -h, --help
          Print help
```

## `ego resume --help`

```text
Continue a stored session.

Usage: ego resume [OPTIONS] [ID] [PROMPT]

Arguments:
  [ID]      Session UUID; omit it for the terminal picker or headless latest in this workspace
  [PROMPT]  The prompt. `-` reads it from stdin

Options:
      --answer <QUESTION_ID=JSON>
          Settle a pending question. Repeat for multiple sibling questions
      --accept-authority-delta
          Accept the authority change shown for this resume, without a question. Applies to this resume only
      --title <TITLE>
          Resume the session of this workspace whose title is exactly TITLE
      --queue
          Wait while another process owns the session, then run the prompt headless. Waiting counts toward --timeout
      --name <NAME>
          Read or change the session title without starting a turn: show, set=TEXT, clear
      --goal-operation <GOAL_OPERATION>
          Read or change goal metadata without starting a turn: show, stop, set=TEXT, achieve, resume, block=REASON
  -f, --file <FILE>
          Read the instruction and its invocation configuration from an agent file
      --var <KEY=VALUE>
          Supply one declared agent-file template variable. Requires --file
      --attach <IMAGE_PATH|HTTPS_IMAGE_URL>
          Add exact image bytes as ordered context before the prompt text
  -m, --model <MODEL>
          provider/model slug, e.g. ollama/qwen3
      --account <N>
          Select an account by its stable number within the provider
      --offline
          No non-loopback egress from ego's own HTTP transport
      --proxy <http://HOST[:PORT]>
          Explicit HTTP CONNECT proxy for provider HTTPS (no ambient proxy discovery)
      --proxy-ca <FILE>
          Additional PEM trust anchor for provider HTTPS through --proxy only
      --sandbox <SANDBOX>
          Filesystem confinement: ro|workspace|adaptive|off; off = no confinement, credentials included
      --thinking <THINKING>
          off|minimal|low|medium|high|xhigh|max
      --read-only-dir <DIR>
          Grant reads of one existing canonical absolute upload directory
      --system <SYSTEM>
          Replace the system prompt. `@file` reads it from a file
      --bare
          No ambient discovery, even in a trusted workspace
      --append-system <APPEND_SYSTEM>
          Append to the system prompt. `@file` reads it from a file
      --output-style <NAME>
          Select a trusted prompt-only style: concise, explanatory, or NAME from EGO_HOME/output-styles/NAME.md
      --instructions <INSTRUCTIONS>
          Use this instruction file instead of discovering one
      --tools <TOOLS>
          Restrict the builtin tools to this comma-separated list
      --no-tools
          Offer no builtin tools; run/resume also disable MCP. ACP still permits host-admitted MCP tools
      --no-hooks
          Run with every hook off for this invocation; configuration is unchanged
      --allow <RULE>
          Allow rule, e.g. 'read(src/**)'. Repeatable
      --deny <RULE>
          Deny rule, e.g. 'write(**)'. Repeatable
      --profile <PROFILE>
          Layer [profile.NAME] from config
      --trust-workspace
          Trust this launch's workspace without asking; remember the answer
      --trust-development-programs
          Trust this workspace and future edits to its current named programs; remember the grant
      --session-dir <SESSION_DIR>
          Override the store location
  -C <DIR>
          Run as if started in DIR
      --output <OUTPUT>
          text|json|jsonl [possible values: text, json, jsonl]
      --artifact-dir <ARTIFACT_DIR>
          Override the sanitized run-artifact root for profile `ci`
      --schema <SCHEMA>
          JSON Schema the final answer must validate against; `@file` or inline
      --schema-retries <SCHEMA_RETRIES>
          How many times to re-prompt on a schema mismatch [default: 2]
      --retry <RETRY>
          Additional delivery-safe attempts for each candidate
      --fallback <P/M>
          Ordered provider/model candidate used after a delivery-safe failure
      --budget <BUDGET>
          Hard cap: '2.5$' or '400k' tokens
      --max-output-tokens <MAX_OUTPUT_TOKENS>
          Maximum generated tokens per request
      --goal <GOAL>
          Set a checked session objective
      --no-context-tool
          Disable model-requested context transitions
      --context-clear
          Permit the model to discard conversation history using clear
      --context-interval <CONTEXT_INTERVAL>
          Minimum completed turns between model-requested transitions [default: 10]
      --goal-turns <GOAL_TURNS>
          Optional maximum automatic goal continuation turns
      --goal-check-command <GOAL_CHECK_COMMAND>
          Evaluator argv as a JSON string array; reads evidence JSON on stdin
      --max-turns <MAX_TURNS>
          Cap the turns this run may take
      --timeout <TIMEOUT>
          Wall clock, e.g. '90s' or '10m'
      --mcp <NAME|@FILE>
          Select an effective server by name, even one marked enabled = false, or attach a strict invocation-scoped JSON MCP server
      --mcp-only
          Attach only the servers that --mcp selects; enabled configured servers stay detached
      --allow-dir <PATH[:ro|rw]>
          Allow this run to reach one existing canonical absolute directory, read-only unless `:rw`. Not stored; `ego roots add` stores a root
      --mode <MODE>
          Permission mode: plan|default|auto|accept-edits|yolo (alias: edits)
      --plan
          Shorthand for --mode plan
      --yolo
          Shorthand for --mode yolo
  -c, --set <K=V>
          Dotted-path config override, value parsed as TOML
  -q, --quiet
          Suppress progress and diagnostics on stderr; stdout is unaffected
      --no-color
          Never emit ANSI colour, whatever the terminal claims to support
      --plain
          Append plain output without terminal styling or live retry updates
      --vim
          Use Vim normal/insert editing in the interactive composer
      --memory
          Keep a memory file for this workspace that the model can add to
      --no-session-compression
          Do not compress old sessions in the background during this run
      --no-checkpoints
          Do not save file checkpoints before writes during this run
  -h, --help
          Print help

With terminal input and output, no ID opens the saved-session picker across roots. Otherwise no ID selects the latest session in this workspace. Explicit --output, --answer, --file, stdin prompt '-' or profile ci select headless resumption.

A single non-UUID positional is a prompt-only form, not a session ID. For example: ego resume "next step" (or ego continue "next step"). Supply a UUID plus a prompt to name a specific session.
```

## `ego continue --help`

```text
Continue a stored session.

Usage: ego continue [OPTIONS] [ID] [PROMPT]

Arguments:
  [ID]      Session UUID; omit it for the terminal picker or headless latest in this workspace
  [PROMPT]  The prompt. `-` reads it from stdin

Options:
      --answer <QUESTION_ID=JSON>
          Settle a pending question. Repeat for multiple sibling questions
      --accept-authority-delta
          Accept the authority change shown for this resume, without a question. Applies to this resume only
      --title <TITLE>
          Resume the session of this workspace whose title is exactly TITLE
      --queue
          Wait while another process owns the session, then run the prompt headless. Waiting counts toward --timeout
      --name <NAME>
          Read or change the session title without starting a turn: show, set=TEXT, clear
      --goal-operation <GOAL_OPERATION>
          Read or change goal metadata without starting a turn: show, stop, set=TEXT, achieve, resume, block=REASON
  -f, --file <FILE>
          Read the instruction and its invocation configuration from an agent file
      --var <KEY=VALUE>
          Supply one declared agent-file template variable. Requires --file
      --attach <IMAGE_PATH|HTTPS_IMAGE_URL>
          Add exact image bytes as ordered context before the prompt text
  -m, --model <MODEL>
          provider/model slug, e.g. ollama/qwen3
      --account <N>
          Select an account by its stable number within the provider
      --offline
          No non-loopback egress from ego's own HTTP transport
      --proxy <http://HOST[:PORT]>
          Explicit HTTP CONNECT proxy for provider HTTPS (no ambient proxy discovery)
      --proxy-ca <FILE>
          Additional PEM trust anchor for provider HTTPS through --proxy only
      --sandbox <SANDBOX>
          Filesystem confinement: ro|workspace|adaptive|off; off = no confinement, credentials included
      --thinking <THINKING>
          off|minimal|low|medium|high|xhigh|max
      --read-only-dir <DIR>
          Grant reads of one existing canonical absolute upload directory
      --system <SYSTEM>
          Replace the system prompt. `@file` reads it from a file
      --bare
          No ambient discovery, even in a trusted workspace
      --append-system <APPEND_SYSTEM>
          Append to the system prompt. `@file` reads it from a file
      --output-style <NAME>
          Select a trusted prompt-only style: concise, explanatory, or NAME from EGO_HOME/output-styles/NAME.md
      --instructions <INSTRUCTIONS>
          Use this instruction file instead of discovering one
      --tools <TOOLS>
          Restrict the builtin tools to this comma-separated list
      --no-tools
          Offer no builtin tools; run/resume also disable MCP. ACP still permits host-admitted MCP tools
      --no-hooks
          Run with every hook off for this invocation; configuration is unchanged
      --allow <RULE>
          Allow rule, e.g. 'read(src/**)'. Repeatable
      --deny <RULE>
          Deny rule, e.g. 'write(**)'. Repeatable
      --profile <PROFILE>
          Layer [profile.NAME] from config
      --trust-workspace
          Trust this launch's workspace without asking; remember the answer
      --trust-development-programs
          Trust this workspace and future edits to its current named programs; remember the grant
      --session-dir <SESSION_DIR>
          Override the store location
  -C <DIR>
          Run as if started in DIR
      --output <OUTPUT>
          text|json|jsonl [possible values: text, json, jsonl]
      --artifact-dir <ARTIFACT_DIR>
          Override the sanitized run-artifact root for profile `ci`
      --schema <SCHEMA>
          JSON Schema the final answer must validate against; `@file` or inline
      --schema-retries <SCHEMA_RETRIES>
          How many times to re-prompt on a schema mismatch [default: 2]
      --retry <RETRY>
          Additional delivery-safe attempts for each candidate
      --fallback <P/M>
          Ordered provider/model candidate used after a delivery-safe failure
      --budget <BUDGET>
          Hard cap: '2.5$' or '400k' tokens
      --max-output-tokens <MAX_OUTPUT_TOKENS>
          Maximum generated tokens per request
      --goal <GOAL>
          Set a checked session objective
      --no-context-tool
          Disable model-requested context transitions
      --context-clear
          Permit the model to discard conversation history using clear
      --context-interval <CONTEXT_INTERVAL>
          Minimum completed turns between model-requested transitions [default: 10]
      --goal-turns <GOAL_TURNS>
          Optional maximum automatic goal continuation turns
      --goal-check-command <GOAL_CHECK_COMMAND>
          Evaluator argv as a JSON string array; reads evidence JSON on stdin
      --max-turns <MAX_TURNS>
          Cap the turns this run may take
      --timeout <TIMEOUT>
          Wall clock, e.g. '90s' or '10m'
      --mcp <NAME|@FILE>
          Select an effective server by name, even one marked enabled = false, or attach a strict invocation-scoped JSON MCP server
      --mcp-only
          Attach only the servers that --mcp selects; enabled configured servers stay detached
      --allow-dir <PATH[:ro|rw]>
          Allow this run to reach one existing canonical absolute directory, read-only unless `:rw`. Not stored; `ego roots add` stores a root
      --mode <MODE>
          Permission mode: plan|default|auto|accept-edits|yolo (alias: edits)
      --plan
          Shorthand for --mode plan
      --yolo
          Shorthand for --mode yolo
  -c, --set <K=V>
          Dotted-path config override, value parsed as TOML
  -q, --quiet
          Suppress progress and diagnostics on stderr; stdout is unaffected
      --no-color
          Never emit ANSI colour, whatever the terminal claims to support
      --plain
          Append plain output without terminal styling or live retry updates
      --vim
          Use Vim normal/insert editing in the interactive composer
      --memory
          Keep a memory file for this workspace that the model can add to
      --no-session-compression
          Do not compress old sessions in the background during this run
      --no-checkpoints
          Do not save file checkpoints before writes during this run
  -h, --help
          Print help

With terminal input and output, no ID opens the saved-session picker across roots. Otherwise no ID selects the latest session in this workspace. Explicit --output, --answer, --file, stdin prompt '-' or profile ci select headless resumption.

A single non-UUID positional is a prompt-only form, not a session ID. For example: ego resume "next step" (or ego continue "next step"). Supply a UUID plus a prompt to name a specific session.
```

## `ego fork --help`

```text
Fork a session at its tip or after the completed turn containing a message.

Usage: ego fork [OPTIONS] <SOURCE>

Arguments:
  <SOURCE>  Stored session ID to fork

Options:
      --at <ITEM_ID>               Fork after the completed turn containing this message
      --session-dir <SESSION_DIR>  Override the store location
  -C <DIR>                         Run as if started in DIR
      --output <OUTPUT>            text|json|jsonl [possible values: text, json, jsonl]
  -h, --help                       Print help
```

## `ego rewind --help`

```text
Restore the files a session changed after a completed turn, then fork the conversation there.

Usage: ego rewind [OPTIONS] --at <ITEM_ID> <SOURCE>

Arguments:
  <SOURCE>  Stored session ID to rewind

Options:
      --at <ITEM_ID>               A user or assistant message of the turn to return to
      --session-dir <SESSION_DIR>  Override the store location
  -C <DIR>                         Run as if started in DIR
      --output <OUTPUT>            text|json|jsonl [possible values: text, json, jsonl]
  -h, --help                       Print help
```

## `ego rm --help`

```text
Delete a stored session (history is retained for descendants).

Usage: ego rm [OPTIONS] [SOURCE]

Arguments:
  [SOURCE]  Stored session ID to delete; history needed by descendants is retained

Options:
      --incompatible               Permanently remove sessions with an unsupported stored format. Requires --yes
      --expired                    Permanently remove sessions not opened within `sessions.retention_days` (30 by default). Open and held sessions stay. Requires --yes
      --yes                        Supply deletion confirmation without prompting; required without terminal stdin
      --force                      Acknowledge an uncertain prior run; does not replace deletion confirmation and is not a general override
      --session-dir <SESSION_DIR>  Override the store location
  -C <DIR>                         Run as if started in DIR
  -h, --help                       Print help
```

## `ego compact --help`

```text
Compact a session into its deterministic successor.

Usage: ego compact [OPTIONS] <SOURCE>

Arguments:
  <SOURCE>  Stored session ID to compact

Options:
      --budget <BUDGET>                        Hard spend ceiling, as for run
      --max-output-tokens <MAX_OUTPUT_TOKENS>  Maximum generated tokens per request
      --request-id <REQUEST_ID>                Reuse this request UUID for a deterministic retry
      --session-dir <SESSION_DIR>              Override the store location
  -C <DIR>                                     Run as if started in DIR
      --output <OUTPUT>                        text|json|jsonl [possible values: text, json, jsonl]
  -h, --help                                   Print help
```

## `ego export --help`

```text
Export a session locally or as a structurally sanitized bundle.

Usage: ego export [OPTIONS] <SOURCE>

Arguments:
  <SOURCE>  Stored session ID to export

Options:
      --sanitize                   Remove private content structurally; exports are unsanitized by default
      --format <FORMAT>            Transcript format; JSONL is stdout-only and cannot be combined with --file [default: md] [possible values: md, json, jsonl, transcript]
      --follow                     Follow committed local transcript events until stopped
      --after <SEQ>                Only emit transcript events after this session-local sequence
      --file <PATH>                Write to PATH instead of stdout; its parent must exist. Refuse symlinks, non-regular targets and existing files with different bytes; identical bytes are accepted
      --session-dir <SESSION_DIR>  Override the store location
  -C <DIR>                         Run as if started in DIR
  -h, --help                       Print help
```

## `ego ls --help`

```text
List sessions (this workspace; --all for every one).

Usage: ego ls [OPTIONS]

Options:
      --all                        Every session in the store, not just this workspace's
      --session-dir <SESSION_DIR>  Override the store location
  -C <DIR>                         Run as if started in DIR
      --output <OUTPUT>            text|json|jsonl [possible values: text, json, jsonl]
  -h, --help                       Print help
```

## `ego usage --help`

```text
Report usage, context and cost for this workspace.

Usage: ego usage [OPTIONS]

Options:
      --period <PERIOD>            all|Ns|Nm|Nh [default: all]
      --session <ID>               Report only this stored session
      --json                       Emit the exact versioned usage-report object
      --all-roles                  Keep role rows that recorded no activity in the text table
      --session-dir <SESSION_DIR>  Override the store location
  -C <DIR>                         Run as if started in DIR
  -h, --help                       Print help
```

## `ego models --help`

```text
List the shared model catalogue.

Usage: ego models [OPTIONS]

Options:
      --account <N>          Select an account by its stable number within the provider
      --provider <PROVIDER>  Restrict enumeration and output to one admitted provider
      --refresh              Refresh the selected discoverable provider slices
      --json                 Emit the exact versioned snapshot object
  -h, --help                 Print help
```

## `ego hooks --help`

```text
Inspect the admitted system and user hook catalogue.

Usage: ego hooks <COMMAND>

Commands:
  ls  List the admitted hook catalogue without changing configuration

Options:
  -h, --help  Print help
```

## `ego hooks ls --help`

```text
List the admitted hook catalogue without changing configuration

Usage: ego hooks ls [OPTIONS]

Options:
      --json  Emit the exact versioned hook-listing object
  -h, --help  Print help
```

## `ego doctor --help`

```text
Report a read-only redacted health snapshot of this installation.

Usage: ego doctor [OPTIONS]

Options:
      --json           Emit the exact versioned health snapshot
      --offline        Skip every provider network probe
      --probe-network  Probe the network reachability of each configured provider, like `doctor_network = true`
  -h, --help           Print help
```

## `ego checkpoint --help`

```text
Preview or selectively restore session-owned Git file checkpoints.

Usage: ego checkpoint [OPTIONS] <SESSION> <COMMAND>

Commands:
  preview  List checkpoint IDs, paths, effects, and external-change conflicts as JSON
  restore  Restore one selected checkpoint; external changes are never forced
  close    Close checkpoint ownership and delete the session's private Git ref
  help     Print this message or the help of the given subcommand(s)

Arguments:
  <SESSION>  Session that owns the checkpoints

Options:
  -C, --directory <DIRECTORY>  Workspace that owns the session [default: .]
  -h, --help                   Print help
```

## `ego checkpoint preview --help`

```text
List checkpoint IDs, paths, effects, and external-change conflicts as JSON

Usage: ego checkpoint <SESSION> preview

Options:
  -h, --help  Print help
```

## `ego checkpoint restore --help`

```text
Restore one selected checkpoint; external changes are never forced

Usage: ego checkpoint <SESSION> restore <CHECKPOINT>

Arguments:
  <CHECKPOINT>  Checkpoint ID to restore

Options:
  -h, --help  Print help
```

## `ego checkpoint close --help`

```text
Close checkpoint ownership and delete the session's private Git ref

Usage: ego checkpoint <SESSION> close

Options:
  -h, --help  Print help
```

## `ego setup --help`

```text
Review provider readiness and configure saved user settings in an interactive terminal.

Usage: ego setup [OPTIONS]

Options:
      --profile <PROFILE>      Edit one user profile rather than the base user settings
  -C, --directory <DIRECTORY>  Resolve effective settings for this workspace
  -h, --help                   Print help
```

## `ego acp --help`

```text
Serve the Agent Client Protocol on stdio or an endpoint.

Usage: ego acp [OPTIONS]

Options:
      --session-dir <SESSION_DIR>
          Override the store location
  -C <DIR>
          Run as if started in DIR
      --sandbox <SANDBOX>
          The weakest confinement a session may select: ro|workspace|adaptive|off; off = no confinement, credentials included
      --read-only-dir <DIR>
          Grant reads of one existing canonical absolute upload directory
      --system <SYSTEM>
          Replace the system prompt. `@file` reads it from a file
      --bare
          No ambient discovery, even in a trusted workspace
      --append-system <APPEND_SYSTEM>
          Append to the system prompt. `@file` reads it from a file
      --output-style <NAME>
          Select a trusted prompt-only style: concise, explanatory, or NAME from EGO_HOME/output-styles/NAME.md
      --instructions <INSTRUCTIONS>
          Use this instruction file instead of discovering one
      --tools <TOOLS>
          Restrict the builtin tools to this comma-separated list
      --no-tools
          Offer no builtin tools; run/resume also disable MCP. ACP still permits host-admitted MCP tools
      --no-hooks
          Run with every hook off for this invocation; configuration is unchanged
      --allow <RULE>
          Allow rule, e.g. 'read(src/**)'. Repeatable
      --deny <RULE>
          Deny rule, e.g. 'write(**)'. Repeatable
      --profile <PROFILE>
          Layer [profile.NAME] from config
      --exclusive-read-only
          Serve an unattended read-only session that no configuration layer or host request can widen: no tools, hooks, skills, commands or MCP servers, and an environment cleared to an allow-list
      --env-allow <NAME>
          Keep this environment variable under --exclusive-read-only. Repeatable
      --listen <LISTEN>
          Serve on a socket path or address instead of stdio
      --allow-public-bind
          Serve on an address reachable from outside this machine
      --max-connections <MAX_CONNECTIONS>
          Maximum simultaneous listener connections (runtime default: 32)
      --connection-lifetime <CONNECTION_LIFETIME>
          Maximum lifetime of one listener connection: 90s, 10m or 2h (runtime default: 24h)
      --max-ingress-bytes <MAX_INGRESS_BYTES>
          Maximum cumulative bytes read from one listener connection (runtime default: 134217728 bytes)
  -h, --help
          Print help
```

## `ego a2a --help`

```text
`ego a2a` — the Agent2Agent protocol, on an endpoint.

`--listen` is required and has no default. Every other surface ego serves speaks over stdio to a parent that already had the right to run it; this one opens an endpoint, and an endpoint that appears because a default said so is one nobody decided to open.

Usage: ego a2a [OPTIONS] --listen <LISTEN>

Options:
  -C <DIR>
          Run as if started in DIR. `ego a2a` keeps no sessions, so it takes no `--session-dir`

      --sandbox <SANDBOX>
          The weakest confinement a session may select: ro|workspace|adaptive|off; off = no confinement, credentials included

      --listen <LISTEN>
          Where to serve: a socket path, or an address such as `127.0.0.1:8080`.

          Anything that parses as an address is one; anything else is a local endpoint, which the operating system authenticates and which therefore needs no credential. A path is preferred for that reason.

      --allow-public-bind
          Serve on an address reachable from outside this machine

  -h, --help
          Print help (see a summary with '-h')
```

## `ego auth --help`

```text
Manage provider credentials.

Usage: ego auth [OPTIONS] <COMMAND>

Commands:
  migrate  Explicitly move one legacy credential to an existing named connection
  usage    Read subscription quota without generating a model response
  login    Authenticate and store a provider credential
  logout   Remove a stored provider credential
  remove   Delete a saved account and its local credential without reusing its number
  status   Show credential type and expiry, never token values
  help     Print this message or the help of the given subcommand(s)

Options:
      --account <N>  Select an account by its stable number within the provider
  -h, --help         Print help
```

## `ego auth migrate --help`

```text
Explicitly move one legacy credential to an existing named connection

Usage: ego auth migrate [OPTIONS] <PROVIDER>

Arguments:
  <PROVIDER>  Legacy provider route whose stored credential is moved

Options:
      --account <N>  Select an account by its stable number within the provider
  -h, --help         Print help
```

## `ego auth usage --help`

```text
Read subscription quota without generating a model response

Usage: ego auth usage [OPTIONS] [PROVIDER]

Arguments:
  [PROVIDER]  Limit the read to one stored provider

Options:
      --json         Print quota and provider issues as JSON
      --account <N>  Select an account by its stable number within the provider
  -h, --help         Print help
```

## `ego auth login --help`

```text
Authenticate and store a provider credential

Usage: ego auth login [OPTIONS] [PROVIDER]

Arguments:
  [PROVIDER]  Provider to authenticate

Options:
      --renew            Renew a saved account instead of creating a new account
      --account <N>      Select an account by its stable number within the provider
      --device           Prefer device-code OAuth login when the provider offers it
      --method <METHOD>  Choose API-key registration or OAuth for a provider offering both [possible values: api-key, oauth]
  -h, --help             Print help
```

## `ego auth logout --help`

```text
Remove a stored provider credential

Usage: ego auth logout [OPTIONS] [PROVIDER]

Arguments:
  [PROVIDER]  Provider whose credential to remove

Options:
      --account <N>  Select an account by its stable number within the provider
  -h, --help         Print help
```

## `ego auth remove --help`

```text
Delete a saved account and its local credential without reusing its number

Usage: ego auth remove [OPTIONS] <PROVIDER>

Arguments:
  <PROVIDER>  Provider whose saved account to delete

Options:
      --account <N>  Select an account by its stable number within the provider
  -h, --help         Print help
```

## `ego auth status --help`

```text
Show credential type and expiry, never token values

Usage: ego auth status [OPTIONS] [PROVIDER]

Arguments:
  [PROVIDER]  Provider whose credential status to show

Options:
      --json         Print versioned local credential presence without secret metadata
      --account <N>  Select an account by its stable number within the provider
  -h, --help         Print help
```

## `ego mcp --help`

```text
Manage configured MCP servers.

Usage: ego mcp <COMMAND>

Commands:
  add     Add or replace one complete server entry
  rm      Remove, mask, or reveal one configured server entry
  ls      List configured and admitted MCP state
  login   Authorize a public OAuth client with an explicit registration strategy
  reload  Reconcile every configured server, or one named server

Options:
  -h, --help  Print help
```

## `ego mcp add --help`

```text
Add or replace one complete server entry

Usage: ego mcp add <NAME> --stdio [--cwd <DIR>] [--inherit-env <NAME>]... -- <COMMAND> [ARG]...
       ego mcp add <NAME> --http <URL> [--profile <current|legacy-sse>] [--auth <none|oauth>] [--allow-insecure-loopback] [--oauth-host <URL>]...

Arguments:
  <NAME>  Name of the server to configure

Options:
      --stdio                    Configure a subprocess transport. Its command follows literal `--`
      --http <URL>               Configure a Streamable HTTP transport
      --cwd <DIR>                Working directory for the stdio child
      --inherit-env <NAME>       Environment variable inherited by the stdio child. Repeatable
      --profile <PROFILE>        Explicit HTTP wire profile; never selected by fallback [possible values: current, legacy-sse]
      --auth <AUTH>              HTTP authentication mode [possible values: none, oauth]
      --allow-insecure-loopback  Consent to plaintext HTTP for this server on loopback or the owner network
      --oauth-host <URL>         Extra OAuth origin that may host this server's authorization, token or registration endpoint when it differs from the issuer's. Repeatable
  -h, --help                     Print help
```

## `ego mcp rm --help`

```text
Remove, mask, or reveal one configured server entry

Usage: ego mcp rm <NAME>

Arguments:
  <NAME>  Name of the configured server

Options:
  -h, --help  Print help
```

## `ego mcp ls --help`

```text
List configured and admitted MCP state

Usage: ego mcp ls [OPTIONS]

Options:
      --json              Emit the typed machine-readable projection
      --mcp <NAME|@FILE>  Also list this invocation's `--mcp` selection (NAME or @FILE); it is not probed
  -h, --help              Print help
```

## `ego mcp login --help`

```text
Authorize a public OAuth client with an explicit registration strategy

Usage: ego mcp login [OPTIONS] <NAME>

Arguments:
  <NAME>  Name of the configured server

Options:
      --client-id <CLIENT_ID>          Operator-issued public client ID
      --cimd <CIMD>                    Published HTTPS client metadata document URL
      --dcr                            Explicitly use issuer-advertised dynamic registration
      --callback-port <CALLBACK_PORT>  Loopback port registered with the authorization server (0 selects a free port) [default: 0]
      --refresh                        Refresh an existing issuer/resource/account-bound credential without interaction
      --issuer <ISSUER>                Issuer bound to the credential to refresh
      --account <ACCOUNT>              Account bound to the credential to refresh
  -h, --help                           Print help
```

## `ego mcp reload --help`

```text
Reconcile every configured server, or one named server

Usage: ego mcp reload [NAME]

Arguments:
  [NAME]  Server to reconcile; omit to reconcile all servers

Options:
  -h, --help  Print help
```

## `ego config --help`

```text
Manage stored user configuration.

Usage: ego config <COMMAND>

Commands:
  capabilities  Inspect managed owner transaction capabilities as JSON
  snapshot      Read stored values and their opaque owner revision as JSON
  batch         Atomically publish a bounded set of typed managed edits
  get           Read one exact stored value
  set           Store one strictly typed TOML value
  unset         Remove one exact stored value
  ls            List the exact stored managed projection
  edit          Edit the complete user document in the selected external editor

Options:
  -h, --help  Print help
```

## `ego config capabilities --help`

```text
Inspect managed owner transaction capabilities as JSON

Usage: ego config capabilities

Options:
  -h, --help  Print help
```

## `ego config snapshot --help`

```text
Read stored values and their opaque owner revision as JSON

Usage: ego config snapshot [OPTIONS]

Options:
      --profile <NAME>  Select the named user profile
  -h, --help            Print help
```

## `ego config batch --help`

```text
Atomically publish a bounded set of typed managed edits

Usage: ego config batch [OPTIONS]

Options:
      --profile <NAME>          Select the named user profile
      --set <KEY=TOML_VALUE>    Set a key to a TOML value
      --if-revision <REVISION>  Apply only if the owner revision matches
      --unset <KEY>             Remove a stored key
  -h, --help                    Print help
```

## `ego config get --help`

```text
Read one exact stored value

Usage: ego config get [OPTIONS] <KEY>

Arguments:
  <KEY>  Managed configuration key

Options:
      --profile <NAME>  Select the named user profile
      --json            Emit JSON output
  -h, --help            Print help
```

## `ego config set --help`

```text
Store one strictly typed TOML value

Usage: ego config set [OPTIONS] <KEY=TOML_VALUE>

Arguments:
  <KEY=TOML_VALUE>  Managed key and its TOML value

Options:
      --profile <NAME>  Select the named user profile
  -h, --help            Print help
```

## `ego config unset --help`

```text
Remove one exact stored value

Usage: ego config unset [OPTIONS] <KEY>

Arguments:
  <KEY>  Managed configuration key

Options:
      --profile <NAME>  Select the named user profile
  -h, --help            Print help
```

## `ego config ls --help`

```text
List the exact stored managed projection

Usage: ego config ls [OPTIONS]

Options:
      --profile <NAME>  Select the named user profile
      --json            Emit JSON output
      --effective       Show the effective run perimeter instead of stored values
  -h, --help            Print help
```

## `ego config edit --help`

```text
Edit the complete user document in the selected external editor

Usage: ego config edit

Options:
  -h, --help  Print help
```

## `ego roots --help`

```text
Usage: ego roots <COMMAND>

Commands:
  add   Add or update one user perimeter root
  rm    Remove a root by its stored spelling
  ls    List roots stored in the selected user target
  help  Print this message or the help of the given subcommand(s)

Options:
  -h, --help  Print help
```

## `ego roots add --help`

```text
Add or update one user perimeter root

Usage: ego roots add [OPTIONS] <PATH>

Arguments:
  <PATH>  Path of the perimeter root

Options:
      --profile <PROFILE>  Select the named user profile
      --access <ACCESS>    Access granted to this root [default: read] [possible values: read, read-write]
  -h, --help               Print help
```

## `ego roots rm --help`

```text
Remove a root by its stored spelling

Usage: ego roots rm [OPTIONS] <PATH>

Arguments:
  <PATH>  Path of the perimeter root

Options:
      --profile <PROFILE>  Select the named user profile
  -h, --help               Print help
```

## `ego roots ls --help`

```text
List roots stored in the selected user target

Usage: ego roots ls [OPTIONS]

Options:
      --profile <PROFILE>  Select the named user profile
      --json               Emit JSON output
  -h, --help               Print help
```

## `ego rules --help`

```text
Inspect and manage persistent permission rules.

Usage: ego rules <COMMAND>

Commands:
  ls        List the effective system and selected user rules
  allow     Persist one allow predicate
  deny      Persist one deny predicate
  revoke    Remove one predicate ID from both selected user lists
  remember  Derive persistent rules from one prepared builtin call
  explain   Explain one prepared builtin call without running it

Options:
  -h, --help  Print help
```

## `ego rules ls --help`

```text
List the effective system and selected user rules

Usage: ego rules ls [OPTIONS]

Options:
      --profile <NAME>  Select the named user profile
      --json            Emit JSON output
  -h, --help            Print help
```

## `ego rules allow --help`

```text
Persist one allow predicate

Usage: ego rules allow [OPTIONS] <RULE>

Arguments:
  <RULE>  Permission rule to store

Options:
      --profile <NAME>  Select the named user profile
  -h, --help            Print help
```

## `ego rules deny --help`

```text
Persist one deny predicate

Usage: ego rules deny [OPTIONS] <RULE>

Arguments:
  <RULE>  Permission rule to store

Options:
      --profile <NAME>  Select the named user profile
  -h, --help            Print help
```

## `ego rules revoke --help`

```text
Remove one predicate ID from both selected user lists

Usage: ego rules revoke [OPTIONS] <ID>

Arguments:
  <ID>  Identifier of the rule to revoke

Options:
      --profile <NAME>  Select the named user profile
  -h, --help            Print help
```

## `ego rules remember --help`

```text
Derive persistent rules from one prepared builtin call

Usage: ego rules remember [OPTIONS] <EFFECT> <TOOL> <ARGS_JSON>

Arguments:
  <EFFECT>     Rule effect to remember
  <TOOL>       Builtin tool name
  <ARGS_JSON>  Tool arguments as a JSON object

Options:
      --profile <NAME>  Select the named user profile
  -C <DIR>              Resolve the call in this workspace
      --yes             Confirm the proposed rules without a prompt
  -h, --help            Print help
```

## `ego rules explain --help`

```text
Explain one prepared builtin call without running it

Usage: ego rules explain [OPTIONS] <TOOL> <ARGS_JSON>

Arguments:
  <TOOL>       Builtin tool name
  <ARGS_JSON>  Tool arguments as a JSON object

Options:
      --profile <NAME>  Select the named user profile
  -C <DIR>              Resolve the call in this workspace
      --json            Emit JSON output
  -h, --help            Print help
```

## `ego mcp-server --help`

```text
Serve scoped ego runs over MCP on stdio.

Usage: ego mcp-server [OPTIONS]

Options:
      --session-dir <SESSION_DIR>
          Override the store location
  -C <DIR>
          Run as if started in DIR
      --max-concurrent-runs <MAX_CONCURRENT_RUNS>
          Runs that may execute at once [default: 1]
      --retained-requests <RETAINED_REQUESTS>
          Finished requests kept for status and resource reads [default: 16]
      --max-resource-bytes <MAX_RESOURCE_BYTES>
          Largest resource body returned to a client, in bytes [default: 65536]
      --allow-root <DIR>
          Also let clients run in DIR. The directory given with -C (or the current one) is always allowed
      --allow-model <P/M>
          Also let clients select this model. The configured model is always allowed
      --allow-mode <ALLOW_MODE>
          Also let clients select this mode. The configured mode is always allowed
  -h, --help
          Print help
```

## `ego worktree --help`

```text
Inspect or clean up a retained local worktree.

Usage: ego worktree <COMMAND>

Commands:
  show     Inspect a durable local worktree receipt without starting a model
  cleanup  Remove only a clean worktree whose commits are integrated; retain its branch
  help     Print this message or the help of the given subcommand(s)

Options:
  -h, --help  Print help
```

## `ego worktree show --help`

```text
Inspect a durable local worktree receipt without starting a model

Usage: ego worktree show <RECEIPT>

Arguments:
  <RECEIPT>  ID of the retained worktree receipt

Options:
  -h, --help  Print help
```

## `ego worktree cleanup --help`

```text
Remove only a clean worktree whose commits are integrated; retain its branch

Usage: ego worktree cleanup <RECEIPT>

Arguments:
  <RECEIPT>  ID of the retained worktree receipt

Options:
  -h, --help  Print help
```

## `ego build-recipes --help`

```text
Inspect, disable, enable or delete the learned build-output recipes of this workspace.

Usage: ego build-recipes <COMMAND>

Commands:
  ls       List the recipes of this workspace
  show     Print one recipe as JSON
  disable  Stop using one recipe; it stays stored
  enable   Use one disabled recipe again
  rm       Delete one recipe
  help     Print this message or the help of the given subcommand(s)

Options:
  -h, --help  Print help
```

## `ego build-recipes ls --help`

```text
List the recipes of this workspace

Usage: ego build-recipes ls

Options:
  -h, --help  Print help
```

## `ego build-recipes show --help`

```text
Print one recipe as JSON

Usage: ego build-recipes show <ID>

Arguments:
  <ID>  Recipe ID, as `ego build-recipes ls` prints it

Options:
  -h, --help  Print help
```

## `ego build-recipes disable --help`

```text
Stop using one recipe; it stays stored

Usage: ego build-recipes disable <ID>

Arguments:
  <ID>  Recipe ID, as `ego build-recipes ls` prints it

Options:
  -h, --help  Print help
```

## `ego build-recipes enable --help`

```text
Use one disabled recipe again

Usage: ego build-recipes enable <ID>

Arguments:
  <ID>  Recipe ID, as `ego build-recipes ls` prints it

Options:
  -h, --help  Print help
```

## `ego build-recipes rm --help`

```text
Delete one recipe

Usage: ego build-recipes rm <ID>

Arguments:
  <ID>  Recipe ID, as `ego build-recipes ls` prints it

Options:
  -h, --help  Print help
```

## `ego resume --name show --help`

```text
Continue a stored session.

Usage: ego resume [OPTIONS] [ID] [PROMPT]

Arguments:
  [ID]      Session UUID; omit it for the terminal picker or headless latest in this workspace
  [PROMPT]  The prompt. `-` reads it from stdin

Options:
      --answer <QUESTION_ID=JSON>
          Settle a pending question. Repeat for multiple sibling questions
      --accept-authority-delta
          Accept the authority change shown for this resume, without a question. Applies to this resume only
      --title <TITLE>
          Resume the session of this workspace whose title is exactly TITLE
      --queue
          Wait while another process owns the session, then run the prompt headless. Waiting counts toward --timeout
      --name <NAME>
          Read or change the session title without starting a turn: show, set=TEXT, clear
      --goal-operation <GOAL_OPERATION>
          Read or change goal metadata without starting a turn: show, stop, set=TEXT, achieve, resume, block=REASON
  -f, --file <FILE>
          Read the instruction and its invocation configuration from an agent file
      --var <KEY=VALUE>
          Supply one declared agent-file template variable. Requires --file
      --attach <IMAGE_PATH|HTTPS_IMAGE_URL>
          Add exact image bytes as ordered context before the prompt text
  -m, --model <MODEL>
          provider/model slug, e.g. ollama/qwen3
      --account <N>
          Select an account by its stable number within the provider
      --offline
          No non-loopback egress from ego's own HTTP transport
      --proxy <http://HOST[:PORT]>
          Explicit HTTP CONNECT proxy for provider HTTPS (no ambient proxy discovery)
      --proxy-ca <FILE>
          Additional PEM trust anchor for provider HTTPS through --proxy only
      --sandbox <SANDBOX>
          Filesystem confinement: ro|workspace|adaptive|off; off = no confinement, credentials included
      --thinking <THINKING>
          off|minimal|low|medium|high|xhigh|max
      --read-only-dir <DIR>
          Grant reads of one existing canonical absolute upload directory
      --system <SYSTEM>
          Replace the system prompt. `@file` reads it from a file
      --bare
          No ambient discovery, even in a trusted workspace
      --append-system <APPEND_SYSTEM>
          Append to the system prompt. `@file` reads it from a file
      --output-style <NAME>
          Select a trusted prompt-only style: concise, explanatory, or NAME from EGO_HOME/output-styles/NAME.md
      --instructions <INSTRUCTIONS>
          Use this instruction file instead of discovering one
      --tools <TOOLS>
          Restrict the builtin tools to this comma-separated list
      --no-tools
          Offer no builtin tools; run/resume also disable MCP. ACP still permits host-admitted MCP tools
      --no-hooks
          Run with every hook off for this invocation; configuration is unchanged
      --allow <RULE>
          Allow rule, e.g. 'read(src/**)'. Repeatable
      --deny <RULE>
          Deny rule, e.g. 'write(**)'. Repeatable
      --profile <PROFILE>
          Layer [profile.NAME] from config
      --trust-workspace
          Trust this launch's workspace without asking; remember the answer
      --trust-development-programs
          Trust this workspace and future edits to its current named programs; remember the grant
      --session-dir <SESSION_DIR>
          Override the store location
  -C <DIR>
          Run as if started in DIR
      --output <OUTPUT>
          text|json|jsonl [possible values: text, json, jsonl]
      --artifact-dir <ARTIFACT_DIR>
          Override the sanitized run-artifact root for profile `ci`
      --schema <SCHEMA>
          JSON Schema the final answer must validate against; `@file` or inline
      --schema-retries <SCHEMA_RETRIES>
          How many times to re-prompt on a schema mismatch [default: 2]
      --retry <RETRY>
          Additional delivery-safe attempts for each candidate
      --fallback <P/M>
          Ordered provider/model candidate used after a delivery-safe failure
      --budget <BUDGET>
          Hard cap: '2.5$' or '400k' tokens
      --max-output-tokens <MAX_OUTPUT_TOKENS>
          Maximum generated tokens per request
      --goal <GOAL>
          Set a checked session objective
      --no-context-tool
          Disable model-requested context transitions
      --context-clear
          Permit the model to discard conversation history using clear
      --context-interval <CONTEXT_INTERVAL>
          Minimum completed turns between model-requested transitions [default: 10]
      --goal-turns <GOAL_TURNS>
          Optional maximum automatic goal continuation turns
      --goal-check-command <GOAL_CHECK_COMMAND>
          Evaluator argv as a JSON string array; reads evidence JSON on stdin
      --max-turns <MAX_TURNS>
          Cap the turns this run may take
      --timeout <TIMEOUT>
          Wall clock, e.g. '90s' or '10m'
      --mcp <NAME|@FILE>
          Select an effective server by name, even one marked enabled = false, or attach a strict invocation-scoped JSON MCP server
      --mcp-only
          Attach only the servers that --mcp selects; enabled configured servers stay detached
      --allow-dir <PATH[:ro|rw]>
          Allow this run to reach one existing canonical absolute directory, read-only unless `:rw`. Not stored; `ego roots add` stores a root
      --mode <MODE>
          Permission mode: plan|default|auto|accept-edits|yolo (alias: edits)
      --plan
          Shorthand for --mode plan
      --yolo
          Shorthand for --mode yolo
  -c, --set <K=V>
          Dotted-path config override, value parsed as TOML
  -q, --quiet
          Suppress progress and diagnostics on stderr; stdout is unaffected
      --no-color
          Never emit ANSI colour, whatever the terminal claims to support
      --plain
          Append plain output without terminal styling or live retry updates
      --vim
          Use Vim normal/insert editing in the interactive composer
      --memory
          Keep a memory file for this workspace that the model can add to
      --no-session-compression
          Do not compress old sessions in the background during this run
      --no-checkpoints
          Do not save file checkpoints before writes during this run
  -h, --help
          Print help

With terminal input and output, no ID opens the saved-session picker across roots. Otherwise no ID selects the latest session in this workspace. Explicit --output, --answer, --file, stdin prompt '-' or profile ci select headless resumption.

A single non-UUID positional is a prompt-only form, not a session ID. For example: ego resume "next step" (or ego continue "next step"). Supply a UUID plus a prompt to name a specific session.
```
