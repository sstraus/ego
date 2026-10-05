# Automation and agent files

Headless runs put the harness into a script: an explicit prompt enters, structured
output and an exit status leave. They never stop to ask for trust or permission.
Choose the model, instructions, permitted effects, and limits explicitly.

## A bounded pipeline

```sh
printf '%s\n' 'Reply with one sentence about what a coding harness does.' | \
  ego run -m ollama/gemma4:e4b-mlx --thinking off --no-tools --timeout 30s --output json -
```

Replace the model with one your installation can resolve. For workspace-only
access, save `ego config set 'roots=[]'` in your user configuration first. The
absent default adds `~/Gits` when it exists. `-` selects stdin as
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
| `--budget 20k` / `--budget '2.5$'` | Bound tokens or money; monetary admission needs known pricing |
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
delimiters, and a Markdown prompt body. Its closed frontmatter keys are `model`,
`mode`, `sandbox`, `allow` (array), `schema`, and `vars` (declared variable names).
`ego run --file FILE --var NAME=VALUE` supplies declared variables. The loader
rejects unknown/duplicate fields and missing/extra variables. An argv prompt and
`--file` conflict. Variables do not turn text into shell evaluation.

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
[agent-file tests](../../crates/ego-cli/tests/agent_file.rs).
