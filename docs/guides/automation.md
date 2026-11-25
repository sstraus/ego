# Automation and agent files

Read this page in the [hosted user guide](https://ego.tuicommander.com/docs/guides/automation.html).

Headless runs put the harness into a script: an explicit prompt enters, structured
output and an exit status leave. They never stop to ask for trust or permission.
Choose the model, instructions, permitted effects, and limits explicitly.

## A bounded pipeline

```sh
printf '%s\n' 'Reply with one sentence about what a coding harness does.' | \
  ego run -m ollama/gemma4:e4b-mlx --thinking off --no-tools --timeout 30s --output json -
```

Replace the model with one your installation can resolve. To clear configured
extra roots, save `ego config set 'roots=[]'` in your user configuration first.
Select `--sandbox workspace` explicitly when you need that confinement level;
the macOS default is adaptive and can derive additional roots. Under strict
confinement, an absent root declaration grants no extra home-tree access. `-` selects stdin as
prompt. JSON writes one final object; JSONL writes live versioned public events;
text writes the answer with diagnostics on stderr. `--quiet` suppresses stderr
progress, not the result. [CLI reference](../reference/README.md) lists exits.

Headless is untrusted/bare by default. To use an instruction file, select it with
`--instructions FILE`; do not combine that with `--bare`. Tool requests outside
available authority stop with the recorded reason. `--allow` is a rule, not
workspace trust; `--no-tools` restricts the catalogue.

## Limits, schemas, retries, and images

| Control | Purpose |
| --- | --- |
| `--timeout 30s`, `--max-turns N` | Bound elapsed work or turns |
| `--max-output-tokens N` | Limit generated tokens per request, under model and system ceilings |
| `--budget 20k` / `--budget '2.5$'` | Bound tokens or money; money caps cover known recorded spend only |
| `--schema @FILE` or inline schema | Validate the final answer; the validated object becomes the result |
| `--schema-retries N` | Re-prompt after mismatch, then fail if still invalid |
| `--retry N`, repeatable `--fallback PROVIDER/MODEL` | Retry or change provider only after a delivery-safe failure |
| `--attach PATH_OR_HTTPS_URL` | Supply exact ordered image bytes before prompt text, under image capabilities and guarded URL retrieval |

Context overflow is a typed provider failure, not output truncation. It is not
blindly retried; compact the source with its explicit lifecycle operation.
Ambiguous delivery and effects are not proof that nothing ran. See
[uncertainty](../concepts/README.md).

## Executable agent files

An agent file packages invocation flags and a prompt; a skill adds instructions
inside an already-running session. They use the same bounded strict YAML envelope
but have different lifecycles.

An agent file can use `#!/usr/bin/env -S ego run --file`, standalone `---` YAML
delimiters, and a Markdown prompt body. The frontmatter must start with
`version: 1`; a file without it is refused. The other keys are optional:

| Key | Value | Same as |
| --- | --- | --- |
| `model` | `provider/model` string | `--model` |
| `effort` | `off`, `minimal`, `low`, `medium`, `high`, `xhigh` or `max` | `--thinking` |
| `mode` | `plan`, `default`, `auto`, `accept-edits` or `yolo` | `--mode` |
| `sandbox` | `ro`, `workspace`, `adaptive` or `off` | `--sandbox` |
| `allow` | List of permission rules | `--allow` |
| `deny` | List of permission rules | `--deny` |
| `schema` | Inline JSON Schema, or `@FILE` relative to the agent file | `--schema` |
| `vars` | List of variable names used as `{{name}}` in the body | |
| `tools`, `roots` | Lists; accepted only in named task definitions | |

`ego run --file` refuses `tools` and `roots`; they are tool and root ceilings
for named task definitions, which also cannot set `allow` or `schema`. Flags on
the command line override the values in the file.

`ego run --file FILE --var NAME=VALUE` supplies declared variables. The loader
rejects unknown and duplicate fields, and variables that are declared, used in
the body and supplied must match exactly. An argv prompt and `--file` conflict,
and so do `--file` and `-`; piped stdin is appended to the body as a fenced
block of context. Variables do not turn text into shell evaluation.

A complete example, `fix-tests.ego`:

```markdown
#!/usr/bin/env -S ego run --file
---
version: 1
model: ollama/gemma4:e4b-mlx
effort: medium
mode: accept-edits
sandbox: workspace
allow: ["exec(cargo test *)"]
deny: ["exec(git push *)"]
schema: "@fix-tests.schema.json"
vars: [package]
---
Run the tests of the {{package}} package, fix any failure, and report the result.
```

and `fix-tests.schema.json` next to it:

```json
{"type":"object","required":["passed","summary"],"additionalProperties":false,
 "properties":{"passed":{"type":"boolean"},"summary":{"type":"string"}}}
```

```sh
chmod +x fix-tests.ego
./fix-tests.ego --var package=my-crate --output json
```

## Goals

A goal keeps a run working until a check says the objective is met. After each
completed turn, a separate check without tools compares the objective with the
session evidence. If the condition is unmet and the agent can still do useful,
authorized work, ego starts an automatic continuation turn.

```sh
ego run --goal 'All tests in crates/parser pass' --goal-turns 10 \
  --allow 'exec(cargo test *)' --output json 'Fix the failing parser tests.'
```

- `--goal TEXT` sets an operator goal for `ego run`, `ego resume ID` and
  `ego continue ID`. Interactively, `/goal TEXT` (or `/goal set TEXT`) does the
  same and starts the first work turn.
- `--goal-turns N` limits automatic continuation turns. An operator goal has no
  limit unless you set one; `0` stops before the first automatic turn. The value
  applies when checks first start and does not change a saved limit. Automatic
  turns also count toward `--max-turns`.
- The model can set its own goal with the `goal` tool only when no goal is
  active, and it cannot replace an active goal. Automatic work on a model-set
  goal pauses after 20 continuation turns. `/goal resume` grants it 20 more and
  starts a work turn; `ego resume ID --goal-operation resume` grants them
  without starting a turn. Neither applies to an operator goal or to a goal
  that has not paused.
- There is no extension command: `/goal extend` is refused. To change an
  objective, set a new operator goal.
- `/goal show`, `/goal achieve` and `/goal block REASON` inspect or settle the
  goal; `ego resume ID --goal-operation show|set=TEXT|achieve|resume|block=REASON`
  does the same without starting a turn.

Five checks in a row that report the same unmet condition block the goal. After
two continuation turns with no tool call and no new input, ego waits for the next
inbound event instead of starting another turn.

### Goal check command

By default the check is a model generation, accounted as `goal_check` usage.
`--goal-check-command` replaces it with a program of your own. Give its argv as
one JSON array:

```sh
ego run --goal 'The release build succeeds' \
  --goal-check-command '["python3", "check_goal.py"]' \
  --allow 'exec(python3 check_goal.py)' 'Prepare the release build.'
```

The program receives the objective and the session evidence as JSON on stdin,
the same text a model check reads. Images and audio never appear as bytes, only
as descriptors:

```json
{"image_evidence":{"reference":"SHA256:BYTES","media_type":"png","width":640,"height":480,"available_to_checker":false}}
{"audio_evidence":{"reference":"SHA256:BYTES","media_type":"wav","available_to_checker":false}}
```

It writes one JSON object on stdout:

```json
{"verdict":"continue","reason":"tests in crates/parser/src/lex.rs still fail; fix the tokenizer next"}
```

| `verdict` | Effect |
| --- | --- |
| `achieved` | The goal is achieved. The reason must state the evidence of success |
| `continue` | The agent has a useful, authorized next action; name it in the reason. Starts a continuation turn |
| `waiting` | Nothing useful can be done now; the next step depends on work already in flight or an external event, named in the reason. The goal stays active, no turn starts, and the next inbound event resumes work |
| `needs_operator` | Only a person can unblock the goal (login, approval, decision). The goal blocks at once with the reason; start a new goal after resolving it |

`verdict` and a non-empty `reason` are required; any other field is refused, and
an invalid answer stops automatic work while the goal stays active. When the
evidence contains an image descriptor, an `achieved` verdict must also contain
`"image_dependent": false`, stating that success does not rest on an image the
check could not see. Otherwise the goal blocks with the reason
`visual evidence unavailable to the goal check: REASON`. Audio follows the same
rule with `"audio_dependent": false`.

The command runs under ordinary policy: grant its exact argv with an `--allow`
rule, or the check fails with `goal check command requires an explicit exec
grant`. It runs inside the run's confinement, so the script and its inputs must
be readable there. It has a 60-second limit. Stderr is not part of the verdict;
when the command fails, the diagnostic shows at most the last 4096 bytes of its
stderr. Command checks cost no model tokens.

## CI posture

`--profile ci` or `EGO_PROFILE=ci` explicitly selects the CI contract; ambient
`CI` does not. The profile requires explicit instructions, a headless prompt and
credential source, and pre-existing absolute session/artifact roots disjoint from
each other and the workspace. An admitted outcome publishes one canonical sanitized
bundle, independently of stdout's JSONL stream. Admission failures publish none.
Full keys and invariants are in [configuration](../reference/configuration.md).

Sources: [agent-file parser](../../crates/ego-cli/src/agent_file.rs),
[schema validator](../../crates/ego-cli/src/schema.rs),
[prompt ownership](../../crates/ego-cli/src/prompt.rs), and
agent-file tests.
