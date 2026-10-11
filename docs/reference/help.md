# CLI help from the verified build

Captured from a fresh `ego-cli 0.1.0` source installation on 2026-10-05.
This supplements the generator-owned [command reference](cli.md) with complete
nested help, required arguments, parser defaults, and metadata-only resume help.

## `ego --help`

```text
ego 0.1.0 — an agent you run from a terminal.

Usage:
  ego [OPTIONS] [PROMPT]
  ego <COMMAND> [OPTIONS]

Interactive:
  ego -m ollama/qwen3

Commands:
  run         Run an agent from a prompt, headless
  resume      Continue a session; no ID = latest in this workspace
  fork        Fork a session at its tip or after a completed turn
  compact     Compact a session into its deterministic successor
  export      Export a session or sanitized bundle
  ls          List sessions (this workspace; --all for every one)
  usage       Report usage, context and cost for this workspace
  models      List the shared model catalogue
  hooks       Inspect the admitted hook catalogue
  doctor      Print a read-only redacted health snapshot
  acp         Serve the Agent Client Protocol on stdio
  a2a         Serve Agent2Agent discovery information
  auth        Manage subscription credentials
  mcp         Manage configured MCP servers
  config      Manage stored user configuration
  roots       Manage perimeter roots
  rules       Inspect and manage permission rules
  mcp-server  Serve scoped ego runs over MCP on stdio

`ego <COMMAND> --help` describes one of them. CLI.md §2 documents the remaining commands; each is refused by name, with the subsystem it waits on. Start Ollama and run `ollama pull qwen3` before the local example above.
```

## `ego run --help`

```text
Run an agent from a prompt, headless.

Usage: ego run [OPTIONS] [PROMPT]

Arguments:
  [PROMPT]  The prompt. `-` reads it from stdin

Options:
  -f, --file <FILE>
          Read the instruction and its invocation configuration from an agent file
      --var <KEY=VALUE>
          Supply one declared agent-file template variable
      --attach <IMAGE_PATH|HTTPS_IMAGE_URL>
          Add exact image bytes as ordered context before the prompt text
  -m, --model <MODEL>
          provider/model slug, e.g. ollama/qwen3
      --offline
          No non-loopback egress from ego's own HTTP transport
      --sandbox <SANDBOX>
          Filesystem confinement: ro|workspace|off
      --thinking <THINKING>
          off|minimal|low|medium|high|xhigh|max
      --system <SYSTEM>
          Replace the system prompt. `@file` reads it from a file
      --append-system <APPEND_SYSTEM>
          Append to the system prompt. `@file` reads it from a file
      --instructions <INSTRUCTIONS>
          Use this instruction file instead of discovering one
      --tools <TOOLS>
          Restrict the builtin tools to this comma-separated list
      --no-tools
          Offer no tools at all
      --allow <RULE>
          Allow rule, e.g. 'read(src/**)'. Repeatable
      --deny <RULE>
          Deny rule, e.g. 'write(**)'. Repeatable
      --profile <PROFILE>
          Layer [profile.NAME] from config
      --bare
          No ambient discovery, even in a trusted workspace
      --session <SESSION>
          Named session: created or attached
      --session-id <SESSION_ID>
          A session id supplied by the caller
      --ephemeral
          Refused: every run uses a durable session
      --session-dir <SESSION_DIR>
          Override the store location
  -C <DIR>
          Run as if started in DIR
      --output <OUTPUT>
          text|json|jsonl [default: text] [possible values: text, json, jsonl]
      --artifact-dir <ARTIFACT_DIR>
          Override the sanitized run-artifact root for profile `ci`
      --schema <SCHEMA>
          JSON Schema the final answer must validate against; `@file` or inline
      --schema-retries <SCHEMA_RETRIES>
          How many times to re-prompt on a schema mismatch [default: 2]
      --retry <RETRY>
          Additional delivery-safe attempts for each candidate [default: 0]
      --fallback <P/M>
          Ordered provider/model candidate used after a delivery-safe failure
      --budget <BUDGET>
          Hard cap: '2.5$' or '400k' tokens
      --max-turns <MAX_TURNS>
          Cap the turns this run may take
      --timeout <TIMEOUT>
          Wall clock, e.g. '90s' or '10m'
      --mcp <NAME|@FILE>
          Select an effective server by name, even one marked enabled = false, or attach a strict invocation-scoped JSON MCP server
      --mode <MODE>
          Permission mode: plan|default|auto|edits|yolo
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
  -h, --help
          Print help
```

## `ego resume --help`

```text
Continue a session; no ID = latest in this workspace.

Usage: ego resume [OPTIONS] [ID] [PROMPT]

Arguments:
  [ID]      The session to continue. Omit it for the latest in this workspace
  [PROMPT]  The prompt. `-` reads it from stdin

Options:
      --answer <QUESTION_ID=JSON>
          Settle a pending question. Repeat for multiple sibling questions
  -f, --file <FILE>
          Read the instruction and its invocation configuration from an agent file
      --var <KEY=VALUE>
          Supply one declared agent-file template variable
      --attach <IMAGE_PATH|HTTPS_IMAGE_URL>
          Add exact image bytes as ordered context before the prompt text
  -m, --model <MODEL>
          provider/model slug, e.g. ollama/qwen3
      --offline
          No non-loopback egress from ego's own HTTP transport
      --sandbox <SANDBOX>
          Filesystem confinement: ro|workspace|off
      --thinking <THINKING>
          off|minimal|low|medium|high|xhigh|max
      --system <SYSTEM>
          Replace the system prompt. `@file` reads it from a file
      --append-system <APPEND_SYSTEM>
          Append to the system prompt. `@file` reads it from a file
      --instructions <INSTRUCTIONS>
          Use this instruction file instead of discovering one
      --tools <TOOLS>
          Restrict the builtin tools to this comma-separated list
      --no-tools
          Offer no tools at all
      --allow <RULE>
          Allow rule, e.g. 'read(src/**)'. Repeatable
      --deny <RULE>
          Deny rule, e.g. 'write(**)'. Repeatable
      --profile <PROFILE>
          Layer [profile.NAME] from config
      --bare
          No ambient discovery, even in a trusted workspace
      --session <SESSION>
          Named session: created or attached
      --session-id <SESSION_ID>
          A session id supplied by the caller
      --ephemeral
          Refused: every run uses a durable session
      --session-dir <SESSION_DIR>
          Override the store location
  -C <DIR>
          Run as if started in DIR
      --output <OUTPUT>
          text|json|jsonl [default: text] [possible values: text, json, jsonl]
      --artifact-dir <ARTIFACT_DIR>
          Override the sanitized run-artifact root for profile `ci`
      --schema <SCHEMA>
          JSON Schema the final answer must validate against; `@file` or inline
      --schema-retries <SCHEMA_RETRIES>
          How many times to re-prompt on a schema mismatch [default: 2]
      --retry <RETRY>
          Additional delivery-safe attempts for each candidate [default: 0]
      --fallback <P/M>
          Ordered provider/model candidate used after a delivery-safe failure
      --budget <BUDGET>
          Hard cap: '2.5$' or '400k' tokens
      --max-turns <MAX_TURNS>
          Cap the turns this run may take
      --timeout <TIMEOUT>
          Wall clock, e.g. '90s' or '10m'
      --mcp <NAME|@FILE>
          Select an effective server by name, even one marked enabled = false, or attach a strict invocation-scoped JSON MCP server
      --mode <MODE>
          Permission mode: plan|default|auto|edits|yolo
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
  -h, --help
          Print help
```

## `ego fork --help`

```text
Fork a session at its tip or after the completed turn containing a message.

Usage: ego fork [OPTIONS] <SOURCE>

Arguments:
  <SOURCE>  

Options:
      --at <ITEM_ID>               
      --session-dir <SESSION_DIR>  Override the store location
  -C <DIR>                         Run as if started in DIR
      --output <OUTPUT>            text|json|jsonl [default: text] [possible values: text, json, jsonl]
  -h, --help                       Print help
```

## `ego compact --help`

```text
Compact a session into its deterministic successor.

Usage: ego compact [OPTIONS] <SOURCE>

Arguments:
  <SOURCE>  

Options:
      --request-id <REQUEST_ID>    
      --session-dir <SESSION_DIR>  Override the store location
  -C <DIR>                         Run as if started in DIR
      --output <OUTPUT>            text|json|jsonl [default: text] [possible values: text, json, jsonl]
  -h, --help                       Print help
```

## `ego export --help`

```text
Export a session locally or as a structurally sanitized bundle.

Usage: ego export [OPTIONS] <SOURCE>

Arguments:
  <SOURCE>  

Options:
      --sanitize                   
      --format <FORMAT>            [default: md] [possible values: md, json, jsonl]
      --file <PATH>                
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
      --output <OUTPUT>            text|json|jsonl [default: text] [possible values: text, json, jsonl]
  -h, --help                       Print help
```

## `ego usage --help`

```text
Report usage, context and cost for this workspace.

Usage: ego usage [OPTIONS]

Options:
      --period <PERIOD>            all|Ns|Nm|Nh [default: all]
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
      --json  Emit the exact versioned health snapshot
  -h, --help  Print help
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
          The weakest confinement a session may select: ro|workspace|off
      --append-system <APPEND_SYSTEM>
          Append to the system prompt. `@file` reads it from a file
      --instructions <INSTRUCTIONS>
          Use this instruction file instead of discovering one
      --tools <TOOLS>
          Restrict the builtin tools to this comma-separated list
      --no-tools
          Offer no tools at all
      --allow <RULE>
          Allow rule, e.g. 'read(src/**)'. Repeatable
      --deny <RULE>
          Deny rule, e.g. 'write(**)'. Repeatable
      --profile <PROFILE>
          Layer [profile.NAME] from config
      --listen <LISTEN>
          Serve on a socket path or address instead of stdio
      --allow-public-bind
          Serve on an address reachable from outside this machine
      --max-connections <MAX_CONNECTIONS>
          Maximum simultaneous listener connections
      --connection-lifetime <CONNECTION_LIFETIME>
          Maximum lifetime of one listener connection: 90s, 10m or 2h
      --max-ingress-bytes <MAX_INGRESS_BYTES>
          Maximum cumulative bytes read from one listener connection
  -h, --help
          Print help
```

## `ego a2a --help`

```text
`ego a2a` — the Agent2Agent protocol, on an endpoint.

`--listen` is required and has no default. Every other surface ego serves speaks over stdio to a parent that already had the right to run it; this one opens an endpoint, and an endpoint that appears because a default said so is one nobody decided to open.

Usage: ego a2a [OPTIONS] --listen <LISTEN>

Options:
      --session-dir <SESSION_DIR>
          Override the store location

  -C <DIR>
          Run as if started in DIR

      --sandbox <SANDBOX>
          The weakest confinement a session may select: ro|workspace|off

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
Manage subscription credentials.

Usage: ego auth <COMMAND>

Commands:
  login   Authenticate and store a provider credential
  logout  Remove a stored provider credential
  status  Show credential type and expiry, never token values
  help    Print this message or the help of the given subcommand(s)

Options:
  -h, --help  Print help
```

## `ego auth login --help`

```text
Authenticate and store a provider credential

Usage: ego auth login [OPTIONS] [PROVIDER]

Arguments:
  [PROVIDER]  

Options:
      --device  Prefer device-code login when the provider offers it
  -h, --help    Print help
```

## `ego auth logout --help`

```text
Remove a stored provider credential

Usage: ego auth logout [PROVIDER]

Arguments:
  [PROVIDER]  

Options:
  -h, --help  Print help
```

## `ego auth status --help`

```text
Show credential type and expiry, never token values

Usage: ego auth status [PROVIDER]

Arguments:
  [PROVIDER]  

Options:
  -h, --help  Print help
```

## `ego mcp --help`

```text
Manage configured MCP servers.

Usage: ego mcp <COMMAND>

Commands:
  add     Add or replace one complete server entry
  rm      Remove, mask, or reveal one configured server entry
  ls      List configured and admitted MCP state
  login   Authorize one configured HTTP server through the product CIMD binding
  reload  Reconcile every configured server, or one named server

Options:
  -h, --help  Print help
```

## `ego mcp add --help`

```text
Add or replace one complete server entry

Usage: ego mcp add <NAME> --stdio [--cwd <DIR>] [--inherit-env <NAME>]... -- <COMMAND> [ARG]...
       ego mcp add <NAME> --http <URL> [--auth <none|oauth>] [--allow-insecure-loopback]

Arguments:
  <NAME>  

Options:
      --stdio                    Configure a subprocess transport. Its command follows literal `--`
      --http <URL>               Configure a Streamable HTTP transport
      --cwd <DIR>                Working directory for the stdio child
      --inherit-env <NAME>       Environment variable inherited by the stdio child. Repeatable
      --auth <AUTH>              HTTP authentication mode [possible values: none, oauth]
      --allow-insecure-loopback  Consent to plaintext HTTP for an exact loopback host
  -h, --help                     Print help
```

## `ego mcp rm --help`

```text
Remove, mask, or reveal one configured server entry

Usage: ego mcp rm <NAME>

Arguments:
  <NAME>  

Options:
  -h, --help  Print help
```

## `ego mcp ls --help`

```text
List configured and admitted MCP state

Usage: ego mcp ls [OPTIONS]

Options:
      --json  Emit the typed machine-readable projection
  -h, --help  Print help
```

## `ego mcp login --help`

```text
Authorize one configured HTTP server through the product CIMD binding

Usage: ego mcp login <NAME>

Arguments:
  <NAME>  

Options:
  -h, --help  Print help
```

## `ego mcp reload --help`

```text
Reconcile every configured server, or one named server

Usage: ego mcp reload [NAME]

Arguments:
  [NAME]  

Options:
  -h, --help  Print help
```

## `ego config --help`

```text
Manage stored user configuration.

Usage: ego config <COMMAND>

Commands:
  get    Read one exact stored value
  set    Store one strictly typed TOML value
  unset  Remove one exact stored value
  ls     List the exact stored managed projection
  edit   Edit the complete user document in the selected external editor

Options:
  -h, --help  Print help
```

## `ego config get --help`

```text
Read one exact stored value

Usage: ego config get [OPTIONS] <KEY>

Arguments:
  <KEY>  

Options:
      --profile <NAME>  
      --json            
  -h, --help            Print help
```

## `ego config set --help`

```text
Store one strictly typed TOML value

Usage: ego config set [OPTIONS] <KEY=TOML_VALUE>

Arguments:
  <KEY=TOML_VALUE>  

Options:
      --profile <NAME>  
  -h, --help            Print help
```

## `ego config unset --help`

```text
Remove one exact stored value

Usage: ego config unset [OPTIONS] <KEY>

Arguments:
  <KEY>  

Options:
      --profile <NAME>  
  -h, --help            Print help
```

## `ego config ls --help`

```text
List the exact stored managed projection

Usage: ego config ls [OPTIONS]

Options:
      --profile <NAME>  
      --json            
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
  <PATH>  

Options:
      --profile <PROFILE>  
      --access <ACCESS>    [default: read]
  -h, --help               Print help
```

## `ego roots rm --help`

```text
Remove a root by its stored spelling

Usage: ego roots rm [OPTIONS] <PATH>

Arguments:
  <PATH>  

Options:
      --profile <PROFILE>  
  -h, --help               Print help
```

## `ego roots ls --help`

```text
List roots stored in the selected user target

Usage: ego roots ls [OPTIONS]

Options:
      --profile <PROFILE>  
      --json               
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
      --profile <NAME>  
      --json            
  -h, --help            Print help
```

## `ego rules allow --help`

```text
Persist one allow predicate

Usage: ego rules allow [OPTIONS] <RULE>

Arguments:
  <RULE>  

Options:
      --profile <NAME>  
  -h, --help            Print help
```

## `ego rules deny --help`

```text
Persist one deny predicate

Usage: ego rules deny [OPTIONS] <RULE>

Arguments:
  <RULE>  

Options:
      --profile <NAME>  
  -h, --help            Print help
```

## `ego rules revoke --help`

```text
Remove one predicate ID from both selected user lists

Usage: ego rules revoke [OPTIONS] <ID>

Arguments:
  <ID>  

Options:
      --profile <NAME>  
  -h, --help            Print help
```

## `ego rules remember --help`

```text
Derive persistent rules from one prepared builtin call

Usage: ego rules remember [OPTIONS] <EFFECT> <TOOL> <ARGS_JSON>

Arguments:
  <EFFECT>     
  <TOOL>       
  <ARGS_JSON>  

Options:
      --profile <NAME>  
  -C <DIR>              
      --yes             
  -h, --help            Print help
```

## `ego rules explain --help`

```text
Explain one prepared builtin call without running it

Usage: ego rules explain [OPTIONS] <TOOL> <ARGS_JSON>

Arguments:
  <TOOL>       
  <ARGS_JSON>  

Options:
      --profile <NAME>  
  -C <DIR>              
      --json            
  -h, --help            Print help
```

## `ego mcp-server --help`

```text
Serve scoped ego runs over MCP on stdio.

Usage: ego mcp-server [OPTIONS] --max-concurrent-runs <MAX_CONCURRENT_RUNS> --retained-requests <RETAINED_REQUESTS> --max-resource-bytes <MAX_RESOURCE_BYTES>

Options:
      --session-dir <SESSION_DIR>                  Override the store location
  -C <DIR>                                         Run as if started in DIR
      --max-concurrent-runs <MAX_CONCURRENT_RUNS>  
      --retained-requests <RETAINED_REQUESTS>      
      --max-resource-bytes <MAX_RESOURCE_BYTES>    
  -h, --help                                       Print help
```

## `ego resume --name show --help`

```text
Read or change durable session metadata.

Usage: ego resume [OPTIONS] <ID>

Arguments:
  <ID>  

Options:
      --name <NAME>                
      --goal <GOAL>                
      --session-dir <SESSION_DIR>  Override the store location
  -C <DIR>                         Run as if started in DIR
      --output <OUTPUT>            text|json|jsonl [default: text] [possible values: text, json, jsonl]
  -h, --help                       Print help
```

