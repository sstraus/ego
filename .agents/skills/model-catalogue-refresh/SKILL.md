---
name: model-catalogue-refresh
description: Refresh ego's builtin hosted-model catalogue from the models.dev snapshot, review the per-provider diff, regenerate the rows and run the catalogue tests. Use when a provider releases a model or when the builtin catalogue looks stale. Stops for review; never commits or pushes.
---

# Model catalogue refresh

The builtin rows of `openai`, `openai-codex`, `anthropic`, `gemini`, `github-copilot`,
`xai-oauth`, `minimax-oauth`, `bedrock`, `vertex` and `deepseek` are generated. Never edit `models.generated.toml`.

Files, all under `crates/ego-cli/catalogue/`:

- `upstream/models.dev.json`: pinned, filtered snapshot of <https://models.dev/api.json>
  (commit of `sst/models.dev@dev`, read date). It carries no `cost` field: models.dev
  states a literal 0 for the coding plans, and ego writes no price it cannot verify.
- `policy.toml`: reviewed rules. Per provider: `floor` (release date of the first
  model of the current generation), `include`, `exclude` (with reasons), `drop_efforts`,
  `default_effort`, and the `wire` facts the snapshot does not carry.
- `models.generated.toml`: the output, one row per current model with a provenance line.
- `models.toml`: startup defaults and tombstones (hand-kept).

Rule (Boss, 2026-10-11): list only current model generations. OpenRouter has no builtin rows.

## Steps

1. `S=scripts/catalogue.py`
2. `cp crates/ego-cli/catalogue/upstream/models.dev.json ~/Gits/.tmp/models.dev.old.json`
3. `python3 $S fetch` (network; writes the snapshot, sets revision and read date).
4. `python3 $S diff ~/Gits/.tmp/models.dev.old.json crates/ego-cli/catalogue/upstream/models.dev.json`
   and read it per provider: new, removed, newly deprecated, changed.
5. Decide with the diff, in `policy.toml`:
   - A new generation: raise the provider `floor`; a row that falls below it leaves
     the catalogue, so add a `[retired."provider/id"]` tombstone with its
     replacement to `models.toml`.
   - A new model that must not be listed: add it to `exclude` with the reason.
   - `generate` stops on a model whose reasoning controls are not an effort list.
6. `python3 $S generate`, then `python3 $S check`.
7. Update `BUILTIN_CATALOGUE_REVISION` in `crates/ego-cli/src/facts.rs` and the
   `# revision =` line in `models.toml` to the read date.
8. Run the catalogue tests (through the mbx shim): `cargo nextest run -p ego-cli --lib -E 'test(facts::)'`.
   They fail when a current snapshot model is missing, a stale row remains, a price
   enters, or a generated row loses its provenance.
9. Stop. Show the diff to Boss. Do not commit or push.

Vendor-stated current aliases absent upstream live in policy `stated` tables.
They contain model limits, modalities, explicit wire facts, a vendor `source` URL
and `read` date. Unknown reasoning controls use `reasoning_efforts = "unknown"`.
The generator stops when such an alias enters the snapshot: reconcile the stated
row with upstream before regenerating. Never copy an API alias to a subscription
provider without evidence that its backend serves it.
