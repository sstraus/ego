# Bounded one-shot text transformers

Use `ego run` when another program supplies a prompt on stdin and consumes the assistant's answer. Keep launch restrictions in the calling program; a prompt is not a permission boundary.

## A bounded recipe

```sh
ego run --bare --no-tools --no-hooks --no-context-tool \
  --no-session-compression --no-checkpoints --sandbox ro --mode plan \
  --max-turns 1 --max-output-tokens 1024 --timeout 45s --retry 0 \
  --no-color --plain - < input.txt > output.txt 2> diagnostic.txt
status=$?
if [ "$status" -ne 0 ]; then
  printf 'ego failed with status %s; inspect diagnostic.txt\n' "$status" >&2
  exit "$status"
fi
```

The final `-` reads stdin. `--plain` requests plain assistant text rather than a structured run envelope; `--no-color` disables color formatting. Keep stdout and stderr separate, check the process status first, and then validate the answer using the consumer's own parser and schema. Requesting JSON in the prompt does not make every successful response valid JSON. Never accept partial stdout after a failed run as a successful transformation.

For example, a JSON consumer can run this after a successful process exit:

```sh
python3 -c 'import json,sys; value=json.load(sys.stdin); assert isinstance(value, dict)' < output.txt
```

Replace the illustrative object check with the actual required fields, types and allowed values. Bound the consumer's input/output sizes and execution time as well; a model token limit is not the caller's complete resource policy.

## What the restrictions mean

- `--bare` avoids automatically discovered project context; it does not select a new provider or account. Existing user model/account settings remain inherited and can change when their owner edits them.
- `--no-tools`, `--no-hooks` and `--no-context-tool` exclude those agent capabilities from this run. Read-only confinement and plan mode are separate restrictions; neither is a substitute for disabling tools when a transformer does not need them.
- One turn, bounded output, a timeout and zero retries keep the call bounded. Do not add a caller retry loop or provider fallback implicitly.
- Compression and checkpoints are disabled for this disposable transformation. This is not a promise that no provider or host ever records usage or session metadata.
- A headless permission request cannot be answered interactively. Treat that nonzero exit separately from an invalid model answer or provider failure; do not loosen permissions merely to make a transformer finish.

Keep reusable prompt/schema content separate from owner-controlled launch restrictions. Ordinary agent files do not express the complete restriction set above, and privileged tool/checkpoint controls must not be moved into lower-authority configuration for convenience.

## Native executable and managed terminal hosts

A terminal host may provide an `ego` wrapper that separates a redirected one-shot process from the terminal's managed identity. Use the host-supported executable when integrating inside that host; do not assume that replacing a wrapper with an absolute native path preserves every host behavior.

A bounded synthetic check on November 19, 2026 exercised both the installed native executable and the terminal wrapper with this recipe: both returned the requested JSON with exit 0 and empty stderr, and both rejected an invalid flag with exit 2 and empty stdout. This supports basic stdin/stdout acceptance, not universal host-identity, tool-enabled, timeout or provider equivalence. See the [assessment and probe limits](../internals/2026-11-19-one-shot-transformer-proposal.md).
