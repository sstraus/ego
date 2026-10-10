# Print an optspec for argparse to handle cmd's options that are independent of any subcommand.
function __fish_ego_global_optspecs
    string join \n h/help
end

function __fish_ego_needs_command
    # Figure out if the current invocation already has a command.
    set -l cmd (commandline -opc)
    set -e cmd[1]
    argparse -s (__fish_ego_global_optspecs) -- $cmd 2>/dev/null
    or return
    if set -q argv[1]
        # Also print the command, so this can be used to figure out what it is.
        echo $argv[1]
        return 1
    end
    return 0
end

function __fish_ego_using_subcommand
    set -l cmd (__fish_ego_needs_command)
    test -z "$cmd"
    and return 1
    contains -- $cmd[1] $argv
end

complete -c ego -n "__fish_ego_needs_command" -s h -l help -d 'Print help'
complete -c ego -n "__fish_ego_needs_command" -f -a "run" -d 'Run an agent from a prompt, headless.'
complete -c ego -n "__fish_ego_needs_command" -f -a "resume" -d 'Continue a session; no ID = latest in this workspace.'
complete -c ego -n "__fish_ego_needs_command" -f -a "fork" -d 'Fork a session at its tip or after the completed turn containing a message.'
complete -c ego -n "__fish_ego_needs_command" -f -a "compact" -d 'Compact a session into its deterministic successor.'
complete -c ego -n "__fish_ego_needs_command" -f -a "export" -d 'Export a session locally or as a structurally sanitized bundle.'
complete -c ego -n "__fish_ego_needs_command" -f -a "ls" -d 'List sessions (this workspace; --all for every one).'
complete -c ego -n "__fish_ego_needs_command" -f -a "usage" -d 'Report usage, context and cost for this workspace.'
complete -c ego -n "__fish_ego_needs_command" -f -a "models" -d 'List the shared model catalogue.'
complete -c ego -n "__fish_ego_needs_command" -f -a "hooks" -d 'Inspect the admitted system and user hook catalogue.'
complete -c ego -n "__fish_ego_needs_command" -f -a "doctor" -d 'Report a read-only redacted health snapshot of this installation.'
complete -c ego -n "__fish_ego_needs_command" -f -a "acp" -d 'Serve the Agent Client Protocol on stdio or an endpoint.'
complete -c ego -n "__fish_ego_needs_command" -f -a "a2a" -d 'Serve the Agent2Agent protocol on a local or loopback endpoint.'
complete -c ego -n "__fish_ego_needs_command" -f -a "auth" -d 'Manage subscription credentials.'
complete -c ego -n "__fish_ego_needs_command" -f -a "mcp" -d 'Manage configured MCP servers.'
complete -c ego -n "__fish_ego_needs_command" -f -a "config" -d 'Manage stored user configuration.'
complete -c ego -n "__fish_ego_needs_command" -f -a "roots"
complete -c ego -n "__fish_ego_needs_command" -f -a "rules" -d 'Inspect and manage persistent permission rules.'
complete -c ego -n "__fish_ego_needs_command" -f -a "mcp-server" -d 'Serve scoped ego runs over MCP on stdio.'
complete -c ego -n "__fish_ego_needs_command" -f -a "orchestrator"
complete -c ego -n "__fish_ego_using_subcommand run" -s f -l file -d 'Read the instruction and its invocation configuration from an agent file' -r -F
complete -c ego -n "__fish_ego_using_subcommand run" -l var -d 'Supply one declared agent-file template variable' -r
complete -c ego -n "__fish_ego_using_subcommand run" -l attach -d 'Add exact image bytes as ordered context before the prompt text' -r
complete -c ego -n "__fish_ego_using_subcommand run" -s m -l model -d 'provider/model slug, e.g. ollama/qwen3' -r
complete -c ego -n "__fish_ego_using_subcommand run" -l sandbox -d 'Filesystem confinement: ro|workspace|off' -r
complete -c ego -n "__fish_ego_using_subcommand run" -l thinking -d 'off|minimal|low|medium|high|xhigh|max' -r
complete -c ego -n "__fish_ego_using_subcommand run" -l system -d 'Replace the system prompt. `@file` reads it from a file' -r
complete -c ego -n "__fish_ego_using_subcommand run" -l append-system -d 'Append to the system prompt. `@file` reads it from a file' -r
complete -c ego -n "__fish_ego_using_subcommand run" -l instructions -d 'Use this instruction file instead of discovering one' -r -F
complete -c ego -n "__fish_ego_using_subcommand run" -l tools -d 'Restrict the builtin tools to this comma-separated list' -r
complete -c ego -n "__fish_ego_using_subcommand run" -l allow -d 'Allow rule, e.g. \'read(src/**)\'. Repeatable' -r
complete -c ego -n "__fish_ego_using_subcommand run" -l deny -d 'Deny rule, e.g. \'write(**)\'. Repeatable' -r
complete -c ego -n "__fish_ego_using_subcommand run" -l profile -d 'Layer [profile.NAME] from config' -r
complete -c ego -n "__fish_ego_using_subcommand run" -l session -d 'Named session: created or attached' -r
complete -c ego -n "__fish_ego_using_subcommand run" -l session-id -d 'A session id supplied by the caller' -r
complete -c ego -n "__fish_ego_using_subcommand run" -l session-dir -d 'Override the store location' -r -F
complete -c ego -n "__fish_ego_using_subcommand run" -s C -d 'Run as if started in DIR' -r -F
complete -c ego -n "__fish_ego_using_subcommand run" -l output -d 'text|json|jsonl' -r -f -a "text\t''
json\t''
jsonl\t''"
complete -c ego -n "__fish_ego_using_subcommand run" -l artifact-dir -d 'Override the sanitized run-artifact root for profile `ci`' -r -F
complete -c ego -n "__fish_ego_using_subcommand run" -l schema -d 'JSON Schema the final answer must validate against; `@file` or inline' -r
complete -c ego -n "__fish_ego_using_subcommand run" -l schema-retries -d 'How many times to re-prompt on a schema mismatch' -r
complete -c ego -n "__fish_ego_using_subcommand run" -l retry -d 'Additional delivery-safe attempts for each candidate' -r
complete -c ego -n "__fish_ego_using_subcommand run" -l fallback -d 'Ordered provider/model candidate used after a delivery-safe failure' -r
complete -c ego -n "__fish_ego_using_subcommand run" -l budget -d 'Hard cap: \'2.5$\' or \'400k\' tokens' -r
complete -c ego -n "__fish_ego_using_subcommand run" -l max-turns -d 'Cap the turns this run may take' -r
complete -c ego -n "__fish_ego_using_subcommand run" -l timeout -d 'Wall clock, e.g. \'90s\' or \'10m\'' -r
complete -c ego -n "__fish_ego_using_subcommand run" -l mcp -d 'Attach a named effective or strict invocation-scoped JSON MCP server' -r
complete -c ego -n "__fish_ego_using_subcommand run" -l mode -d 'Permission mode: plan|default|auto|edits|yolo' -r
complete -c ego -n "__fish_ego_using_subcommand run" -s c -l set -d 'Dotted-path config override, value parsed as TOML' -r
complete -c ego -n "__fish_ego_using_subcommand run" -l offline -d 'No non-loopback egress from ego\'s own HTTP transport'
complete -c ego -n "__fish_ego_using_subcommand run" -l no-tools -d 'Offer no tools at all'
complete -c ego -n "__fish_ego_using_subcommand run" -l bare -d 'No ambient discovery, even in a trusted workspace'
complete -c ego -n "__fish_ego_using_subcommand run" -l trust-workspace -d 'Trust this interactive launch\'s workspace without asking; remember the answer'
complete -c ego -n "__fish_ego_using_subcommand run" -l ephemeral -d 'Refused: every run uses a durable session'
complete -c ego -n "__fish_ego_using_subcommand run" -l plan -d 'Shorthand for --mode plan'
complete -c ego -n "__fish_ego_using_subcommand run" -l yolo -d 'Shorthand for --mode yolo'
complete -c ego -n "__fish_ego_using_subcommand run" -s q -l quiet -d 'Suppress progress and diagnostics on stderr; stdout is unaffected'
complete -c ego -n "__fish_ego_using_subcommand run" -l no-color -d 'Never emit ANSI colour, whatever the terminal claims to support'
complete -c ego -n "__fish_ego_using_subcommand run" -s h -l help -d 'Print help'
complete -c ego -n "__fish_ego_using_subcommand resume" -l answer -d 'Settle a pending question. Repeat for multiple sibling questions' -r
complete -c ego -n "__fish_ego_using_subcommand resume" -s f -l file -d 'Read the instruction and its invocation configuration from an agent file' -r -F
complete -c ego -n "__fish_ego_using_subcommand resume" -l var -d 'Supply one declared agent-file template variable' -r
complete -c ego -n "__fish_ego_using_subcommand resume" -l attach -d 'Add exact image bytes as ordered context before the prompt text' -r
complete -c ego -n "__fish_ego_using_subcommand resume" -s m -l model -d 'provider/model slug, e.g. ollama/qwen3' -r
complete -c ego -n "__fish_ego_using_subcommand resume" -l sandbox -d 'Filesystem confinement: ro|workspace|off' -r
complete -c ego -n "__fish_ego_using_subcommand resume" -l thinking -d 'off|minimal|low|medium|high|xhigh|max' -r
complete -c ego -n "__fish_ego_using_subcommand resume" -l system -d 'Replace the system prompt. `@file` reads it from a file' -r
complete -c ego -n "__fish_ego_using_subcommand resume" -l append-system -d 'Append to the system prompt. `@file` reads it from a file' -r
complete -c ego -n "__fish_ego_using_subcommand resume" -l instructions -d 'Use this instruction file instead of discovering one' -r -F
complete -c ego -n "__fish_ego_using_subcommand resume" -l tools -d 'Restrict the builtin tools to this comma-separated list' -r
complete -c ego -n "__fish_ego_using_subcommand resume" -l allow -d 'Allow rule, e.g. \'read(src/**)\'. Repeatable' -r
complete -c ego -n "__fish_ego_using_subcommand resume" -l deny -d 'Deny rule, e.g. \'write(**)\'. Repeatable' -r
complete -c ego -n "__fish_ego_using_subcommand resume" -l profile -d 'Layer [profile.NAME] from config' -r
complete -c ego -n "__fish_ego_using_subcommand resume" -l session -d 'Named session: created or attached' -r
complete -c ego -n "__fish_ego_using_subcommand resume" -l session-id -d 'A session id supplied by the caller' -r
complete -c ego -n "__fish_ego_using_subcommand resume" -l session-dir -d 'Override the store location' -r -F
complete -c ego -n "__fish_ego_using_subcommand resume" -s C -d 'Run as if started in DIR' -r -F
complete -c ego -n "__fish_ego_using_subcommand resume" -l output -d 'text|json|jsonl' -r -f -a "text\t''
json\t''
jsonl\t''"
complete -c ego -n "__fish_ego_using_subcommand resume" -l artifact-dir -d 'Override the sanitized run-artifact root for profile `ci`' -r -F
complete -c ego -n "__fish_ego_using_subcommand resume" -l schema -d 'JSON Schema the final answer must validate against; `@file` or inline' -r
complete -c ego -n "__fish_ego_using_subcommand resume" -l schema-retries -d 'How many times to re-prompt on a schema mismatch' -r
complete -c ego -n "__fish_ego_using_subcommand resume" -l retry -d 'Additional delivery-safe attempts for each candidate' -r
complete -c ego -n "__fish_ego_using_subcommand resume" -l fallback -d 'Ordered provider/model candidate used after a delivery-safe failure' -r
complete -c ego -n "__fish_ego_using_subcommand resume" -l budget -d 'Hard cap: \'2.5$\' or \'400k\' tokens' -r
complete -c ego -n "__fish_ego_using_subcommand resume" -l max-turns -d 'Cap the turns this run may take' -r
complete -c ego -n "__fish_ego_using_subcommand resume" -l timeout -d 'Wall clock, e.g. \'90s\' or \'10m\'' -r
complete -c ego -n "__fish_ego_using_subcommand resume" -l mcp -d 'Attach a named effective or strict invocation-scoped JSON MCP server' -r
complete -c ego -n "__fish_ego_using_subcommand resume" -l mode -d 'Permission mode: plan|default|auto|edits|yolo' -r
complete -c ego -n "__fish_ego_using_subcommand resume" -s c -l set -d 'Dotted-path config override, value parsed as TOML' -r
complete -c ego -n "__fish_ego_using_subcommand resume" -l offline -d 'No non-loopback egress from ego\'s own HTTP transport'
complete -c ego -n "__fish_ego_using_subcommand resume" -l no-tools -d 'Offer no tools at all'
complete -c ego -n "__fish_ego_using_subcommand resume" -l bare -d 'No ambient discovery, even in a trusted workspace'
complete -c ego -n "__fish_ego_using_subcommand resume" -l trust-workspace -d 'Trust this interactive launch\'s workspace without asking; remember the answer'
complete -c ego -n "__fish_ego_using_subcommand resume" -l ephemeral -d 'Refused: every run uses a durable session'
complete -c ego -n "__fish_ego_using_subcommand resume" -l plan -d 'Shorthand for --mode plan'
complete -c ego -n "__fish_ego_using_subcommand resume" -l yolo -d 'Shorthand for --mode yolo'
complete -c ego -n "__fish_ego_using_subcommand resume" -s q -l quiet -d 'Suppress progress and diagnostics on stderr; stdout is unaffected'
complete -c ego -n "__fish_ego_using_subcommand resume" -l no-color -d 'Never emit ANSI colour, whatever the terminal claims to support'
complete -c ego -n "__fish_ego_using_subcommand resume" -s h -l help -d 'Print help'
complete -c ego -n "__fish_ego_using_subcommand fork" -l at -r
complete -c ego -n "__fish_ego_using_subcommand fork" -l session-dir -d 'Override the store location' -r -F
complete -c ego -n "__fish_ego_using_subcommand fork" -s C -d 'Run as if started in DIR' -r -F
complete -c ego -n "__fish_ego_using_subcommand fork" -l output -d 'text|json|jsonl' -r -f -a "text\t''
json\t''
jsonl\t''"
complete -c ego -n "__fish_ego_using_subcommand fork" -s h -l help -d 'Print help'
complete -c ego -n "__fish_ego_using_subcommand compact" -l request-id -r
complete -c ego -n "__fish_ego_using_subcommand compact" -l session-dir -d 'Override the store location' -r -F
complete -c ego -n "__fish_ego_using_subcommand compact" -s C -d 'Run as if started in DIR' -r -F
complete -c ego -n "__fish_ego_using_subcommand compact" -l output -d 'text|json|jsonl' -r -f -a "text\t''
json\t''
jsonl\t''"
complete -c ego -n "__fish_ego_using_subcommand compact" -s h -l help -d 'Print help'
complete -c ego -n "__fish_ego_using_subcommand export" -l format -r -f -a "md\t''
json\t''
jsonl\t''"
complete -c ego -n "__fish_ego_using_subcommand export" -l file -r -F
complete -c ego -n "__fish_ego_using_subcommand export" -l session-dir -d 'Override the store location' -r -F
complete -c ego -n "__fish_ego_using_subcommand export" -s C -d 'Run as if started in DIR' -r -F
complete -c ego -n "__fish_ego_using_subcommand export" -l sanitize
complete -c ego -n "__fish_ego_using_subcommand export" -s h -l help -d 'Print help'
complete -c ego -n "__fish_ego_using_subcommand ls" -l session-dir -d 'Override the store location' -r -F
complete -c ego -n "__fish_ego_using_subcommand ls" -s C -d 'Run as if started in DIR' -r -F
complete -c ego -n "__fish_ego_using_subcommand ls" -l output -d 'text|json|jsonl' -r -f -a "text\t''
json\t''
jsonl\t''"
complete -c ego -n "__fish_ego_using_subcommand ls" -l all -d 'Every session in the store, not just this workspace\'s'
complete -c ego -n "__fish_ego_using_subcommand ls" -s h -l help -d 'Print help'
complete -c ego -n "__fish_ego_using_subcommand usage" -l period -d 'all|Ns|Nm|Nh' -r
complete -c ego -n "__fish_ego_using_subcommand usage" -l session-dir -d 'Override the store location' -r -F
complete -c ego -n "__fish_ego_using_subcommand usage" -s C -d 'Run as if started in DIR' -r -F
complete -c ego -n "__fish_ego_using_subcommand usage" -l json -d 'Emit the exact versioned usage-report object'
complete -c ego -n "__fish_ego_using_subcommand usage" -l all-roles -d 'Keep role rows that recorded no activity in the text table'
complete -c ego -n "__fish_ego_using_subcommand usage" -s h -l help -d 'Print help'
complete -c ego -n "__fish_ego_using_subcommand models" -l provider -d 'Restrict enumeration and output to one admitted provider' -r
complete -c ego -n "__fish_ego_using_subcommand models" -l refresh -d 'Refresh the selected discoverable provider slices'
complete -c ego -n "__fish_ego_using_subcommand models" -l json -d 'Emit the exact versioned snapshot object'
complete -c ego -n "__fish_ego_using_subcommand models" -s h -l help -d 'Print help'
complete -c ego -n "__fish_ego_using_subcommand hooks; and not __fish_seen_subcommand_from ls" -s h -l help -d 'Print help'
complete -c ego -n "__fish_ego_using_subcommand hooks; and not __fish_seen_subcommand_from ls" -f -a "ls" -d 'List the admitted hook catalogue without changing configuration'
complete -c ego -n "__fish_ego_using_subcommand hooks; and __fish_seen_subcommand_from ls" -l json -d 'Emit the exact versioned hook-listing object'
complete -c ego -n "__fish_ego_using_subcommand hooks; and __fish_seen_subcommand_from ls" -s h -l help -d 'Print help'
complete -c ego -n "__fish_ego_using_subcommand doctor" -l json -d 'Emit the exact versioned health snapshot'
complete -c ego -n "__fish_ego_using_subcommand doctor" -s h -l help -d 'Print help'
complete -c ego -n "__fish_ego_using_subcommand acp" -l session-dir -d 'Override the store location' -r -F
complete -c ego -n "__fish_ego_using_subcommand acp" -s C -d 'Run as if started in DIR' -r -F
complete -c ego -n "__fish_ego_using_subcommand acp" -l sandbox -d 'The weakest confinement a session may select: ro|workspace|off' -r
complete -c ego -n "__fish_ego_using_subcommand acp" -l append-system -d 'Append to the system prompt. `@file` reads it from a file' -r
complete -c ego -n "__fish_ego_using_subcommand acp" -l instructions -d 'Use this instruction file instead of discovering one' -r -F
complete -c ego -n "__fish_ego_using_subcommand acp" -l tools -d 'Restrict the builtin tools to this comma-separated list' -r
complete -c ego -n "__fish_ego_using_subcommand acp" -l allow -d 'Allow rule, e.g. \'read(src/**)\'. Repeatable' -r
complete -c ego -n "__fish_ego_using_subcommand acp" -l deny -d 'Deny rule, e.g. \'write(**)\'. Repeatable' -r
complete -c ego -n "__fish_ego_using_subcommand acp" -l profile -d 'Layer [profile.NAME] from config' -r
complete -c ego -n "__fish_ego_using_subcommand acp" -l listen -d 'Serve on a socket path or address instead of stdio' -r
complete -c ego -n "__fish_ego_using_subcommand acp" -l max-connections -d 'Maximum simultaneous listener connections' -r
complete -c ego -n "__fish_ego_using_subcommand acp" -l connection-lifetime -d 'Maximum lifetime of one listener connection: 90s, 10m or 2h' -r
complete -c ego -n "__fish_ego_using_subcommand acp" -l max-ingress-bytes -d 'Maximum cumulative bytes read from one listener connection' -r
complete -c ego -n "__fish_ego_using_subcommand acp" -l no-tools -d 'Offer no tools at all'
complete -c ego -n "__fish_ego_using_subcommand acp" -l allow-public-bind -d 'Serve on an address reachable from outside this machine'
complete -c ego -n "__fish_ego_using_subcommand acp" -s h -l help -d 'Print help'
complete -c ego -n "__fish_ego_using_subcommand a2a" -l session-dir -d 'Override the store location' -r -F
complete -c ego -n "__fish_ego_using_subcommand a2a" -s C -d 'Run as if started in DIR' -r -F
complete -c ego -n "__fish_ego_using_subcommand a2a" -l sandbox -d 'The weakest confinement a session may select: ro|workspace|off' -r
complete -c ego -n "__fish_ego_using_subcommand a2a" -l listen -d 'Where to serve: a socket path, or an address such as `127.0.0.1:8080`' -r
complete -c ego -n "__fish_ego_using_subcommand a2a" -l allow-public-bind -d 'Serve on an address reachable from outside this machine'
complete -c ego -n "__fish_ego_using_subcommand a2a" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c ego -n "__fish_ego_using_subcommand auth; and not __fish_seen_subcommand_from login logout status" -s h -l help -d 'Print help'
complete -c ego -n "__fish_ego_using_subcommand auth; and not __fish_seen_subcommand_from login logout status" -f -a "login" -d 'Authenticate and store a provider credential'
complete -c ego -n "__fish_ego_using_subcommand auth; and not __fish_seen_subcommand_from login logout status" -f -a "logout" -d 'Remove a stored provider credential'
complete -c ego -n "__fish_ego_using_subcommand auth; and not __fish_seen_subcommand_from login logout status" -f -a "status" -d 'Show credential type and expiry, never token values'
complete -c ego -n "__fish_ego_using_subcommand auth; and __fish_seen_subcommand_from login" -l device -d 'Prefer device-code login when the provider offers it'
complete -c ego -n "__fish_ego_using_subcommand auth; and __fish_seen_subcommand_from login" -s h -l help -d 'Print help'
complete -c ego -n "__fish_ego_using_subcommand auth; and __fish_seen_subcommand_from logout" -s h -l help -d 'Print help'
complete -c ego -n "__fish_ego_using_subcommand auth; and __fish_seen_subcommand_from status" -s h -l help -d 'Print help'
complete -c ego -n "__fish_ego_using_subcommand mcp; and not __fish_seen_subcommand_from add rm ls login reload" -s h -l help -d 'Print help'
complete -c ego -n "__fish_ego_using_subcommand mcp; and not __fish_seen_subcommand_from add rm ls login reload" -f -a "add" -d 'Add or replace one complete server entry'
complete -c ego -n "__fish_ego_using_subcommand mcp; and not __fish_seen_subcommand_from add rm ls login reload" -f -a "rm" -d 'Remove, mask, or reveal one configured server entry'
complete -c ego -n "__fish_ego_using_subcommand mcp; and not __fish_seen_subcommand_from add rm ls login reload" -f -a "ls" -d 'List configured and admitted MCP state'
complete -c ego -n "__fish_ego_using_subcommand mcp; and not __fish_seen_subcommand_from add rm ls login reload" -f -a "login" -d 'Authorize one configured HTTP server through the product CIMD binding'
complete -c ego -n "__fish_ego_using_subcommand mcp; and not __fish_seen_subcommand_from add rm ls login reload" -f -a "reload" -d 'Reconcile every configured server, or one named server'
complete -c ego -n "__fish_ego_using_subcommand mcp; and __fish_seen_subcommand_from add" -l http -d 'Configure a Streamable HTTP transport' -r
complete -c ego -n "__fish_ego_using_subcommand mcp; and __fish_seen_subcommand_from add" -l cwd -d 'Working directory for the stdio child' -r -F
complete -c ego -n "__fish_ego_using_subcommand mcp; and __fish_seen_subcommand_from add" -l inherit-env -d 'Environment variable inherited by the stdio child. Repeatable' -r
complete -c ego -n "__fish_ego_using_subcommand mcp; and __fish_seen_subcommand_from add" -l auth -d 'HTTP authentication mode' -r -f -a "none\t''
oauth\t''"
complete -c ego -n "__fish_ego_using_subcommand mcp; and __fish_seen_subcommand_from add" -l stdio -d 'Configure a subprocess transport. Its command follows literal `--`'
complete -c ego -n "__fish_ego_using_subcommand mcp; and __fish_seen_subcommand_from add" -l allow-insecure-loopback -d 'Consent to plaintext HTTP for an exact loopback host'
complete -c ego -n "__fish_ego_using_subcommand mcp; and __fish_seen_subcommand_from add" -s h -l help -d 'Print help'
complete -c ego -n "__fish_ego_using_subcommand mcp; and __fish_seen_subcommand_from rm" -s h -l help -d 'Print help'
complete -c ego -n "__fish_ego_using_subcommand mcp; and __fish_seen_subcommand_from ls" -l json -d 'Emit the typed machine-readable projection'
complete -c ego -n "__fish_ego_using_subcommand mcp; and __fish_seen_subcommand_from ls" -s h -l help -d 'Print help'
complete -c ego -n "__fish_ego_using_subcommand mcp; and __fish_seen_subcommand_from login" -s h -l help -d 'Print help'
complete -c ego -n "__fish_ego_using_subcommand mcp; and __fish_seen_subcommand_from reload" -s h -l help -d 'Print help'
complete -c ego -n "__fish_ego_using_subcommand config; and not __fish_seen_subcommand_from get set unset ls edit" -s h -l help -d 'Print help'
complete -c ego -n "__fish_ego_using_subcommand config; and not __fish_seen_subcommand_from get set unset ls edit" -f -a "get" -d 'Read one exact stored value'
complete -c ego -n "__fish_ego_using_subcommand config; and not __fish_seen_subcommand_from get set unset ls edit" -f -a "set" -d 'Store one strictly typed TOML value'
complete -c ego -n "__fish_ego_using_subcommand config; and not __fish_seen_subcommand_from get set unset ls edit" -f -a "unset" -d 'Remove one exact stored value'
complete -c ego -n "__fish_ego_using_subcommand config; and not __fish_seen_subcommand_from get set unset ls edit" -f -a "ls" -d 'List the exact stored managed projection'
complete -c ego -n "__fish_ego_using_subcommand config; and not __fish_seen_subcommand_from get set unset ls edit" -f -a "edit" -d 'Edit the complete user document in the selected external editor'
complete -c ego -n "__fish_ego_using_subcommand config; and __fish_seen_subcommand_from get" -l profile -r
complete -c ego -n "__fish_ego_using_subcommand config; and __fish_seen_subcommand_from get" -l json
complete -c ego -n "__fish_ego_using_subcommand config; and __fish_seen_subcommand_from get" -s h -l help -d 'Print help'
complete -c ego -n "__fish_ego_using_subcommand config; and __fish_seen_subcommand_from set" -l profile -r
complete -c ego -n "__fish_ego_using_subcommand config; and __fish_seen_subcommand_from set" -s h -l help -d 'Print help'
complete -c ego -n "__fish_ego_using_subcommand config; and __fish_seen_subcommand_from unset" -l profile -r
complete -c ego -n "__fish_ego_using_subcommand config; and __fish_seen_subcommand_from unset" -s h -l help -d 'Print help'
complete -c ego -n "__fish_ego_using_subcommand config; and __fish_seen_subcommand_from ls" -l profile -r
complete -c ego -n "__fish_ego_using_subcommand config; and __fish_seen_subcommand_from ls" -l json
complete -c ego -n "__fish_ego_using_subcommand config; and __fish_seen_subcommand_from ls" -l effective -d 'Show the effective run perimeter instead of stored values'
complete -c ego -n "__fish_ego_using_subcommand config; and __fish_seen_subcommand_from ls" -s h -l help -d 'Print help'
complete -c ego -n "__fish_ego_using_subcommand config; and __fish_seen_subcommand_from edit" -s h -l help -d 'Print help'
complete -c ego -n "__fish_ego_using_subcommand roots; and not __fish_seen_subcommand_from add rm ls" -s h -l help -d 'Print help'
complete -c ego -n "__fish_ego_using_subcommand roots; and not __fish_seen_subcommand_from add rm ls" -f -a "add" -d 'Add or update one user perimeter root'
complete -c ego -n "__fish_ego_using_subcommand roots; and not __fish_seen_subcommand_from add rm ls" -f -a "rm" -d 'Remove a root by its stored spelling'
complete -c ego -n "__fish_ego_using_subcommand roots; and not __fish_seen_subcommand_from add rm ls" -f -a "ls" -d 'List roots stored in the selected user target'
complete -c ego -n "__fish_ego_using_subcommand roots; and __fish_seen_subcommand_from add" -l profile -r
complete -c ego -n "__fish_ego_using_subcommand roots; and __fish_seen_subcommand_from add" -l access -r
complete -c ego -n "__fish_ego_using_subcommand roots; and __fish_seen_subcommand_from add" -s h -l help -d 'Print help'
complete -c ego -n "__fish_ego_using_subcommand roots; and __fish_seen_subcommand_from rm" -l profile -r
complete -c ego -n "__fish_ego_using_subcommand roots; and __fish_seen_subcommand_from rm" -s h -l help -d 'Print help'
complete -c ego -n "__fish_ego_using_subcommand roots; and __fish_seen_subcommand_from ls" -l profile -r
complete -c ego -n "__fish_ego_using_subcommand roots; and __fish_seen_subcommand_from ls" -l json
complete -c ego -n "__fish_ego_using_subcommand roots; and __fish_seen_subcommand_from ls" -s h -l help -d 'Print help'
complete -c ego -n "__fish_ego_using_subcommand rules; and not __fish_seen_subcommand_from ls allow deny revoke remember explain" -s h -l help -d 'Print help'
complete -c ego -n "__fish_ego_using_subcommand rules; and not __fish_seen_subcommand_from ls allow deny revoke remember explain" -f -a "ls" -d 'List the effective system and selected user rules'
complete -c ego -n "__fish_ego_using_subcommand rules; and not __fish_seen_subcommand_from ls allow deny revoke remember explain" -f -a "allow" -d 'Persist one allow predicate'
complete -c ego -n "__fish_ego_using_subcommand rules; and not __fish_seen_subcommand_from ls allow deny revoke remember explain" -f -a "deny" -d 'Persist one deny predicate'
complete -c ego -n "__fish_ego_using_subcommand rules; and not __fish_seen_subcommand_from ls allow deny revoke remember explain" -f -a "revoke" -d 'Remove one predicate ID from both selected user lists'
complete -c ego -n "__fish_ego_using_subcommand rules; and not __fish_seen_subcommand_from ls allow deny revoke remember explain" -f -a "remember" -d 'Derive persistent rules from one prepared builtin call'
complete -c ego -n "__fish_ego_using_subcommand rules; and not __fish_seen_subcommand_from ls allow deny revoke remember explain" -f -a "explain" -d 'Explain one prepared builtin call without running it'
complete -c ego -n "__fish_ego_using_subcommand rules; and __fish_seen_subcommand_from ls" -l profile -r
complete -c ego -n "__fish_ego_using_subcommand rules; and __fish_seen_subcommand_from ls" -l json
complete -c ego -n "__fish_ego_using_subcommand rules; and __fish_seen_subcommand_from ls" -s h -l help -d 'Print help'
complete -c ego -n "__fish_ego_using_subcommand rules; and __fish_seen_subcommand_from allow" -l profile -r
complete -c ego -n "__fish_ego_using_subcommand rules; and __fish_seen_subcommand_from allow" -s h -l help -d 'Print help'
complete -c ego -n "__fish_ego_using_subcommand rules; and __fish_seen_subcommand_from deny" -l profile -r
complete -c ego -n "__fish_ego_using_subcommand rules; and __fish_seen_subcommand_from deny" -s h -l help -d 'Print help'
complete -c ego -n "__fish_ego_using_subcommand rules; and __fish_seen_subcommand_from revoke" -l profile -r
complete -c ego -n "__fish_ego_using_subcommand rules; and __fish_seen_subcommand_from revoke" -s h -l help -d 'Print help'
complete -c ego -n "__fish_ego_using_subcommand rules; and __fish_seen_subcommand_from remember" -l profile -r
complete -c ego -n "__fish_ego_using_subcommand rules; and __fish_seen_subcommand_from remember" -s C -r -F
complete -c ego -n "__fish_ego_using_subcommand rules; and __fish_seen_subcommand_from remember" -l yes
complete -c ego -n "__fish_ego_using_subcommand rules; and __fish_seen_subcommand_from remember" -s h -l help -d 'Print help'
complete -c ego -n "__fish_ego_using_subcommand rules; and __fish_seen_subcommand_from explain" -l profile -r
complete -c ego -n "__fish_ego_using_subcommand rules; and __fish_seen_subcommand_from explain" -s C -r -F
complete -c ego -n "__fish_ego_using_subcommand rules; and __fish_seen_subcommand_from explain" -l json
complete -c ego -n "__fish_ego_using_subcommand rules; and __fish_seen_subcommand_from explain" -s h -l help -d 'Print help'
complete -c ego -n "__fish_ego_using_subcommand mcp-server" -l session-dir -d 'Override the store location' -r -F
complete -c ego -n "__fish_ego_using_subcommand mcp-server" -s C -d 'Run as if started in DIR' -r -F
complete -c ego -n "__fish_ego_using_subcommand mcp-server" -l max-concurrent-runs -r
complete -c ego -n "__fish_ego_using_subcommand mcp-server" -l retained-requests -r
complete -c ego -n "__fish_ego_using_subcommand mcp-server" -l max-resource-bytes -r
complete -c ego -n "__fish_ego_using_subcommand mcp-server" -s h -l help -d 'Print help'
complete -c ego -n "__fish_ego_using_subcommand orchestrator; and not __fish_seen_subcommand_from brief" -s h -l help -d 'Print help'
complete -c ego -n "__fish_ego_using_subcommand orchestrator; and not __fish_seen_subcommand_from brief" -f -a "brief" -d 'Inspect tracked children and unread results without model generation or inbox reads'
complete -c ego -n "__fish_ego_using_subcommand orchestrator; and __fish_seen_subcommand_from brief" -l root -r -F
complete -c ego -n "__fish_ego_using_subcommand orchestrator; and __fish_seen_subcommand_from brief" -s h -l help -d 'Print help'
