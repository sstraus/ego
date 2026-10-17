
using namespace System.Management.Automation
using namespace System.Management.Automation.Language

Register-ArgumentCompleter -Native -CommandName 'ego' -ScriptBlock {
    param($wordToComplete, $commandAst, $cursorPosition)

    $commandElements = $commandAst.CommandElements
    $command = @(
        'ego'
        for ($i = 1; $i -lt $commandElements.Count; $i++) {
            $element = $commandElements[$i]
            if ($element -isnot [StringConstantExpressionAst] -or
                $element.StringConstantType -ne [StringConstantType]::BareWord -or
                $element.Value.StartsWith('-') -or
                $element.Value -eq $wordToComplete) {
                break
        }
        $element.Value
    }) -join ';'

    $completions = @(switch ($command) {
        'ego' {
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('run', 'run', [CompletionResultType]::ParameterValue, 'Run an agent from a prompt, headless.')
            [CompletionResult]::new('resume', 'resume', [CompletionResultType]::ParameterValue, 'Continue a session; no ID = latest in this workspace.')
            [CompletionResult]::new('continue', 'continue', [CompletionResultType]::ParameterValue, 'Continue a session; no ID = latest in this workspace.')
            [CompletionResult]::new('fork', 'fork', [CompletionResultType]::ParameterValue, 'Fork a session at its tip or after the completed turn containing a message.')
            [CompletionResult]::new('rm', 'rm', [CompletionResultType]::ParameterValue, 'Delete a stored session (history is retained for descendants).')
            [CompletionResult]::new('compact', 'compact', [CompletionResultType]::ParameterValue, 'Compact a session into its deterministic successor.')
            [CompletionResult]::new('export', 'export', [CompletionResultType]::ParameterValue, 'Export a session locally or as a structurally sanitized bundle.')
            [CompletionResult]::new('ls', 'ls', [CompletionResultType]::ParameterValue, 'List sessions (this workspace; --all for every one).')
            [CompletionResult]::new('usage', 'usage', [CompletionResultType]::ParameterValue, 'Report usage, context and cost for this workspace.')
            [CompletionResult]::new('models', 'models', [CompletionResultType]::ParameterValue, 'List the shared model catalogue.')
            [CompletionResult]::new('hooks', 'hooks', [CompletionResultType]::ParameterValue, 'Inspect the admitted system and user hook catalogue.')
            [CompletionResult]::new('doctor', 'doctor', [CompletionResultType]::ParameterValue, 'Report a read-only redacted health snapshot of this installation.')
            [CompletionResult]::new('setup', 'setup', [CompletionResultType]::ParameterValue, 'Inspect expected work without running effects.')
            [CompletionResult]::new('acp', 'acp', [CompletionResultType]::ParameterValue, 'Serve the Agent Client Protocol on stdio or an endpoint.')
            [CompletionResult]::new('a2a', 'a2a', [CompletionResultType]::ParameterValue, 'Serve the Agent2Agent protocol on a local or loopback endpoint.')
            [CompletionResult]::new('auth', 'auth', [CompletionResultType]::ParameterValue, 'Manage subscription credentials.')
            [CompletionResult]::new('mcp', 'mcp', [CompletionResultType]::ParameterValue, 'Manage configured MCP servers.')
            [CompletionResult]::new('config', 'config', [CompletionResultType]::ParameterValue, 'Manage stored user configuration.')
            [CompletionResult]::new('roots', 'roots', [CompletionResultType]::ParameterValue, 'roots')
            [CompletionResult]::new('rules', 'rules', [CompletionResultType]::ParameterValue, 'Inspect and manage persistent permission rules.')
            [CompletionResult]::new('mcp-server', 'mcp-server', [CompletionResultType]::ParameterValue, 'Serve scoped ego runs over MCP on stdio.')
            [CompletionResult]::new('orchestrator', 'orchestrator', [CompletionResultType]::ParameterValue, 'orchestrator')
            [CompletionResult]::new('worktree', 'worktree', [CompletionResultType]::ParameterValue, 'Inspect or clean up a retained local worktree.')
            break
        }
        'ego;run' {
            [CompletionResult]::new('--worktree', '--worktree', [CompletionResultType]::ParameterName, 'Create a retained managed worktree on this explicitly approved new branch')
            [CompletionResult]::new('-f', '-f', [CompletionResultType]::ParameterName, 'Read the instruction and its invocation configuration from an agent file')
            [CompletionResult]::new('--file', '--file', [CompletionResultType]::ParameterName, 'Read the instruction and its invocation configuration from an agent file')
            [CompletionResult]::new('--var', '--var', [CompletionResultType]::ParameterName, 'Supply one declared agent-file template variable')
            [CompletionResult]::new('--attach', '--attach', [CompletionResultType]::ParameterName, 'Add exact image bytes as ordered context before the prompt text')
            [CompletionResult]::new('-m', '-m', [CompletionResultType]::ParameterName, 'provider/model slug, e.g. ollama/qwen3')
            [CompletionResult]::new('--model', '--model', [CompletionResultType]::ParameterName, 'provider/model slug, e.g. ollama/qwen3')
            [CompletionResult]::new('--sandbox', '--sandbox', [CompletionResultType]::ParameterName, 'Filesystem confinement: ro|workspace|adaptive|off; off = no confinement, credentials included')
            [CompletionResult]::new('--thinking', '--thinking', [CompletionResultType]::ParameterName, 'off|minimal|low|medium|high|xhigh|max')
            [CompletionResult]::new('--read-only-dir', '--read-only-dir', [CompletionResultType]::ParameterName, 'Grant reads of one existing canonical absolute upload directory')
            [CompletionResult]::new('--system', '--system', [CompletionResultType]::ParameterName, 'Replace the system prompt. `@file` reads it from a file')
            [CompletionResult]::new('--append-system', '--append-system', [CompletionResultType]::ParameterName, 'Append to the system prompt. `@file` reads it from a file')
            [CompletionResult]::new('--instructions', '--instructions', [CompletionResultType]::ParameterName, 'Use this instruction file instead of discovering one')
            [CompletionResult]::new('--tools', '--tools', [CompletionResultType]::ParameterName, 'Restrict the builtin tools to this comma-separated list')
            [CompletionResult]::new('--allow', '--allow', [CompletionResultType]::ParameterName, 'Allow rule, e.g. ''read(src/**)''. Repeatable')
            [CompletionResult]::new('--deny', '--deny', [CompletionResultType]::ParameterName, 'Deny rule, e.g. ''write(**)''. Repeatable')
            [CompletionResult]::new('--profile', '--profile', [CompletionResultType]::ParameterName, 'Layer [profile.NAME] from config')
            [CompletionResult]::new('--session', '--session', [CompletionResultType]::ParameterName, 'Named session: created or attached')
            [CompletionResult]::new('--session-id', '--session-id', [CompletionResultType]::ParameterName, 'A session id supplied by the caller')
            [CompletionResult]::new('--session-dir', '--session-dir', [CompletionResultType]::ParameterName, 'Override the store location')
            [CompletionResult]::new('-C', '-C ', [CompletionResultType]::ParameterName, 'Run as if started in DIR')
            [CompletionResult]::new('--output', '--output', [CompletionResultType]::ParameterName, 'text|json|jsonl')
            [CompletionResult]::new('--artifact-dir', '--artifact-dir', [CompletionResultType]::ParameterName, 'Override the sanitized run-artifact root for profile `ci`')
            [CompletionResult]::new('--schema', '--schema', [CompletionResultType]::ParameterName, 'JSON Schema the final answer must validate against; `@file` or inline')
            [CompletionResult]::new('--schema-retries', '--schema-retries', [CompletionResultType]::ParameterName, 'How many times to re-prompt on a schema mismatch')
            [CompletionResult]::new('--retry', '--retry', [CompletionResultType]::ParameterName, 'Additional delivery-safe attempts for each candidate')
            [CompletionResult]::new('--fallback', '--fallback', [CompletionResultType]::ParameterName, 'Ordered provider/model candidate used after a delivery-safe failure')
            [CompletionResult]::new('--budget', '--budget', [CompletionResultType]::ParameterName, 'Hard cap: ''2.5$'' or ''400k'' tokens')
            [CompletionResult]::new('--max-output-tokens', '--max-output-tokens', [CompletionResultType]::ParameterName, 'Maximum generated tokens per request')
            [CompletionResult]::new('--goal', '--goal', [CompletionResultType]::ParameterName, 'Set a checked session objective')
            [CompletionResult]::new('--context-interval', '--context-interval', [CompletionResultType]::ParameterName, 'Minimum completed turns between model-requested transitions')
            [CompletionResult]::new('--goal-turns', '--goal-turns', [CompletionResultType]::ParameterName, 'Maximum automatic goal continuation turns')
            [CompletionResult]::new('--goal-check-command', '--goal-check-command', [CompletionResultType]::ParameterName, 'Evaluator argv as a JSON string array; reads evidence JSON on stdin')
            [CompletionResult]::new('--max-turns', '--max-turns', [CompletionResultType]::ParameterName, 'Cap the turns this run may take')
            [CompletionResult]::new('--timeout', '--timeout', [CompletionResultType]::ParameterName, 'Wall clock, e.g. ''90s'' or ''10m''')
            [CompletionResult]::new('--mcp', '--mcp', [CompletionResultType]::ParameterName, 'Select an effective server by name, even one marked enabled = false, or attach a strict invocation-scoped JSON MCP server')
            [CompletionResult]::new('--mode', '--mode', [CompletionResultType]::ParameterName, 'Permission mode: plan|default|auto|edits|yolo')
            [CompletionResult]::new('-c', '-c', [CompletionResultType]::ParameterName, 'Dotted-path config override, value parsed as TOML')
            [CompletionResult]::new('--set', '--set', [CompletionResultType]::ParameterName, 'Dotted-path config override, value parsed as TOML')
            [CompletionResult]::new('--offline', '--offline', [CompletionResultType]::ParameterName, 'No non-loopback egress from ego''s own HTTP transport')
            [CompletionResult]::new('--bare', '--bare', [CompletionResultType]::ParameterName, 'No ambient discovery, even in a trusted workspace')
            [CompletionResult]::new('--no-tools', '--no-tools', [CompletionResultType]::ParameterName, 'Offer no tools at all')
            [CompletionResult]::new('--trust-workspace', '--trust-workspace', [CompletionResultType]::ParameterName, 'Trust this launch''s workspace without asking; remember the answer')
            [CompletionResult]::new('--ephemeral', '--ephemeral', [CompletionResultType]::ParameterName, 'Refused: every run uses a durable session')
            [CompletionResult]::new('--no-context-tool', '--no-context-tool', [CompletionResultType]::ParameterName, 'Disable model-requested context transitions')
            [CompletionResult]::new('--context-clear', '--context-clear', [CompletionResultType]::ParameterName, 'Permit the model to discard conversation history using clear')
            [CompletionResult]::new('--plan', '--plan', [CompletionResultType]::ParameterName, 'Shorthand for --mode plan')
            [CompletionResult]::new('--yolo', '--yolo', [CompletionResultType]::ParameterName, 'Shorthand for --mode yolo')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress progress and diagnostics on stderr; stdout is unaffected')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress progress and diagnostics on stderr; stdout is unaffected')
            [CompletionResult]::new('--no-color', '--no-color', [CompletionResultType]::ParameterName, 'Never emit ANSI colour, whatever the terminal claims to support')
            [CompletionResult]::new('--plain', '--plain', [CompletionResultType]::ParameterName, 'Append plain output without terminal styling or live retry updates')
            [CompletionResult]::new('--vim', '--vim', [CompletionResultType]::ParameterName, 'Use Vim normal/insert editing in the interactive composer')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'ego;resume' {
            [CompletionResult]::new('--answer', '--answer', [CompletionResultType]::ParameterName, 'Settle a pending question. Repeat for multiple sibling questions')
            [CompletionResult]::new('--worktree', '--worktree', [CompletionResultType]::ParameterName, 'Create a retained managed worktree on this explicitly approved new branch')
            [CompletionResult]::new('-f', '-f', [CompletionResultType]::ParameterName, 'Read the instruction and its invocation configuration from an agent file')
            [CompletionResult]::new('--file', '--file', [CompletionResultType]::ParameterName, 'Read the instruction and its invocation configuration from an agent file')
            [CompletionResult]::new('--var', '--var', [CompletionResultType]::ParameterName, 'Supply one declared agent-file template variable')
            [CompletionResult]::new('--attach', '--attach', [CompletionResultType]::ParameterName, 'Add exact image bytes as ordered context before the prompt text')
            [CompletionResult]::new('-m', '-m', [CompletionResultType]::ParameterName, 'provider/model slug, e.g. ollama/qwen3')
            [CompletionResult]::new('--model', '--model', [CompletionResultType]::ParameterName, 'provider/model slug, e.g. ollama/qwen3')
            [CompletionResult]::new('--sandbox', '--sandbox', [CompletionResultType]::ParameterName, 'Filesystem confinement: ro|workspace|adaptive|off; off = no confinement, credentials included')
            [CompletionResult]::new('--thinking', '--thinking', [CompletionResultType]::ParameterName, 'off|minimal|low|medium|high|xhigh|max')
            [CompletionResult]::new('--read-only-dir', '--read-only-dir', [CompletionResultType]::ParameterName, 'Grant reads of one existing canonical absolute upload directory')
            [CompletionResult]::new('--system', '--system', [CompletionResultType]::ParameterName, 'Replace the system prompt. `@file` reads it from a file')
            [CompletionResult]::new('--append-system', '--append-system', [CompletionResultType]::ParameterName, 'Append to the system prompt. `@file` reads it from a file')
            [CompletionResult]::new('--instructions', '--instructions', [CompletionResultType]::ParameterName, 'Use this instruction file instead of discovering one')
            [CompletionResult]::new('--tools', '--tools', [CompletionResultType]::ParameterName, 'Restrict the builtin tools to this comma-separated list')
            [CompletionResult]::new('--allow', '--allow', [CompletionResultType]::ParameterName, 'Allow rule, e.g. ''read(src/**)''. Repeatable')
            [CompletionResult]::new('--deny', '--deny', [CompletionResultType]::ParameterName, 'Deny rule, e.g. ''write(**)''. Repeatable')
            [CompletionResult]::new('--profile', '--profile', [CompletionResultType]::ParameterName, 'Layer [profile.NAME] from config')
            [CompletionResult]::new('--session', '--session', [CompletionResultType]::ParameterName, 'Named session: created or attached')
            [CompletionResult]::new('--session-id', '--session-id', [CompletionResultType]::ParameterName, 'A session id supplied by the caller')
            [CompletionResult]::new('--session-dir', '--session-dir', [CompletionResultType]::ParameterName, 'Override the store location')
            [CompletionResult]::new('-C', '-C ', [CompletionResultType]::ParameterName, 'Run as if started in DIR')
            [CompletionResult]::new('--output', '--output', [CompletionResultType]::ParameterName, 'text|json|jsonl')
            [CompletionResult]::new('--artifact-dir', '--artifact-dir', [CompletionResultType]::ParameterName, 'Override the sanitized run-artifact root for profile `ci`')
            [CompletionResult]::new('--schema', '--schema', [CompletionResultType]::ParameterName, 'JSON Schema the final answer must validate against; `@file` or inline')
            [CompletionResult]::new('--schema-retries', '--schema-retries', [CompletionResultType]::ParameterName, 'How many times to re-prompt on a schema mismatch')
            [CompletionResult]::new('--retry', '--retry', [CompletionResultType]::ParameterName, 'Additional delivery-safe attempts for each candidate')
            [CompletionResult]::new('--fallback', '--fallback', [CompletionResultType]::ParameterName, 'Ordered provider/model candidate used after a delivery-safe failure')
            [CompletionResult]::new('--budget', '--budget', [CompletionResultType]::ParameterName, 'Hard cap: ''2.5$'' or ''400k'' tokens')
            [CompletionResult]::new('--max-output-tokens', '--max-output-tokens', [CompletionResultType]::ParameterName, 'Maximum generated tokens per request')
            [CompletionResult]::new('--goal', '--goal', [CompletionResultType]::ParameterName, 'Set a checked session objective')
            [CompletionResult]::new('--context-interval', '--context-interval', [CompletionResultType]::ParameterName, 'Minimum completed turns between model-requested transitions')
            [CompletionResult]::new('--goal-turns', '--goal-turns', [CompletionResultType]::ParameterName, 'Maximum automatic goal continuation turns')
            [CompletionResult]::new('--goal-check-command', '--goal-check-command', [CompletionResultType]::ParameterName, 'Evaluator argv as a JSON string array; reads evidence JSON on stdin')
            [CompletionResult]::new('--max-turns', '--max-turns', [CompletionResultType]::ParameterName, 'Cap the turns this run may take')
            [CompletionResult]::new('--timeout', '--timeout', [CompletionResultType]::ParameterName, 'Wall clock, e.g. ''90s'' or ''10m''')
            [CompletionResult]::new('--mcp', '--mcp', [CompletionResultType]::ParameterName, 'Select an effective server by name, even one marked enabled = false, or attach a strict invocation-scoped JSON MCP server')
            [CompletionResult]::new('--mode', '--mode', [CompletionResultType]::ParameterName, 'Permission mode: plan|default|auto|edits|yolo')
            [CompletionResult]::new('-c', '-c', [CompletionResultType]::ParameterName, 'Dotted-path config override, value parsed as TOML')
            [CompletionResult]::new('--set', '--set', [CompletionResultType]::ParameterName, 'Dotted-path config override, value parsed as TOML')
            [CompletionResult]::new('--offline', '--offline', [CompletionResultType]::ParameterName, 'No non-loopback egress from ego''s own HTTP transport')
            [CompletionResult]::new('--bare', '--bare', [CompletionResultType]::ParameterName, 'No ambient discovery, even in a trusted workspace')
            [CompletionResult]::new('--no-tools', '--no-tools', [CompletionResultType]::ParameterName, 'Offer no tools at all')
            [CompletionResult]::new('--trust-workspace', '--trust-workspace', [CompletionResultType]::ParameterName, 'Trust this launch''s workspace without asking; remember the answer')
            [CompletionResult]::new('--ephemeral', '--ephemeral', [CompletionResultType]::ParameterName, 'Refused: every run uses a durable session')
            [CompletionResult]::new('--no-context-tool', '--no-context-tool', [CompletionResultType]::ParameterName, 'Disable model-requested context transitions')
            [CompletionResult]::new('--context-clear', '--context-clear', [CompletionResultType]::ParameterName, 'Permit the model to discard conversation history using clear')
            [CompletionResult]::new('--plan', '--plan', [CompletionResultType]::ParameterName, 'Shorthand for --mode plan')
            [CompletionResult]::new('--yolo', '--yolo', [CompletionResultType]::ParameterName, 'Shorthand for --mode yolo')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress progress and diagnostics on stderr; stdout is unaffected')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress progress and diagnostics on stderr; stdout is unaffected')
            [CompletionResult]::new('--no-color', '--no-color', [CompletionResultType]::ParameterName, 'Never emit ANSI colour, whatever the terminal claims to support')
            [CompletionResult]::new('--plain', '--plain', [CompletionResultType]::ParameterName, 'Append plain output without terminal styling or live retry updates')
            [CompletionResult]::new('--vim', '--vim', [CompletionResultType]::ParameterName, 'Use Vim normal/insert editing in the interactive composer')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'ego;continue' {
            [CompletionResult]::new('--answer', '--answer', [CompletionResultType]::ParameterName, 'Settle a pending question. Repeat for multiple sibling questions')
            [CompletionResult]::new('--worktree', '--worktree', [CompletionResultType]::ParameterName, 'Create a retained managed worktree on this explicitly approved new branch')
            [CompletionResult]::new('-f', '-f', [CompletionResultType]::ParameterName, 'Read the instruction and its invocation configuration from an agent file')
            [CompletionResult]::new('--file', '--file', [CompletionResultType]::ParameterName, 'Read the instruction and its invocation configuration from an agent file')
            [CompletionResult]::new('--var', '--var', [CompletionResultType]::ParameterName, 'Supply one declared agent-file template variable')
            [CompletionResult]::new('--attach', '--attach', [CompletionResultType]::ParameterName, 'Add exact image bytes as ordered context before the prompt text')
            [CompletionResult]::new('-m', '-m', [CompletionResultType]::ParameterName, 'provider/model slug, e.g. ollama/qwen3')
            [CompletionResult]::new('--model', '--model', [CompletionResultType]::ParameterName, 'provider/model slug, e.g. ollama/qwen3')
            [CompletionResult]::new('--sandbox', '--sandbox', [CompletionResultType]::ParameterName, 'Filesystem confinement: ro|workspace|adaptive|off; off = no confinement, credentials included')
            [CompletionResult]::new('--thinking', '--thinking', [CompletionResultType]::ParameterName, 'off|minimal|low|medium|high|xhigh|max')
            [CompletionResult]::new('--read-only-dir', '--read-only-dir', [CompletionResultType]::ParameterName, 'Grant reads of one existing canonical absolute upload directory')
            [CompletionResult]::new('--system', '--system', [CompletionResultType]::ParameterName, 'Replace the system prompt. `@file` reads it from a file')
            [CompletionResult]::new('--append-system', '--append-system', [CompletionResultType]::ParameterName, 'Append to the system prompt. `@file` reads it from a file')
            [CompletionResult]::new('--instructions', '--instructions', [CompletionResultType]::ParameterName, 'Use this instruction file instead of discovering one')
            [CompletionResult]::new('--tools', '--tools', [CompletionResultType]::ParameterName, 'Restrict the builtin tools to this comma-separated list')
            [CompletionResult]::new('--allow', '--allow', [CompletionResultType]::ParameterName, 'Allow rule, e.g. ''read(src/**)''. Repeatable')
            [CompletionResult]::new('--deny', '--deny', [CompletionResultType]::ParameterName, 'Deny rule, e.g. ''write(**)''. Repeatable')
            [CompletionResult]::new('--profile', '--profile', [CompletionResultType]::ParameterName, 'Layer [profile.NAME] from config')
            [CompletionResult]::new('--session', '--session', [CompletionResultType]::ParameterName, 'Named session: created or attached')
            [CompletionResult]::new('--session-id', '--session-id', [CompletionResultType]::ParameterName, 'A session id supplied by the caller')
            [CompletionResult]::new('--session-dir', '--session-dir', [CompletionResultType]::ParameterName, 'Override the store location')
            [CompletionResult]::new('-C', '-C ', [CompletionResultType]::ParameterName, 'Run as if started in DIR')
            [CompletionResult]::new('--output', '--output', [CompletionResultType]::ParameterName, 'text|json|jsonl')
            [CompletionResult]::new('--artifact-dir', '--artifact-dir', [CompletionResultType]::ParameterName, 'Override the sanitized run-artifact root for profile `ci`')
            [CompletionResult]::new('--schema', '--schema', [CompletionResultType]::ParameterName, 'JSON Schema the final answer must validate against; `@file` or inline')
            [CompletionResult]::new('--schema-retries', '--schema-retries', [CompletionResultType]::ParameterName, 'How many times to re-prompt on a schema mismatch')
            [CompletionResult]::new('--retry', '--retry', [CompletionResultType]::ParameterName, 'Additional delivery-safe attempts for each candidate')
            [CompletionResult]::new('--fallback', '--fallback', [CompletionResultType]::ParameterName, 'Ordered provider/model candidate used after a delivery-safe failure')
            [CompletionResult]::new('--budget', '--budget', [CompletionResultType]::ParameterName, 'Hard cap: ''2.5$'' or ''400k'' tokens')
            [CompletionResult]::new('--max-output-tokens', '--max-output-tokens', [CompletionResultType]::ParameterName, 'Maximum generated tokens per request')
            [CompletionResult]::new('--goal', '--goal', [CompletionResultType]::ParameterName, 'Set a checked session objective')
            [CompletionResult]::new('--context-interval', '--context-interval', [CompletionResultType]::ParameterName, 'Minimum completed turns between model-requested transitions')
            [CompletionResult]::new('--goal-turns', '--goal-turns', [CompletionResultType]::ParameterName, 'Maximum automatic goal continuation turns')
            [CompletionResult]::new('--goal-check-command', '--goal-check-command', [CompletionResultType]::ParameterName, 'Evaluator argv as a JSON string array; reads evidence JSON on stdin')
            [CompletionResult]::new('--max-turns', '--max-turns', [CompletionResultType]::ParameterName, 'Cap the turns this run may take')
            [CompletionResult]::new('--timeout', '--timeout', [CompletionResultType]::ParameterName, 'Wall clock, e.g. ''90s'' or ''10m''')
            [CompletionResult]::new('--mcp', '--mcp', [CompletionResultType]::ParameterName, 'Select an effective server by name, even one marked enabled = false, or attach a strict invocation-scoped JSON MCP server')
            [CompletionResult]::new('--mode', '--mode', [CompletionResultType]::ParameterName, 'Permission mode: plan|default|auto|edits|yolo')
            [CompletionResult]::new('-c', '-c', [CompletionResultType]::ParameterName, 'Dotted-path config override, value parsed as TOML')
            [CompletionResult]::new('--set', '--set', [CompletionResultType]::ParameterName, 'Dotted-path config override, value parsed as TOML')
            [CompletionResult]::new('--offline', '--offline', [CompletionResultType]::ParameterName, 'No non-loopback egress from ego''s own HTTP transport')
            [CompletionResult]::new('--bare', '--bare', [CompletionResultType]::ParameterName, 'No ambient discovery, even in a trusted workspace')
            [CompletionResult]::new('--no-tools', '--no-tools', [CompletionResultType]::ParameterName, 'Offer no tools at all')
            [CompletionResult]::new('--trust-workspace', '--trust-workspace', [CompletionResultType]::ParameterName, 'Trust this launch''s workspace without asking; remember the answer')
            [CompletionResult]::new('--ephemeral', '--ephemeral', [CompletionResultType]::ParameterName, 'Refused: every run uses a durable session')
            [CompletionResult]::new('--no-context-tool', '--no-context-tool', [CompletionResultType]::ParameterName, 'Disable model-requested context transitions')
            [CompletionResult]::new('--context-clear', '--context-clear', [CompletionResultType]::ParameterName, 'Permit the model to discard conversation history using clear')
            [CompletionResult]::new('--plan', '--plan', [CompletionResultType]::ParameterName, 'Shorthand for --mode plan')
            [CompletionResult]::new('--yolo', '--yolo', [CompletionResultType]::ParameterName, 'Shorthand for --mode yolo')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress progress and diagnostics on stderr; stdout is unaffected')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress progress and diagnostics on stderr; stdout is unaffected')
            [CompletionResult]::new('--no-color', '--no-color', [CompletionResultType]::ParameterName, 'Never emit ANSI colour, whatever the terminal claims to support')
            [CompletionResult]::new('--plain', '--plain', [CompletionResultType]::ParameterName, 'Append plain output without terminal styling or live retry updates')
            [CompletionResult]::new('--vim', '--vim', [CompletionResultType]::ParameterName, 'Use Vim normal/insert editing in the interactive composer')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'ego;fork' {
            [CompletionResult]::new('--at', '--at', [CompletionResultType]::ParameterName, 'at')
            [CompletionResult]::new('--session-dir', '--session-dir', [CompletionResultType]::ParameterName, 'Override the store location')
            [CompletionResult]::new('-C', '-C ', [CompletionResultType]::ParameterName, 'Run as if started in DIR')
            [CompletionResult]::new('--output', '--output', [CompletionResultType]::ParameterName, 'text|json|jsonl')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'ego;rm' {
            [CompletionResult]::new('--session-dir', '--session-dir', [CompletionResultType]::ParameterName, 'Override the store location')
            [CompletionResult]::new('-C', '-C ', [CompletionResultType]::ParameterName, 'Run as if started in DIR')
            [CompletionResult]::new('--yes', '--yes', [CompletionResultType]::ParameterName, 'yes')
            [CompletionResult]::new('--force', '--force', [CompletionResultType]::ParameterName, 'force')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'ego;compact' {
            [CompletionResult]::new('--budget', '--budget', [CompletionResultType]::ParameterName, 'Hard spend ceiling, as for run')
            [CompletionResult]::new('--max-output-tokens', '--max-output-tokens', [CompletionResultType]::ParameterName, 'Maximum generated tokens per request')
            [CompletionResult]::new('--request-id', '--request-id', [CompletionResultType]::ParameterName, 'request-id')
            [CompletionResult]::new('--session-dir', '--session-dir', [CompletionResultType]::ParameterName, 'Override the store location')
            [CompletionResult]::new('-C', '-C ', [CompletionResultType]::ParameterName, 'Run as if started in DIR')
            [CompletionResult]::new('--output', '--output', [CompletionResultType]::ParameterName, 'text|json|jsonl')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'ego;export' {
            [CompletionResult]::new('--format', '--format', [CompletionResultType]::ParameterName, 'format')
            [CompletionResult]::new('--after', '--after', [CompletionResultType]::ParameterName, 'Only emit transcript events after this session-local sequence')
            [CompletionResult]::new('--file', '--file', [CompletionResultType]::ParameterName, 'file')
            [CompletionResult]::new('--session-dir', '--session-dir', [CompletionResultType]::ParameterName, 'Override the store location')
            [CompletionResult]::new('-C', '-C ', [CompletionResultType]::ParameterName, 'Run as if started in DIR')
            [CompletionResult]::new('--sanitize', '--sanitize', [CompletionResultType]::ParameterName, 'sanitize')
            [CompletionResult]::new('--follow', '--follow', [CompletionResultType]::ParameterName, 'Follow committed local transcript events until stopped')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'ego;ls' {
            [CompletionResult]::new('--session-dir', '--session-dir', [CompletionResultType]::ParameterName, 'Override the store location')
            [CompletionResult]::new('-C', '-C ', [CompletionResultType]::ParameterName, 'Run as if started in DIR')
            [CompletionResult]::new('--output', '--output', [CompletionResultType]::ParameterName, 'text|json|jsonl')
            [CompletionResult]::new('--all', '--all', [CompletionResultType]::ParameterName, 'Every session in the store, not just this workspace''s')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'ego;usage' {
            [CompletionResult]::new('--period', '--period', [CompletionResultType]::ParameterName, 'all|Ns|Nm|Nh')
            [CompletionResult]::new('--session-dir', '--session-dir', [CompletionResultType]::ParameterName, 'Override the store location')
            [CompletionResult]::new('-C', '-C ', [CompletionResultType]::ParameterName, 'Run as if started in DIR')
            [CompletionResult]::new('--json', '--json', [CompletionResultType]::ParameterName, 'Emit the exact versioned usage-report object')
            [CompletionResult]::new('--all-roles', '--all-roles', [CompletionResultType]::ParameterName, 'Keep role rows that recorded no activity in the text table')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'ego;models' {
            [CompletionResult]::new('--provider', '--provider', [CompletionResultType]::ParameterName, 'Restrict enumeration and output to one admitted provider')
            [CompletionResult]::new('--refresh', '--refresh', [CompletionResultType]::ParameterName, 'Refresh the selected discoverable provider slices')
            [CompletionResult]::new('--json', '--json', [CompletionResultType]::ParameterName, 'Emit the exact versioned snapshot object')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'ego;hooks' {
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('ls', 'ls', [CompletionResultType]::ParameterValue, 'List the admitted hook catalogue without changing configuration')
            break
        }
        'ego;hooks;ls' {
            [CompletionResult]::new('--json', '--json', [CompletionResultType]::ParameterName, 'Emit the exact versioned hook-listing object')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'ego;doctor' {
            [CompletionResult]::new('--json', '--json', [CompletionResultType]::ParameterName, 'Emit the exact versioned health snapshot')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'ego;setup' {
            [CompletionResult]::new('--items', '--items', [CompletionResultType]::ParameterName, 'Limit proposals to these report item IDs (comma-separated)')
            [CompletionResult]::new('--judge', '--judge', [CompletionResultType]::ParameterName, 'Offer a validated judge for unresolved policy; never authorizes an action')
            [CompletionResult]::new('--profile', '--profile', [CompletionResultType]::ParameterName, 'profile')
            [CompletionResult]::new('--directory', '--directory', [CompletionResultType]::ParameterName, 'directory')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'ego;acp' {
            [CompletionResult]::new('--session-dir', '--session-dir', [CompletionResultType]::ParameterName, 'Override the store location')
            [CompletionResult]::new('-C', '-C ', [CompletionResultType]::ParameterName, 'Run as if started in DIR')
            [CompletionResult]::new('--sandbox', '--sandbox', [CompletionResultType]::ParameterName, 'The weakest confinement a session may select: ro|workspace|adaptive|off; off = no confinement, credentials included')
            [CompletionResult]::new('--read-only-dir', '--read-only-dir', [CompletionResultType]::ParameterName, 'Grant reads of one existing canonical absolute upload directory')
            [CompletionResult]::new('--system', '--system', [CompletionResultType]::ParameterName, 'Replace the system prompt. `@file` reads it from a file')
            [CompletionResult]::new('--append-system', '--append-system', [CompletionResultType]::ParameterName, 'Append to the system prompt. `@file` reads it from a file')
            [CompletionResult]::new('--instructions', '--instructions', [CompletionResultType]::ParameterName, 'Use this instruction file instead of discovering one')
            [CompletionResult]::new('--tools', '--tools', [CompletionResultType]::ParameterName, 'Restrict the builtin tools to this comma-separated list')
            [CompletionResult]::new('--allow', '--allow', [CompletionResultType]::ParameterName, 'Allow rule, e.g. ''read(src/**)''. Repeatable')
            [CompletionResult]::new('--deny', '--deny', [CompletionResultType]::ParameterName, 'Deny rule, e.g. ''write(**)''. Repeatable')
            [CompletionResult]::new('--profile', '--profile', [CompletionResultType]::ParameterName, 'Layer [profile.NAME] from config')
            [CompletionResult]::new('--listen', '--listen', [CompletionResultType]::ParameterName, 'Serve on a socket path or address instead of stdio')
            [CompletionResult]::new('--max-connections', '--max-connections', [CompletionResultType]::ParameterName, 'Maximum simultaneous listener connections')
            [CompletionResult]::new('--connection-lifetime', '--connection-lifetime', [CompletionResultType]::ParameterName, 'Maximum lifetime of one listener connection: 90s, 10m or 2h')
            [CompletionResult]::new('--max-ingress-bytes', '--max-ingress-bytes', [CompletionResultType]::ParameterName, 'Maximum cumulative bytes read from one listener connection')
            [CompletionResult]::new('--bare', '--bare', [CompletionResultType]::ParameterName, 'No ambient discovery, even in a trusted workspace')
            [CompletionResult]::new('--no-tools', '--no-tools', [CompletionResultType]::ParameterName, 'Offer no tools at all')
            [CompletionResult]::new('--allow-public-bind', '--allow-public-bind', [CompletionResultType]::ParameterName, 'Serve on an address reachable from outside this machine')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'ego;a2a' {
            [CompletionResult]::new('--session-dir', '--session-dir', [CompletionResultType]::ParameterName, 'Override the store location')
            [CompletionResult]::new('-C', '-C ', [CompletionResultType]::ParameterName, 'Run as if started in DIR')
            [CompletionResult]::new('--sandbox', '--sandbox', [CompletionResultType]::ParameterName, 'The weakest confinement a session may select: ro|workspace|adaptive|off; off = no confinement, credentials included')
            [CompletionResult]::new('--listen', '--listen', [CompletionResultType]::ParameterName, 'Where to serve: a socket path, or an address such as `127.0.0.1:8080`')
            [CompletionResult]::new('--allow-public-bind', '--allow-public-bind', [CompletionResultType]::ParameterName, 'Serve on an address reachable from outside this machine')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help (see more with ''--help'')')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help (see more with ''--help'')')
            break
        }
        'ego;auth' {
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('login', 'login', [CompletionResultType]::ParameterValue, 'Authenticate and store a provider credential')
            [CompletionResult]::new('logout', 'logout', [CompletionResultType]::ParameterValue, 'Remove a stored provider credential')
            [CompletionResult]::new('status', 'status', [CompletionResultType]::ParameterValue, 'Show credential type and expiry, never token values')
            break
        }
        'ego;auth;login' {
            [CompletionResult]::new('--device', '--device', [CompletionResultType]::ParameterName, 'Prefer device-code login when the provider offers it')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'ego;auth;logout' {
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'ego;auth;status' {
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'ego;mcp' {
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('add', 'add', [CompletionResultType]::ParameterValue, 'Add or replace one complete server entry')
            [CompletionResult]::new('rm', 'rm', [CompletionResultType]::ParameterValue, 'Remove, mask, or reveal one configured server entry')
            [CompletionResult]::new('ls', 'ls', [CompletionResultType]::ParameterValue, 'List configured and admitted MCP state')
            [CompletionResult]::new('login', 'login', [CompletionResultType]::ParameterValue, 'Authorize a public OAuth client with an explicit registration strategy')
            [CompletionResult]::new('reload', 'reload', [CompletionResultType]::ParameterValue, 'Reconcile every configured server, or one named server')
            break
        }
        'ego;mcp;add' {
            [CompletionResult]::new('--http', '--http', [CompletionResultType]::ParameterName, 'Configure a Streamable HTTP transport')
            [CompletionResult]::new('--cwd', '--cwd', [CompletionResultType]::ParameterName, 'Working directory for the stdio child')
            [CompletionResult]::new('--inherit-env', '--inherit-env', [CompletionResultType]::ParameterName, 'Environment variable inherited by the stdio child. Repeatable')
            [CompletionResult]::new('--profile', '--profile', [CompletionResultType]::ParameterName, 'Explicit HTTP wire profile; never selected by fallback')
            [CompletionResult]::new('--auth', '--auth', [CompletionResultType]::ParameterName, 'HTTP authentication mode')
            [CompletionResult]::new('--stdio', '--stdio', [CompletionResultType]::ParameterName, 'Configure a subprocess transport. Its command follows literal `--`')
            [CompletionResult]::new('--allow-insecure-loopback', '--allow-insecure-loopback', [CompletionResultType]::ParameterName, 'Consent to plaintext HTTP for an exact loopback host')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'ego;mcp;rm' {
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'ego;mcp;ls' {
            [CompletionResult]::new('--json', '--json', [CompletionResultType]::ParameterName, 'Emit the typed machine-readable projection')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'ego;mcp;login' {
            [CompletionResult]::new('--client-id', '--client-id', [CompletionResultType]::ParameterName, 'Operator-issued public client ID')
            [CompletionResult]::new('--cimd', '--cimd', [CompletionResultType]::ParameterName, 'Published HTTPS client metadata document URL')
            [CompletionResult]::new('--callback-port', '--callback-port', [CompletionResultType]::ParameterName, 'Loopback port registered with the authorization server (0 selects a free port)')
            [CompletionResult]::new('--issuer', '--issuer', [CompletionResultType]::ParameterName, 'issuer')
            [CompletionResult]::new('--account', '--account', [CompletionResultType]::ParameterName, 'account')
            [CompletionResult]::new('--dcr', '--dcr', [CompletionResultType]::ParameterName, 'Explicitly use issuer-advertised dynamic registration')
            [CompletionResult]::new('--refresh', '--refresh', [CompletionResultType]::ParameterName, 'Refresh an existing issuer/resource/account-bound credential without interaction')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'ego;mcp;reload' {
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'ego;config' {
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('capabilities', 'capabilities', [CompletionResultType]::ParameterValue, 'Inspect managed owner transaction capabilities as JSON')
            [CompletionResult]::new('snapshot', 'snapshot', [CompletionResultType]::ParameterValue, 'Read stored values and their opaque owner revision as JSON')
            [CompletionResult]::new('batch', 'batch', [CompletionResultType]::ParameterValue, 'Atomically publish a bounded set of typed managed edits')
            [CompletionResult]::new('get', 'get', [CompletionResultType]::ParameterValue, 'Read one exact stored value')
            [CompletionResult]::new('set', 'set', [CompletionResultType]::ParameterValue, 'Store one strictly typed TOML value')
            [CompletionResult]::new('unset', 'unset', [CompletionResultType]::ParameterValue, 'Remove one exact stored value')
            [CompletionResult]::new('ls', 'ls', [CompletionResultType]::ParameterValue, 'List the exact stored managed projection')
            [CompletionResult]::new('edit', 'edit', [CompletionResultType]::ParameterValue, 'Edit the complete user document in the selected external editor')
            break
        }
        'ego;config;capabilities' {
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'ego;config;snapshot' {
            [CompletionResult]::new('--profile', '--profile', [CompletionResultType]::ParameterName, 'profile')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'ego;config;batch' {
            [CompletionResult]::new('--profile', '--profile', [CompletionResultType]::ParameterName, 'profile')
            [CompletionResult]::new('--set', '--set', [CompletionResultType]::ParameterName, 'set')
            [CompletionResult]::new('--if-revision', '--if-revision', [CompletionResultType]::ParameterName, 'if-revision')
            [CompletionResult]::new('--unset', '--unset', [CompletionResultType]::ParameterName, 'unset')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'ego;config;get' {
            [CompletionResult]::new('--profile', '--profile', [CompletionResultType]::ParameterName, 'profile')
            [CompletionResult]::new('--json', '--json', [CompletionResultType]::ParameterName, 'json')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'ego;config;set' {
            [CompletionResult]::new('--profile', '--profile', [CompletionResultType]::ParameterName, 'profile')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'ego;config;unset' {
            [CompletionResult]::new('--profile', '--profile', [CompletionResultType]::ParameterName, 'profile')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'ego;config;ls' {
            [CompletionResult]::new('--profile', '--profile', [CompletionResultType]::ParameterName, 'profile')
            [CompletionResult]::new('--json', '--json', [CompletionResultType]::ParameterName, 'json')
            [CompletionResult]::new('--effective', '--effective', [CompletionResultType]::ParameterName, 'Show the effective run perimeter instead of stored values')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'ego;config;edit' {
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'ego;roots' {
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('add', 'add', [CompletionResultType]::ParameterValue, 'Add or update one user perimeter root')
            [CompletionResult]::new('rm', 'rm', [CompletionResultType]::ParameterValue, 'Remove a root by its stored spelling')
            [CompletionResult]::new('ls', 'ls', [CompletionResultType]::ParameterValue, 'List roots stored in the selected user target')
            break
        }
        'ego;roots;add' {
            [CompletionResult]::new('--profile', '--profile', [CompletionResultType]::ParameterName, 'profile')
            [CompletionResult]::new('--access', '--access', [CompletionResultType]::ParameterName, 'access')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'ego;roots;rm' {
            [CompletionResult]::new('--profile', '--profile', [CompletionResultType]::ParameterName, 'profile')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'ego;roots;ls' {
            [CompletionResult]::new('--profile', '--profile', [CompletionResultType]::ParameterName, 'profile')
            [CompletionResult]::new('--json', '--json', [CompletionResultType]::ParameterName, 'json')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'ego;rules' {
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('ls', 'ls', [CompletionResultType]::ParameterValue, 'List the effective system and selected user rules')
            [CompletionResult]::new('allow', 'allow', [CompletionResultType]::ParameterValue, 'Persist one allow predicate')
            [CompletionResult]::new('deny', 'deny', [CompletionResultType]::ParameterValue, 'Persist one deny predicate')
            [CompletionResult]::new('revoke', 'revoke', [CompletionResultType]::ParameterValue, 'Remove one predicate ID from both selected user lists')
            [CompletionResult]::new('remember', 'remember', [CompletionResultType]::ParameterValue, 'Derive persistent rules from one prepared builtin call')
            [CompletionResult]::new('explain', 'explain', [CompletionResultType]::ParameterValue, 'Explain one prepared builtin call without running it')
            break
        }
        'ego;rules;ls' {
            [CompletionResult]::new('--profile', '--profile', [CompletionResultType]::ParameterName, 'profile')
            [CompletionResult]::new('--json', '--json', [CompletionResultType]::ParameterName, 'json')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'ego;rules;allow' {
            [CompletionResult]::new('--profile', '--profile', [CompletionResultType]::ParameterName, 'profile')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'ego;rules;deny' {
            [CompletionResult]::new('--profile', '--profile', [CompletionResultType]::ParameterName, 'profile')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'ego;rules;revoke' {
            [CompletionResult]::new('--profile', '--profile', [CompletionResultType]::ParameterName, 'profile')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'ego;rules;remember' {
            [CompletionResult]::new('--profile', '--profile', [CompletionResultType]::ParameterName, 'profile')
            [CompletionResult]::new('-C', '-C ', [CompletionResultType]::ParameterName, 'C')
            [CompletionResult]::new('--yes', '--yes', [CompletionResultType]::ParameterName, 'yes')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'ego;rules;explain' {
            [CompletionResult]::new('--profile', '--profile', [CompletionResultType]::ParameterName, 'profile')
            [CompletionResult]::new('-C', '-C ', [CompletionResultType]::ParameterName, 'C')
            [CompletionResult]::new('--json', '--json', [CompletionResultType]::ParameterName, 'json')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'ego;mcp-server' {
            [CompletionResult]::new('--session-dir', '--session-dir', [CompletionResultType]::ParameterName, 'Override the store location')
            [CompletionResult]::new('-C', '-C ', [CompletionResultType]::ParameterName, 'Run as if started in DIR')
            [CompletionResult]::new('--max-concurrent-runs', '--max-concurrent-runs', [CompletionResultType]::ParameterName, 'max-concurrent-runs')
            [CompletionResult]::new('--retained-requests', '--retained-requests', [CompletionResultType]::ParameterName, 'retained-requests')
            [CompletionResult]::new('--max-resource-bytes', '--max-resource-bytes', [CompletionResultType]::ParameterName, 'max-resource-bytes')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'ego;orchestrator' {
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('brief', 'brief', [CompletionResultType]::ParameterValue, 'Inspect tracked children and unread results without model generation or inbox reads')
            break
        }
        'ego;orchestrator;brief' {
            [CompletionResult]::new('--root', '--root', [CompletionResultType]::ParameterName, 'root')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'ego;worktree' {
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('show', 'show', [CompletionResultType]::ParameterValue, 'Inspect a durable local worktree receipt without starting a model')
            [CompletionResult]::new('cleanup', 'cleanup', [CompletionResultType]::ParameterValue, 'Remove only a clean worktree whose commits are integrated; retain its branch')
            break
        }
        'ego;worktree;show' {
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'ego;worktree;cleanup' {
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
    })

    $completions.Where{ $_.CompletionText -like "$wordToComplete*" } |
        Sort-Object -Property ListItemText
}
