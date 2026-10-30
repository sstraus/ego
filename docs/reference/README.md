# CLI reference

Use the CLI to start work, manage durable conversations, configure authority, or
connect a host. Every command below is in the current parser; accepting a command
name does not prove a provider credential or confinement backend is available.

| Command | Purpose |
| --- | --- |
| `ego [PROMPT]` | Interactive session |
| `ego run [PROMPT]` | Headless run; `-` takes the prompt from stdin |
| `ego resume [ID] [PROMPT]` | Select or resume durable work; `--answer QUESTION_ID=JSON` answers a pending question |
| `ego continue [ID] [PROMPT]` | Alias of `resume` |
| `ego rm ID` | Delete a stored session |
| `ego ls` | List this workspace's sessions; `--all` spans the store |
| `ego fork SOURCE` | Branch at the tip or after a completed message with `--at ITEM_ID` |
| `ego compact SOURCE` | Create a checkpoint successor; `--request-id UUID` provides retry identity |
| `ego export SOURCE` | Write local `md`, `json`, `jsonl`, or [followable transcript](transcript.md); `--sanitize` writes a structural bundle |
| `ego usage` | Report context, tokens, roles, and monetary state |
| `ego models` | Inspect model facts, source, freshness, and availability |
| `ego auth login\|logout\|status` | Manage a provider's subscription credential |
| `ego config get\|set\|unset\|ls\|edit\|capabilities\|snapshot\|batch` | Manage user configuration; `ls --effective` observes the current untrusted perimeter |
| `ego roots add\|rm\|ls` | Manage user/profile extra roots |
| `ego rules ls\|allow\|deny\|revoke\|remember\|explain` | Inspect and manage permission predicates |
| `ego mcp add\|rm\|ls\|login\|reload` | Configure and reconcile external MCP servers |
| `ego hooks ls` | Inspect admitted system/user hooks |
| `ego doctor` | Inspect one redacted health snapshot |
| `ego checkpoint SESSION_ID preview\|restore\|close` | Inspect or selectively restore session-owned file edits |
| `ego setup` | Inspect expected work without running effects |
| `ego worktree show\|cleanup RECEIPT_UUID` | Inspect or safely remove a retained checkout |
| `ego acp` | Serve ACP on stdio or an endpoint |
| `ego mcp-server` | Serve rooted runs on MCP stdio |
| `ego a2a --listen ENDPOINT` | Serve local/loopback A2A discovery; operational methods are refused |

`ego resume ID --name ...` and `--goal-operation ...` manage metadata outside the REPL;
these accept `--name show|set=TEXT|clear` and
`--goal-operation show|set=TEXT|achieve|block=REASON`. They are covered in
[sessions](../guides/sessions.md) and captured binary help.

## Flags and defaults

The [generated reference](cli.md) lists visible commands, including nested
management commands, and their flags.
Installed-binary flags are authoritative in `ego COMMAND SUBCOMMAND --help`;
[the fresh-build help snapshot](help.md) includes their help. [CLI.md](../../CLI.md) describes
output contracts and design; some examples there predate this checkout.

| Group | Controls | Default or constraint |
| --- | --- | --- |
| Generation | `--model`, `--thinking`, `--system`, `--append-system`, `--attach` | Explicit/configured model for headless; effort must be supported; images are capability-gated |
| Authority | `--mode`, `--plan`, `--yolo`, `--sandbox`, `--allow`, `--deny` | See [permissions](../guides/permissions.md); denies and system ceilings remain binding |
| Inputs | `--file`, `--var`, `--instructions`, `--bare` | File and argv prompt conflict; vars require a file; bare and explicit instructions conflict |
| Tools | `--tools`, `--no-tools`, `--mcp` | Built-ins plus admitted extensions; selectors restrict rather than grant |
| Limits | `--budget`, `--max-output-tokens`, `--max-turns`, `--timeout` | No invocation cap unless configured; system budget can clamp it |
| Resilience | `--retry`, repeatable `--fallback` | Delivery-safe failures only; no blind repeat of an uncertain effect |
| Validation | `--schema`, `--schema-retries` | Inline or `@file` JSON Schema; exhausted mismatches are task failure |
| State | `--session`, `--session-id`, `--session-dir`, `--artifact-dir` | Durable sessions; `--ephemeral` is explicitly refused |
| Configuration | `--profile`, `--set`, `-C` | Strict typed TOML overrides; workspace changes before discovery |
| Rendering | `--output`, `--quiet`, `--no-color` | Text default; JSON one final object; JSONL live events; quiet affects stderr |

## Output and exit codes

JSON contains the run result and settlement, not terminal decoration. JSONL is a
versioned public event stream; internal secret-bearing events are not dumped.
Listing, usage, doctor, and mutation receipts have their own versioned shapes;
never assume every JSON command has a `.result` property.

| Exit | Meaning |
| --- | --- |
| 0 | Done |
| 1 | Task failed, including schema exhaustion |
| 2 | Usage/configuration input error |
| 3 | Human input required in headless mode |
| 4 | Provider/network failure |
| 5 | Budget or turn limit |
| 6 | Host-requested pause |
| 124 | Timeout |
| 130 / 143 | SIGINT / SIGTERM |

A question exposes its ID and answer shape; resume that session with the matching
answer instead of inventing a new turn. A host pause requests no human payload.
Shell-specific completions can be generated from the CLI parser.
The parser-owned reference remains generated by
[the reference test](../../crates/ego-cli/tests/reference.rs).

Sources: [parser](../../crates/ego-cli/src/cli.rs) and
[output](../../crates/ego-cli/src/output.rs).
