# Live conformance 20261022T230209Z-04d77b7e

Runner commit: `eab45143e03d47b7ccec0e5619db692bebe230ba`.
Accounted cost: USD 0.007182790; unknown cost: True.

| Model | Suite / case | Status | Later cache share |
| --- | --- | --- | --- |
| openai-codex/gpt-6-luna | cache/fork-3 | pass | n/a |
| openai/gpt-6-luna | cache/multi-turn-tool-resume-1 | pass | 0.958210236181485 |
| openai/gpt-6-luna | cache/fork-1 | pass | n/a |
| openai/gpt-6-luna | cache/multi-turn-tool-resume-2 | pass | 0.958210236181485 |
| openai/gpt-6-luna | cache/fork-2 | pass | n/a |
| openai/gpt-6-luna | cache/multi-turn-tool-resume-3 | pass | 0.9579540029303788 |
| openai/gpt-6-luna | cache/fork-3 | pass | n/a |
| anthropic/claude-sonnet-5-5 | cache/prerequisite | blocked | n/a |
| openrouter/anthropic/claude-sonnet-5.5 | cache/multi-turn-tool-resume-1 | blocked | n/a |
| openrouter/anthropic/claude-sonnet-5.5 | cache/multi-turn-tool-resume-2 | blocked | n/a |
| openrouter/anthropic/claude-sonnet-5.5 | cache/multi-turn-tool-resume-3 | blocked | n/a |
| openrouter/openai/gpt-6-luna | cache/multi-turn-tool-resume-1 | blocked | n/a |
| openrouter/openai/gpt-6-luna | cache/multi-turn-tool-resume-2 | blocked | n/a |
| openrouter/openai/gpt-6-luna | cache/multi-turn-tool-resume-3 | blocked | n/a |
| openai-codex/gpt-6-luna | skills/description | pass | 0.7675276752767528 |
| openai-codex/gpt-6-luna | skills/model | pass | 0.7710843373493976 |
| openai-codex/gpt-6-luna | skills/tools | pass | 0.3530659876936134 |
| openai-codex/gpt-6-luna | skills/arguments | pass | 0.0 |
| openai/gpt-6-luna | skills/description | pass | 0.8451982799808887 |
| openai/gpt-6-luna | skills/model | pass | 0.0 |
| openai/gpt-6-luna | skills/tools | pass | 0.8509138956177054 |
| openai/gpt-6-luna | skills/arguments | pass | 0.8259449370041997 |
| anthropic/claude-sonnet-5-5 | skills/prerequisite | blocked | n/a |
| openrouter/anthropic/claude-sonnet-5.5 | skills/description | blocked | n/a |
| openrouter/anthropic/claude-sonnet-5.5 | skills/model | blocked | n/a |
| openrouter/anthropic/claude-sonnet-5.5 | skills/tools | blocked | n/a |
| openrouter/anthropic/claude-sonnet-5.5 | skills/arguments | blocked | n/a |
| openrouter/openai/gpt-6-luna | skills/description | blocked | n/a |
| openrouter/openai/gpt-6-luna | skills/model | blocked | n/a |
| openrouter/openai/gpt-6-luna | skills/tools | blocked | n/a |
| openrouter/openai/gpt-6-luna | skills/arguments | blocked | n/a |
| openai-codex/gpt-6-luna | compaction/checkpoint-continuation | pass | 0.44872918492550395 |
| openai/gpt-6-luna | compaction/checkpoint-continuation | pass | 0.8907944235450321 |
| anthropic/claude-sonnet-5-5 | compaction/prerequisite | blocked | n/a |
| openrouter/anthropic/claude-sonnet-5.5 | compaction/checkpoint-continuation | blocked | n/a |
| openrouter/openai/gpt-6-luna | compaction/checkpoint-continuation | blocked | n/a |
| openai-codex/gpt-6-luna | recovery/fault-proxy | not_applicable | n/a |
| openai/gpt-6-luna | recovery/before | fail | n/a |
| openai/gpt-6-luna | recovery/prerequisite | stopped | n/a |
| anthropic/claude-sonnet-5-5 | recovery/prerequisite | blocked | n/a |
| openrouter/anthropic/claude-sonnet-5.5 | recovery/fault-proxy | not_applicable | n/a |
| openrouter/openai/gpt-6-luna | recovery/fault-proxy | not_applicable | n/a |

Previous matching matrix: 20261021T111934Z-b369bb25.
Accounted cost change: USD -0.000283100.
- openai/gpt-6-luna / cache / multi-turn-tool-resume-1: cache share change +0.0030.
- openai/gpt-6-luna / cache / multi-turn-tool-resume-2: cache share change -0.0012.
- openai/gpt-6-luna / cache / multi-turn-tool-resume-3: cache share change -0.0012.
- openai-codex/gpt-6-luna / skills / description: cache share change +0.7675.
- openai-codex/gpt-6-luna / skills / model: cache share change +0.7711.
- openai-codex/gpt-6-luna / skills / tools: cache share change -0.0007.
- openai-codex/gpt-6-luna / skills / arguments: cache share change -0.7546.
- openai/gpt-6-luna / skills / description: cache share change -0.0026.
- openai/gpt-6-luna / skills / model: cache share change +0.0000.
- openai/gpt-6-luna / skills / tools: cache share change -0.0024.
- openai/gpt-6-luna / skills / arguments: cache share change -0.0028.
- openai-codex/gpt-6-luna / compaction / checkpoint-continuation: cache share change +0.0189.
- openai/gpt-6-luna / compaction / checkpoint-continuation: cache share change +0.0062.
