
use builtin;
use str;

set edit:completion:arg-completer[ego] = {|@words|
    fn spaces {|n|
        builtin:repeat $n ' ' | str:join ''
    }
    fn cand {|text desc|
        edit:complex-candidate $text &display=$text' '(spaces (- 14 (wcswidth $text)))$desc
    }
    var command = 'ego'
    for word $words[1..-1] {
        if (str:has-prefix $word '-') {
            break
        }
        set command = $command';'$word
    }
    var completions = [
        &'ego'= {
            cand --worktree 'Create a retained managed worktree on this explicitly approved new branch'
            cand -f 'Read the instruction and its invocation configuration from an agent file'
            cand --file 'Read the instruction and its invocation configuration from an agent file'
            cand --var 'Supply one declared agent-file template variable'
            cand --attach 'Add exact image bytes as ordered context before the prompt text'
            cand -m 'provider/model slug, e.g. ollama/qwen3'
            cand --model 'provider/model slug, e.g. ollama/qwen3'
            cand --sandbox 'Filesystem confinement: ro|workspace|adaptive|off; off = no confinement, credentials included'
            cand --thinking 'off|minimal|low|medium|high|xhigh|max'
            cand --read-only-dir 'Grant reads of one existing canonical absolute upload directory'
            cand --system 'Replace the system prompt. `@file` reads it from a file'
            cand --append-system 'Append to the system prompt. `@file` reads it from a file'
            cand --output-style 'Select a trusted prompt-only style: concise, explanatory, or NAME from EGO_HOME/output-styles/NAME.md'
            cand --instructions 'Use this instruction file instead of discovering one'
            cand --tools 'Restrict the builtin tools to this comma-separated list'
            cand --allow 'Allow rule, e.g. ''read(src/**)''. Repeatable'
            cand --deny 'Deny rule, e.g. ''write(**)''. Repeatable'
            cand --profile 'Layer [profile.NAME] from config'
            cand --session 'Named session: created or attached'
            cand --session-id 'A session id supplied by the caller'
            cand --session-dir 'Override the store location'
            cand -C 'Run as if started in DIR'
            cand --output 'text|json|jsonl'
            cand --artifact-dir 'Override the sanitized run-artifact root for profile `ci`'
            cand --schema 'JSON Schema the final answer must validate against; `@file` or inline'
            cand --schema-retries 'How many times to re-prompt on a schema mismatch'
            cand --retry 'Additional delivery-safe attempts for each candidate'
            cand --fallback 'Ordered provider/model candidate used after a delivery-safe failure'
            cand --budget 'Hard cap: ''2.5$'' or ''400k'' tokens'
            cand --max-output-tokens 'Maximum generated tokens per request'
            cand --goal 'Set a checked session objective'
            cand --context-interval 'Minimum completed turns between model-requested transitions'
            cand --goal-turns 'Maximum automatic goal continuation turns'
            cand --goal-check-command 'Evaluator argv as a JSON string array; reads evidence JSON on stdin'
            cand --max-turns 'Cap the turns this run may take'
            cand --timeout 'Wall clock, e.g. ''90s'' or ''10m'''
            cand --mcp 'Select an effective server by name, even one marked enabled = false, or attach a strict invocation-scoped JSON MCP server'
            cand --mode 'Permission mode: plan|default|auto|edits|yolo'
            cand -c 'Dotted-path config override, value parsed as TOML'
            cand --set 'Dotted-path config override, value parsed as TOML'
            cand --offline 'No non-loopback egress from ego''s own HTTP transport'
            cand --bare 'No ambient discovery, even in a trusted workspace'
            cand --no-tools 'Offer no tools at all'
            cand --trust-workspace 'Trust this launch''s workspace without asking; remember the answer'
            cand --ephemeral 'Refused: every run uses a durable session'
            cand --no-context-tool 'Disable model-requested context transitions'
            cand --context-clear 'Permit the model to discard conversation history using clear'
            cand --plan 'Shorthand for --mode plan'
            cand --yolo 'Shorthand for --mode yolo'
            cand -q 'Suppress progress and diagnostics on stderr; stdout is unaffected'
            cand --quiet 'Suppress progress and diagnostics on stderr; stdout is unaffected'
            cand --no-color 'Never emit ANSI colour, whatever the terminal claims to support'
            cand --plain 'Append plain output without terminal styling or live retry updates'
            cand --vim 'Use Vim normal/insert editing in the interactive composer'
            cand -h 'Print help'
            cand --help 'Print help'
            cand -V 'Print version'
            cand --version 'Print version'
            cand run 'Run an agent from a prompt, headless.'
            cand resume 'Continue a session; no ID = latest in this workspace.'
            cand continue 'Continue a session; no ID = latest in this workspace.'
            cand fork 'Fork a session at its tip or after the completed turn containing a message.'
            cand rm 'Delete a stored session (history is retained for descendants).'
            cand compact 'Compact a session into its deterministic successor.'
            cand export 'Export a session locally or as a structurally sanitized bundle.'
            cand ls 'List sessions (this workspace; --all for every one).'
            cand usage 'Report usage, context and cost for this workspace.'
            cand models 'List the shared model catalogue.'
            cand hooks 'Inspect the admitted system and user hook catalogue.'
            cand doctor 'Report a read-only redacted health snapshot of this installation.'
            cand setup 'Inspect expected work without running effects.'
            cand acp 'Serve the Agent Client Protocol on stdio or an endpoint.'
            cand a2a 'Serve the Agent2Agent protocol on a local or loopback endpoint.'
            cand auth 'Manage subscription credentials.'
            cand mcp 'Manage configured MCP servers.'
            cand config 'Manage stored user configuration.'
            cand roots 'roots'
            cand rules 'Inspect and manage persistent permission rules.'
            cand mcp-server 'Serve scoped ego runs over MCP on stdio.'
            cand orchestrator 'orchestrator'
            cand worktree 'Inspect or clean up a retained local worktree.'
        }
        &'ego;run'= {
            cand --worktree 'Create a retained managed worktree on this explicitly approved new branch'
            cand -f 'Read the instruction and its invocation configuration from an agent file'
            cand --file 'Read the instruction and its invocation configuration from an agent file'
            cand --var 'Supply one declared agent-file template variable'
            cand --attach 'Add exact image bytes as ordered context before the prompt text'
            cand -m 'provider/model slug, e.g. ollama/qwen3'
            cand --model 'provider/model slug, e.g. ollama/qwen3'
            cand --sandbox 'Filesystem confinement: ro|workspace|adaptive|off; off = no confinement, credentials included'
            cand --thinking 'off|minimal|low|medium|high|xhigh|max'
            cand --read-only-dir 'Grant reads of one existing canonical absolute upload directory'
            cand --system 'Replace the system prompt. `@file` reads it from a file'
            cand --append-system 'Append to the system prompt. `@file` reads it from a file'
            cand --output-style 'Select a trusted prompt-only style: concise, explanatory, or NAME from EGO_HOME/output-styles/NAME.md'
            cand --instructions 'Use this instruction file instead of discovering one'
            cand --tools 'Restrict the builtin tools to this comma-separated list'
            cand --allow 'Allow rule, e.g. ''read(src/**)''. Repeatable'
            cand --deny 'Deny rule, e.g. ''write(**)''. Repeatable'
            cand --profile 'Layer [profile.NAME] from config'
            cand --session 'Named session: created or attached'
            cand --session-id 'A session id supplied by the caller'
            cand --session-dir 'Override the store location'
            cand -C 'Run as if started in DIR'
            cand --output 'text|json|jsonl'
            cand --artifact-dir 'Override the sanitized run-artifact root for profile `ci`'
            cand --schema 'JSON Schema the final answer must validate against; `@file` or inline'
            cand --schema-retries 'How many times to re-prompt on a schema mismatch'
            cand --retry 'Additional delivery-safe attempts for each candidate'
            cand --fallback 'Ordered provider/model candidate used after a delivery-safe failure'
            cand --budget 'Hard cap: ''2.5$'' or ''400k'' tokens'
            cand --max-output-tokens 'Maximum generated tokens per request'
            cand --goal 'Set a checked session objective'
            cand --context-interval 'Minimum completed turns between model-requested transitions'
            cand --goal-turns 'Maximum automatic goal continuation turns'
            cand --goal-check-command 'Evaluator argv as a JSON string array; reads evidence JSON on stdin'
            cand --max-turns 'Cap the turns this run may take'
            cand --timeout 'Wall clock, e.g. ''90s'' or ''10m'''
            cand --mcp 'Select an effective server by name, even one marked enabled = false, or attach a strict invocation-scoped JSON MCP server'
            cand --mode 'Permission mode: plan|default|auto|edits|yolo'
            cand -c 'Dotted-path config override, value parsed as TOML'
            cand --set 'Dotted-path config override, value parsed as TOML'
            cand --offline 'No non-loopback egress from ego''s own HTTP transport'
            cand --bare 'No ambient discovery, even in a trusted workspace'
            cand --no-tools 'Offer no tools at all'
            cand --trust-workspace 'Trust this launch''s workspace without asking; remember the answer'
            cand --ephemeral 'Refused: every run uses a durable session'
            cand --no-context-tool 'Disable model-requested context transitions'
            cand --context-clear 'Permit the model to discard conversation history using clear'
            cand --plan 'Shorthand for --mode plan'
            cand --yolo 'Shorthand for --mode yolo'
            cand -q 'Suppress progress and diagnostics on stderr; stdout is unaffected'
            cand --quiet 'Suppress progress and diagnostics on stderr; stdout is unaffected'
            cand --no-color 'Never emit ANSI colour, whatever the terminal claims to support'
            cand --plain 'Append plain output without terminal styling or live retry updates'
            cand --vim 'Use Vim normal/insert editing in the interactive composer'
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'ego;resume'= {
            cand --answer 'Settle a pending question. Repeat for multiple sibling questions'
            cand --worktree 'Create a retained managed worktree on this explicitly approved new branch'
            cand -f 'Read the instruction and its invocation configuration from an agent file'
            cand --file 'Read the instruction and its invocation configuration from an agent file'
            cand --var 'Supply one declared agent-file template variable'
            cand --attach 'Add exact image bytes as ordered context before the prompt text'
            cand -m 'provider/model slug, e.g. ollama/qwen3'
            cand --model 'provider/model slug, e.g. ollama/qwen3'
            cand --sandbox 'Filesystem confinement: ro|workspace|adaptive|off; off = no confinement, credentials included'
            cand --thinking 'off|minimal|low|medium|high|xhigh|max'
            cand --read-only-dir 'Grant reads of one existing canonical absolute upload directory'
            cand --system 'Replace the system prompt. `@file` reads it from a file'
            cand --append-system 'Append to the system prompt. `@file` reads it from a file'
            cand --output-style 'Select a trusted prompt-only style: concise, explanatory, or NAME from EGO_HOME/output-styles/NAME.md'
            cand --instructions 'Use this instruction file instead of discovering one'
            cand --tools 'Restrict the builtin tools to this comma-separated list'
            cand --allow 'Allow rule, e.g. ''read(src/**)''. Repeatable'
            cand --deny 'Deny rule, e.g. ''write(**)''. Repeatable'
            cand --profile 'Layer [profile.NAME] from config'
            cand --session 'Named session: created or attached'
            cand --session-id 'A session id supplied by the caller'
            cand --session-dir 'Override the store location'
            cand -C 'Run as if started in DIR'
            cand --output 'text|json|jsonl'
            cand --artifact-dir 'Override the sanitized run-artifact root for profile `ci`'
            cand --schema 'JSON Schema the final answer must validate against; `@file` or inline'
            cand --schema-retries 'How many times to re-prompt on a schema mismatch'
            cand --retry 'Additional delivery-safe attempts for each candidate'
            cand --fallback 'Ordered provider/model candidate used after a delivery-safe failure'
            cand --budget 'Hard cap: ''2.5$'' or ''400k'' tokens'
            cand --max-output-tokens 'Maximum generated tokens per request'
            cand --goal 'Set a checked session objective'
            cand --context-interval 'Minimum completed turns between model-requested transitions'
            cand --goal-turns 'Maximum automatic goal continuation turns'
            cand --goal-check-command 'Evaluator argv as a JSON string array; reads evidence JSON on stdin'
            cand --max-turns 'Cap the turns this run may take'
            cand --timeout 'Wall clock, e.g. ''90s'' or ''10m'''
            cand --mcp 'Select an effective server by name, even one marked enabled = false, or attach a strict invocation-scoped JSON MCP server'
            cand --mode 'Permission mode: plan|default|auto|edits|yolo'
            cand -c 'Dotted-path config override, value parsed as TOML'
            cand --set 'Dotted-path config override, value parsed as TOML'
            cand --offline 'No non-loopback egress from ego''s own HTTP transport'
            cand --bare 'No ambient discovery, even in a trusted workspace'
            cand --no-tools 'Offer no tools at all'
            cand --trust-workspace 'Trust this launch''s workspace without asking; remember the answer'
            cand --ephemeral 'Refused: every run uses a durable session'
            cand --no-context-tool 'Disable model-requested context transitions'
            cand --context-clear 'Permit the model to discard conversation history using clear'
            cand --plan 'Shorthand for --mode plan'
            cand --yolo 'Shorthand for --mode yolo'
            cand -q 'Suppress progress and diagnostics on stderr; stdout is unaffected'
            cand --quiet 'Suppress progress and diagnostics on stderr; stdout is unaffected'
            cand --no-color 'Never emit ANSI colour, whatever the terminal claims to support'
            cand --plain 'Append plain output without terminal styling or live retry updates'
            cand --vim 'Use Vim normal/insert editing in the interactive composer'
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'ego;continue'= {
            cand --answer 'Settle a pending question. Repeat for multiple sibling questions'
            cand --worktree 'Create a retained managed worktree on this explicitly approved new branch'
            cand -f 'Read the instruction and its invocation configuration from an agent file'
            cand --file 'Read the instruction and its invocation configuration from an agent file'
            cand --var 'Supply one declared agent-file template variable'
            cand --attach 'Add exact image bytes as ordered context before the prompt text'
            cand -m 'provider/model slug, e.g. ollama/qwen3'
            cand --model 'provider/model slug, e.g. ollama/qwen3'
            cand --sandbox 'Filesystem confinement: ro|workspace|adaptive|off; off = no confinement, credentials included'
            cand --thinking 'off|minimal|low|medium|high|xhigh|max'
            cand --read-only-dir 'Grant reads of one existing canonical absolute upload directory'
            cand --system 'Replace the system prompt. `@file` reads it from a file'
            cand --append-system 'Append to the system prompt. `@file` reads it from a file'
            cand --output-style 'Select a trusted prompt-only style: concise, explanatory, or NAME from EGO_HOME/output-styles/NAME.md'
            cand --instructions 'Use this instruction file instead of discovering one'
            cand --tools 'Restrict the builtin tools to this comma-separated list'
            cand --allow 'Allow rule, e.g. ''read(src/**)''. Repeatable'
            cand --deny 'Deny rule, e.g. ''write(**)''. Repeatable'
            cand --profile 'Layer [profile.NAME] from config'
            cand --session 'Named session: created or attached'
            cand --session-id 'A session id supplied by the caller'
            cand --session-dir 'Override the store location'
            cand -C 'Run as if started in DIR'
            cand --output 'text|json|jsonl'
            cand --artifact-dir 'Override the sanitized run-artifact root for profile `ci`'
            cand --schema 'JSON Schema the final answer must validate against; `@file` or inline'
            cand --schema-retries 'How many times to re-prompt on a schema mismatch'
            cand --retry 'Additional delivery-safe attempts for each candidate'
            cand --fallback 'Ordered provider/model candidate used after a delivery-safe failure'
            cand --budget 'Hard cap: ''2.5$'' or ''400k'' tokens'
            cand --max-output-tokens 'Maximum generated tokens per request'
            cand --goal 'Set a checked session objective'
            cand --context-interval 'Minimum completed turns between model-requested transitions'
            cand --goal-turns 'Maximum automatic goal continuation turns'
            cand --goal-check-command 'Evaluator argv as a JSON string array; reads evidence JSON on stdin'
            cand --max-turns 'Cap the turns this run may take'
            cand --timeout 'Wall clock, e.g. ''90s'' or ''10m'''
            cand --mcp 'Select an effective server by name, even one marked enabled = false, or attach a strict invocation-scoped JSON MCP server'
            cand --mode 'Permission mode: plan|default|auto|edits|yolo'
            cand -c 'Dotted-path config override, value parsed as TOML'
            cand --set 'Dotted-path config override, value parsed as TOML'
            cand --offline 'No non-loopback egress from ego''s own HTTP transport'
            cand --bare 'No ambient discovery, even in a trusted workspace'
            cand --no-tools 'Offer no tools at all'
            cand --trust-workspace 'Trust this launch''s workspace without asking; remember the answer'
            cand --ephemeral 'Refused: every run uses a durable session'
            cand --no-context-tool 'Disable model-requested context transitions'
            cand --context-clear 'Permit the model to discard conversation history using clear'
            cand --plan 'Shorthand for --mode plan'
            cand --yolo 'Shorthand for --mode yolo'
            cand -q 'Suppress progress and diagnostics on stderr; stdout is unaffected'
            cand --quiet 'Suppress progress and diagnostics on stderr; stdout is unaffected'
            cand --no-color 'Never emit ANSI colour, whatever the terminal claims to support'
            cand --plain 'Append plain output without terminal styling or live retry updates'
            cand --vim 'Use Vim normal/insert editing in the interactive composer'
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'ego;fork'= {
            cand --at 'at'
            cand --session-dir 'Override the store location'
            cand -C 'Run as if started in DIR'
            cand --output 'text|json|jsonl'
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'ego;rm'= {
            cand --session-dir 'Override the store location'
            cand -C 'Run as if started in DIR'
            cand --yes 'Supply deletion confirmation without prompting; required without terminal stdin'
            cand --force 'Acknowledge an uncertain prior run; does not replace deletion confirmation and is not a general override'
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'ego;compact'= {
            cand --budget 'Hard spend ceiling, as for run'
            cand --max-output-tokens 'Maximum generated tokens per request'
            cand --request-id 'request-id'
            cand --session-dir 'Override the store location'
            cand -C 'Run as if started in DIR'
            cand --output 'text|json|jsonl'
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'ego;export'= {
            cand --format 'Transcript format; JSONL is stdout-only and cannot be combined with --file'
            cand --file 'Write to PATH instead of stdout; its parent must exist. Refuse symlinks, non-regular targets and existing files with different bytes; identical bytes are accepted'
            cand --session-dir 'Override the store location'
            cand -C 'Run as if started in DIR'
            cand --sanitize 'Remove private content structurally; exports are unsanitized by default'
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'ego;ls'= {
            cand --session-dir 'Override the store location'
            cand -C 'Run as if started in DIR'
            cand --output 'text|json|jsonl'
            cand --all 'Every session in the store, not just this workspace''s'
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'ego;usage'= {
            cand --period 'all|Ns|Nm|Nh'
            cand --session-dir 'Override the store location'
            cand -C 'Run as if started in DIR'
            cand --json 'Emit the exact versioned usage-report object'
            cand --all-roles 'Keep role rows that recorded no activity in the text table'
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'ego;models'= {
            cand --provider 'Restrict enumeration and output to one admitted provider'
            cand --refresh 'Refresh the selected discoverable provider slices'
            cand --json 'Emit the exact versioned snapshot object'
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'ego;hooks'= {
            cand -h 'Print help'
            cand --help 'Print help'
            cand ls 'List the admitted hook catalogue without changing configuration'
        }
        &'ego;hooks;ls'= {
            cand --json 'Emit the exact versioned hook-listing object'
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'ego;doctor'= {
            cand --json 'Emit the exact versioned health snapshot'
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'ego;setup'= {
            cand --items 'Limit proposals to these report item IDs (comma-separated)'
            cand --judge 'Offer a validated judge for unresolved policy; never authorizes an action'
            cand --profile 'profile'
            cand --directory 'directory'
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'ego;acp'= {
            cand --session-dir 'Override the store location'
            cand -C 'Run as if started in DIR'
            cand --sandbox 'The weakest confinement a session may select: ro|workspace|adaptive|off; off = no confinement, credentials included'
            cand --read-only-dir 'Grant reads of one existing canonical absolute upload directory'
            cand --system 'Replace the system prompt. `@file` reads it from a file'
            cand --append-system 'Append to the system prompt. `@file` reads it from a file'
            cand --output-style 'Select a trusted prompt-only style: concise, explanatory, or NAME from EGO_HOME/output-styles/NAME.md'
            cand --instructions 'Use this instruction file instead of discovering one'
            cand --tools 'Restrict the builtin tools to this comma-separated list'
            cand --allow 'Allow rule, e.g. ''read(src/**)''. Repeatable'
            cand --deny 'Deny rule, e.g. ''write(**)''. Repeatable'
            cand --profile 'Layer [profile.NAME] from config'
            cand --listen 'Serve on a socket path or address instead of stdio'
            cand --max-connections 'Maximum simultaneous listener connections (runtime default: 32)'
            cand --connection-lifetime 'Maximum lifetime of one listener connection: 90s, 10m or 2h (runtime default: 24h)'
            cand --max-ingress-bytes 'Maximum cumulative bytes read from one listener connection (runtime default: 134217728 bytes)'
            cand --bare 'No ambient discovery, even in a trusted workspace'
            cand --no-tools 'Offer no tools at all'
            cand --allow-public-bind 'Serve on an address reachable from outside this machine'
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'ego;a2a'= {
            cand --session-dir 'Override the store location'
            cand -C 'Run as if started in DIR'
            cand --sandbox 'The weakest confinement a session may select: ro|workspace|adaptive|off; off = no confinement, credentials included'
            cand --listen 'Where to serve: a socket path, or an address such as `127.0.0.1:8080`'
            cand --allow-public-bind 'Serve on an address reachable from outside this machine'
            cand -h 'Print help (see more with ''--help'')'
            cand --help 'Print help (see more with ''--help'')'
        }
        &'ego;auth'= {
            cand -h 'Print help'
            cand --help 'Print help'
            cand login 'Authenticate and store a provider credential'
            cand logout 'Remove a stored provider credential'
            cand status 'Show credential type and expiry, never token values'
        }
        &'ego;auth;login'= {
            cand --device 'Prefer device-code login when the provider offers it'
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'ego;auth;logout'= {
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'ego;auth;status'= {
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'ego;mcp'= {
            cand -h 'Print help'
            cand --help 'Print help'
            cand add 'Add or replace one complete server entry'
            cand rm 'Remove, mask, or reveal one configured server entry'
            cand ls 'List configured and admitted MCP state'
            cand login 'Authorize a public OAuth client with an explicit registration strategy'
            cand reload 'Reconcile every configured server, or one named server'
        }
        &'ego;mcp;add'= {
            cand --http 'Configure a Streamable HTTP transport'
            cand --cwd 'Working directory for the stdio child'
            cand --inherit-env 'Environment variable inherited by the stdio child. Repeatable'
            cand --profile 'Explicit HTTP wire profile; never selected by fallback'
            cand --auth 'HTTP authentication mode'
            cand --stdio 'Configure a subprocess transport. Its command follows literal `--`'
            cand --allow-insecure-loopback 'Consent to plaintext HTTP for an exact loopback host'
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'ego;mcp;rm'= {
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'ego;mcp;ls'= {
            cand --json 'Emit the typed machine-readable projection'
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'ego;mcp;login'= {
            cand --client-id 'Operator-issued public client ID'
            cand --cimd 'Published HTTPS client metadata document URL'
            cand --callback-port 'Loopback port registered with the authorization server (0 selects a free port)'
            cand --issuer 'issuer'
            cand --account 'account'
            cand --dcr 'Explicitly use issuer-advertised dynamic registration'
            cand --refresh 'Refresh an existing issuer/resource/account-bound credential without interaction'
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'ego;mcp;reload'= {
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'ego;config'= {
            cand -h 'Print help'
            cand --help 'Print help'
            cand capabilities 'Inspect managed owner transaction capabilities as JSON'
            cand snapshot 'Read stored values and their opaque owner revision as JSON'
            cand batch 'Atomically publish a bounded set of typed managed edits'
            cand get 'Read one exact stored value'
            cand set 'Store one strictly typed TOML value'
            cand unset 'Remove one exact stored value'
            cand ls 'List the exact stored managed projection'
            cand edit 'Edit the complete user document in the selected external editor'
        }
        &'ego;config;capabilities'= {
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'ego;config;snapshot'= {
            cand --profile 'profile'
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'ego;config;batch'= {
            cand --profile 'profile'
            cand --set 'set'
            cand --if-revision 'if-revision'
            cand --unset 'unset'
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'ego;config;get'= {
            cand --profile 'profile'
            cand --json 'json'
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'ego;config;set'= {
            cand --profile 'profile'
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'ego;config;unset'= {
            cand --profile 'profile'
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'ego;config;ls'= {
            cand --profile 'profile'
            cand --json 'json'
            cand --effective 'Show the effective run perimeter instead of stored values'
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'ego;config;edit'= {
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'ego;roots'= {
            cand -h 'Print help'
            cand --help 'Print help'
            cand add 'Add or update one user perimeter root'
            cand rm 'Remove a root by its stored spelling'
            cand ls 'List roots stored in the selected user target'
        }
        &'ego;roots;add'= {
            cand --profile 'profile'
            cand --access 'access'
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'ego;roots;rm'= {
            cand --profile 'profile'
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'ego;roots;ls'= {
            cand --profile 'profile'
            cand --json 'json'
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'ego;rules'= {
            cand -h 'Print help'
            cand --help 'Print help'
            cand ls 'List the effective system and selected user rules'
            cand allow 'Persist one allow predicate'
            cand deny 'Persist one deny predicate'
            cand revoke 'Remove one predicate ID from both selected user lists'
            cand remember 'Derive persistent rules from one prepared builtin call'
            cand explain 'Explain one prepared builtin call without running it'
        }
        &'ego;rules;ls'= {
            cand --profile 'profile'
            cand --json 'json'
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'ego;rules;allow'= {
            cand --profile 'profile'
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'ego;rules;deny'= {
            cand --profile 'profile'
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'ego;rules;revoke'= {
            cand --profile 'profile'
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'ego;rules;remember'= {
            cand --profile 'profile'
            cand -C 'C'
            cand --yes 'yes'
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'ego;rules;explain'= {
            cand --profile 'profile'
            cand -C 'C'
            cand --json 'json'
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'ego;mcp-server'= {
            cand --session-dir 'Override the store location'
            cand -C 'Run as if started in DIR'
            cand --max-concurrent-runs 'max-concurrent-runs'
            cand --retained-requests 'retained-requests'
            cand --max-resource-bytes 'max-resource-bytes'
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'ego;orchestrator'= {
            cand -h 'Print help'
            cand --help 'Print help'
            cand brief 'Inspect tracked children and unread results without model generation or inbox reads'
        }
        &'ego;orchestrator;brief'= {
            cand --root 'root'
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'ego;worktree'= {
            cand -h 'Print help'
            cand --help 'Print help'
            cand show 'Inspect a durable local worktree receipt without starting a model'
            cand cleanup 'Remove only a clean worktree whose commits are integrated; retain its branch'
        }
        &'ego;worktree;show'= {
            cand -h 'Print help'
            cand --help 'Print help'
        }
        &'ego;worktree;cleanup'= {
            cand -h 'Print help'
            cand --help 'Print help'
        }
    ]
    $completions[$command]
}
