# ego command reference

Generated from the clap definitions in `crates/ego-cli/src/cli.rs`.
Do not edit: `crates/ego-cli/tests/reference.rs` regenerates and compares it.

`ego PROMPT` with no subcommand starts an interactive session and is not
listed below, because it is the absence of a subcommand rather than one.

## `ego run`

Run an agent from a prompt, headless.

- `--allow <RULE>` — Allow rule, e.g. 'read(src/**)'. Repeatable
- `--append-system <APPEND_SYSTEM>` — Append to the system prompt. `@file` reads it from a file
- `--artifact-dir <ARTIFACT_DIR>` — Override the sanitized run-artifact root for profile `ci`
- `--attach <IMAGE_PATH|HTTPS_IMAGE_URL>` — Add exact image bytes as ordered context before the prompt text
- `--bare` — No ambient discovery, even in a trusted workspace
- `--budget <BUDGET>` — Hard cap: '2.5$' or '400k' tokens
- `--deny <RULE>` — Deny rule, e.g. 'write(**)'. Repeatable
- `-C <DIR>` — Run as if started in DIR
- `--ephemeral` — Refused: every run uses a durable session
- `--fallback <P/M>` — Ordered provider/model candidate used after a delivery-safe failure
- `-f, --file <FILE>` — Read the instruction and its invocation configuration from an agent file
- `-h, --help` — Print help
- `--instructions <INSTRUCTIONS>` — Use this instruction file instead of discovering one
- `--max-turns <MAX_TURNS>` — Cap the turns this run may take
- `--mcp <NAME|@FILE>` — Attach a named effective or strict invocation-scoped JSON MCP server
- `--mode <MODE>` — Permission mode: plan|default|auto|edits|yolo
- `-m, --model <MODEL>` — provider/model slug, e.g. ollama/qwen3
- `--no-color` — Never emit ANSI colour, whatever the terminal claims to support
- `--no-tools` — Offer no tools at all
- `--offline` — No non-loopback egress from ego's own HTTP transport
- `--output <OUTPUT>` — text|json|jsonl
- `--plan` — Shorthand for --mode plan
- `--profile <PROFILE>` — Layer [profile.NAME] from config
- `-q, --quiet` — Suppress progress and diagnostics on stderr; stdout is unaffected
- `--retry <RETRY>` — Additional delivery-safe attempts for each candidate
- `--sandbox <SANDBOX>` — Filesystem confinement: ro|workspace|off
- `--schema <SCHEMA>` — JSON Schema the final answer must validate against; `@file` or inline
- `--schema-retries <SCHEMA_RETRIES>` — How many times to re-prompt on a schema mismatch
- `--session <SESSION>` — Named session: created or attached
- `--session-dir <SESSION_DIR>` — Override the store location
- `--session-id <SESSION_ID>` — A session id supplied by the caller
- `-c, --set <K=V>` — Dotted-path config override, value parsed as TOML
- `--system <SYSTEM>` — Replace the system prompt. `@file` reads it from a file
- `--thinking <THINKING>` — off|minimal|low|medium|high|xhigh|max
- `--timeout <TIMEOUT>` — Wall clock, e.g. '90s' or '10m'
- `--tools <TOOLS>` — Restrict the builtin tools to this comma-separated list
- `--var <KEY=VALUE>` — Supply one declared agent-file template variable
- `--yolo` — Shorthand for --mode yolo
- `<PROMPT>` — The prompt. `-` reads it from stdin

## `ego resume`

Continue a session; no ID = latest in this workspace.

- `--allow <RULE>` — Allow rule, e.g. 'read(src/**)'. Repeatable
- `--answer <QUESTION_ID=JSON>` — Settle a pending question. Repeat for multiple sibling questions
- `--append-system <APPEND_SYSTEM>` — Append to the system prompt. `@file` reads it from a file
- `--artifact-dir <ARTIFACT_DIR>` — Override the sanitized run-artifact root for profile `ci`
- `--attach <IMAGE_PATH|HTTPS_IMAGE_URL>` — Add exact image bytes as ordered context before the prompt text
- `--bare` — No ambient discovery, even in a trusted workspace
- `--budget <BUDGET>` — Hard cap: '2.5$' or '400k' tokens
- `--deny <RULE>` — Deny rule, e.g. 'write(**)'. Repeatable
- `-C <DIR>` — Run as if started in DIR
- `--ephemeral` — Refused: every run uses a durable session
- `--fallback <P/M>` — Ordered provider/model candidate used after a delivery-safe failure
- `-f, --file <FILE>` — Read the instruction and its invocation configuration from an agent file
- `-h, --help` — Print help
- `--instructions <INSTRUCTIONS>` — Use this instruction file instead of discovering one
- `--max-turns <MAX_TURNS>` — Cap the turns this run may take
- `--mcp <NAME|@FILE>` — Attach a named effective or strict invocation-scoped JSON MCP server
- `--mode <MODE>` — Permission mode: plan|default|auto|edits|yolo
- `-m, --model <MODEL>` — provider/model slug, e.g. ollama/qwen3
- `--no-color` — Never emit ANSI colour, whatever the terminal claims to support
- `--no-tools` — Offer no tools at all
- `--offline` — No non-loopback egress from ego's own HTTP transport
- `--output <OUTPUT>` — text|json|jsonl
- `--plan` — Shorthand for --mode plan
- `--profile <PROFILE>` — Layer [profile.NAME] from config
- `-q, --quiet` — Suppress progress and diagnostics on stderr; stdout is unaffected
- `--retry <RETRY>` — Additional delivery-safe attempts for each candidate
- `--sandbox <SANDBOX>` — Filesystem confinement: ro|workspace|off
- `--schema <SCHEMA>` — JSON Schema the final answer must validate against; `@file` or inline
- `--schema-retries <SCHEMA_RETRIES>` — How many times to re-prompt on a schema mismatch
- `--session <SESSION>` — Named session: created or attached
- `--session-dir <SESSION_DIR>` — Override the store location
- `--session-id <SESSION_ID>` — A session id supplied by the caller
- `-c, --set <K=V>` — Dotted-path config override, value parsed as TOML
- `--system <SYSTEM>` — Replace the system prompt. `@file` reads it from a file
- `--thinking <THINKING>` — off|minimal|low|medium|high|xhigh|max
- `--timeout <TIMEOUT>` — Wall clock, e.g. '90s' or '10m'
- `--tools <TOOLS>` — Restrict the builtin tools to this comma-separated list
- `--var <KEY=VALUE>` — Supply one declared agent-file template variable
- `--yolo` — Shorthand for --mode yolo
- `<ID>` — The session to continue. Omit it for the latest in this workspace
- `<PROMPT>` — The prompt. `-` reads it from stdin

## `ego fork`

Fork a session at its tip or after the completed turn containing a message.

- `--at <ITEM_ID>` — (undocumented)
- `-C <DIR>` — Run as if started in DIR
- `-h, --help` — Print help
- `--output <OUTPUT>` — text|json|jsonl
- `--session-dir <SESSION_DIR>` — Override the store location
- `<SOURCE>` — (undocumented)

## `ego compact`

Compact a session into its deterministic successor.

- `-C <DIR>` — Run as if started in DIR
- `-h, --help` — Print help
- `--output <OUTPUT>` — text|json|jsonl
- `--request-id <REQUEST_ID>` — (undocumented)
- `--session-dir <SESSION_DIR>` — Override the store location
- `<SOURCE>` — (undocumented)

## `ego export`

Export a session locally or as a structurally sanitized bundle.

- `-C <DIR>` — Run as if started in DIR
- `--file <PATH>` — (undocumented)
- `--format <FORMAT>` — (undocumented)
- `-h, --help` — Print help
- `--sanitize` — (undocumented)
- `--session-dir <SESSION_DIR>` — Override the store location
- `<SOURCE>` — (undocumented)

## `ego ls`

List sessions (this workspace; --all for every one).

- `--all` — Every session in the store, not just this workspace's
- `-C <DIR>` — Run as if started in DIR
- `-h, --help` — Print help
- `--output <OUTPUT>` — text|json|jsonl
- `--session-dir <SESSION_DIR>` — Override the store location

## `ego usage`

Report usage, context and cost for this workspace.

- `--all-roles` — Keep role rows that recorded no activity in the text table
- `-C <DIR>` — Run as if started in DIR
- `-h, --help` — Print help
- `--json` — Emit the exact versioned usage-report object
- `--period <PERIOD>` — all|Ns|Nm|Nh
- `--session-dir <SESSION_DIR>` — Override the store location

## `ego models`

List the shared model catalogue.

- `-h, --help` — Print help
- `--json` — Emit the exact versioned snapshot object
- `--provider <PROVIDER>` — Restrict enumeration and output to one admitted provider
- `--refresh` — Refresh the selected discoverable provider slices

## `ego hooks`

Inspect the admitted system and user hook catalogue.

- `hooks ls` — List the admitted hook catalogue without changing configuration

- `-h, --help` — Print help

## `ego doctor`

Report a read-only redacted health snapshot of this installation.

- `-h, --help` — Print help
- `--json` — Emit the exact versioned health snapshot

## `ego acp`

Serve the Agent Client Protocol on stdio or an endpoint.

- `--allow <RULE>` — Allow rule, e.g. 'read(src/**)'. Repeatable
- `--allow-public-bind` — Serve on an address reachable from outside this machine
- `--append-system <APPEND_SYSTEM>` — Append to the system prompt. `@file` reads it from a file
- `--connection-lifetime <CONNECTION_LIFETIME>` — Maximum lifetime of one listener connection: 90s, 10m or 2h
- `--deny <RULE>` — Deny rule, e.g. 'write(**)'. Repeatable
- `-C <DIR>` — Run as if started in DIR
- `-h, --help` — Print help
- `--instructions <INSTRUCTIONS>` — Use this instruction file instead of discovering one
- `--listen <LISTEN>` — Serve on a socket path or address instead of stdio
- `--max-connections <MAX_CONNECTIONS>` — Maximum simultaneous listener connections
- `--max-ingress-bytes <MAX_INGRESS_BYTES>` — Maximum cumulative bytes read from one listener connection
- `--no-tools` — Offer no tools at all
- `--profile <PROFILE>` — Layer [profile.NAME] from config
- `--sandbox <SANDBOX>` — The weakest confinement a session may select: ro|workspace|off
- `--session-dir <SESSION_DIR>` — Override the store location
- `--tools <TOOLS>` — Restrict the builtin tools to this comma-separated list

## `ego a2a`

Serve the Agent2Agent protocol on a local or loopback endpoint.

- `--allow-public-bind` — Serve on an address reachable from outside this machine
- `-C <DIR>` — Run as if started in DIR
- `-h, --help` — Print help (see more with '--help')
- `--listen <LISTEN>` — Where to serve: a socket path, or an address such as `127.0.0.1:8080`
- `--sandbox <SANDBOX>` — The weakest confinement a session may select: ro|workspace|off
- `--session-dir <SESSION_DIR>` — Override the store location

## `ego auth`

Manage subscription credentials.

- `auth login` — Authenticate and store a provider credential
- `auth logout` — Remove a stored provider credential
- `auth status` — Show credential type and expiry, never token values

- `-h, --help` — Print help

## `ego mcp`

Manage configured MCP servers.

- `mcp add` — Add or replace one complete server entry
- `mcp rm` — Remove, mask, or reveal one configured server entry
- `mcp ls` — List configured and admitted MCP state
- `mcp login` — Authorize one configured HTTP server through the product CIMD binding
- `mcp reload` — Reconcile every configured server, or one named server

- `-h, --help` — Print help

## `ego config`

Manage stored user configuration.

- `config get` — Read one exact stored value
- `config set` — Store one strictly typed TOML value
- `config unset` — Remove one exact stored value
- `config ls` — List the exact stored managed projection
- `config edit` — Edit the complete user document in the selected external editor

- `-h, --help` — Print help

## `ego rules`

Inspect and manage persistent permission rules.

- `rules ls` — List the effective system and selected user rules
- `rules allow` — Persist one allow predicate
- `rules deny` — Persist one deny predicate
- `rules revoke` — Remove one predicate ID from both selected user lists
- `rules remember` — Derive persistent rules from one prepared builtin call
- `rules explain` — Explain one prepared builtin call without running it

- `-h, --help` — Print help

## `ego mcp-server`

Serve scoped ego runs over MCP on stdio.

- `-C <DIR>` — Run as if started in DIR
- `-h, --help` — Print help
- `--max-concurrent-runs <MAX_CONCURRENT_RUNS>` — (undocumented)
- `--max-resource-bytes <MAX_RESOURCE_BYTES>` — (undocumented)
- `--retained-requests <RETAINED_REQUESTS>` — (undocumented)
- `--session-dir <SESSION_DIR>` — Override the store location

