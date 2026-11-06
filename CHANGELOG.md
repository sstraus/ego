# Changelog

All notable user-visible changes to ego are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/). Published versions show the tag
date; other versions show the release-preparation date.

Every change that a user, script, or host integration can observe adds an entry
under `Unreleased` in the same commit. A release moves those entries to a dated
version section. `scripts/check-changelog.py` refuses a release when this file is
not complete. The release procedure is in `docs/maintaining.md`, section "Prepare a release".

## [Unreleased]

- Edit up to sixteen existing files with exact original-text chunks. The batch requires admission before effects and reports ordered outcomes and recovery evidence.

- Show the observed token-weighted cache hit rate as a percentage in the default status line. Custom cache countdown templates retain their existing behavior.
- Show preceding prose and valid requested commands before completion, omit empty command rows for partial requests, and preserve answer boundaries across messages, queued turns and cancellation.

- Write one to sixteen complete files with a required `files` array. Each path needs admission before writing; the result records ordered outcomes and stops at the first error.

- Preserve colored headless tool diffs as pre-laid-out diagnostic rows. ANSI styling no longer causes premature hard line breaks in wide terminals; ordinary notices retain their wrapping.

- Read one to sixteen named files with a required `files` array. Results identify each item, retain explicit per-file failures and continuation offsets, and share a serialized 32 KiB budget. First-party read callers use the new request shape.

- Apply the configured permission mode to command and HTTP hook admission. An uncovered command hook can run in Yolo; unresolved approval questions end without an interactive prompt or automatic retry.

- Report a failed prompt-submission hook instead of ending the prompt silently as a successful turn. Keep intentional veto feedback visible.
- Stop the turn when the completion hook fails, instead of repeatedly generating another answer. Intentional completion vetoes still continue with their feedback.
- Treat native task delegation as the default: explicit user or applicable workspace instructions can select another available coordination mechanism while retaining that mechanism's contract.
- Tell TUIC once that ego is busy at every actual turn start, including resumed and automatic turns without a new user message; avoid a duplicate busy signal in the headless opening preamble.

- An extra read root equal to the active workspace no longer blocks authorized builtin writes there, including when a broader writable root is configured. This restores workspace write precedence in ordinary permission modes as well as Yolo.
- With `mcp_tasks` on, an interactive session that waits at its prompt polls its live MCP tasks when they are due. A task that ends starts one generation that carries its result, instead of waiting for the next prompt; a poll that finds the task still working starts none.
- Add `ego rewind ID --at ITEM_ID`: return the conversation and the files to the same turn in one command. It restores, newest first, the file checkpoints that the session's builtin writes recorded after that turn, then forks the conversation after it. Before it changes anything it refuses, and lists the files, when a file changed outside ego after that turn or when a write after the turn is not recorded (for example a `task` child's write). The source session is kept, and the new session's lineage names it. Builtin write, edit and notebook-edit results now record their checkpoint id in the session journal.
- Add an opt-in recap after an absence: with `recap.enabled = true` (a row in `ego setup`; user or system configuration only), the first interactive prompt sent at least `recap.after_minutes` (default 30) after the last completed turn first shows a short recap of the session. The absence counts across restarts. The recap reuses the cached request prefix, shows a usage line, is counted by `/usage` with the main model, and is not added to the session history; a failure is shown and the prompt still runs.
- The generated CLI reference no longer prints "Repeatable Repeatable." for `--allow`, `--deny` and `--oauth-host`: a flag whose help already says Repeatable gets no second mark.
- Add the `checkpoints` configuration key, `ego setup` row and `--no-checkpoints` flag. False (or the flag) stops `write` and `edit` from saving private Git checkpoints in the repository; the default is unchanged: checkpoints stay on. Only user configuration and the command line set it.
- Add the `ego setup` row and `ego config` key `sessions.compress`, and the `--no-session-compression` flag that skips the background compression of old sessions for one run. The default is unchanged: compression stays on.
- Add the `tools` configuration key and `ego setup` row: the same tool names as `--tools`, an empty list for `--no-tools`. It limits the model tools of every run; `--tools` or `--no-tools` overrides it for one run. Only user configuration and the command line set it.
- `ego setup` and `ego config set/get/unset` now manage `cache`, `compaction.idle_margin_seconds`, `compaction.idle_threshold` and `instructions.max_files` as typed rows instead of only naming them; `auth.PROVIDER.protocol_filter` is listed as an advanced row.
- Add the opt-in `lsp` tool: the model asks a language server for the definition of a symbol, its references, or the diagnostics of a file, instead of searching text. Turn it on with the `lsp` row in `ego setup` or `ego config set lsp=true`; only the user configuration can set it, and it is off by default. ego starts `rust-analyzer`, `gopls`, `clangd`, `pyright-langserver` or `typescript-language-server` by file extension, from `PATH`, at the first query, under the run's confinement like an MCP stdio server, and reuses it while ego runs. Only workspace files are answered; an answer lists at most 50 entries and 16 KiB.
- Add `--title TITLE` to `ego resume` and `ego continue`: resume the session of this workspace whose title is exactly TITLE (case-sensitive), for example `ego resume --title "release notes" "next step"`. When several sessions have the title, ego lists them with their ids, most recent first, resumes none and exits 2; no match also exits 2.
- Enroll OAuth services that host login, token or registration on a different origin than the issuer: repeatable `--oauth-host ORIGIN` on `ego mcp add`, or `oauth_hosts` in the server entry. Only configuration enrolls hosts, per server; discovery metadata never adds one. Issuer, resource and audience binding, PKCE, state, the callback issuer check and the public-client requirement still apply. Remove the origin and log in again to revert.
- Add `ego resume ID --queue PROMPT` (also `-f FILE` or `-` for stdin): a script queues a prompt for a saved session without a terminal. It waits while another process (an interactive session, a resume, an ACP connection or an earlier queued prompt) owns the session, then runs the prompt headless with the same admission as `ego resume ID PROMPT` and exits with that turn's status. Queued prompts on one session run one turn at a time, in no guaranteed order. The wait counts toward `--timeout` (exit 124). `--queue` needs a prompt and is refused with `--answer` and with profile `ci`.
- Add the `bedrock` and `vertex` providers for Claude through Amazon Bedrock and Google Cloud Vertex AI. `bedrock` sends the Messages API to `https://bedrock-mantle.REGION.api.aws/anthropic/v1/` with an Amazon Bedrock API key from `AWS_BEARER_TOKEN_BEDROCK`; the region comes from `AWS_REGION`, then `AWS_DEFAULT_REGION`, else `us-east-1`. `vertex` sends to the project in `GOOGLE_CLOUD_PROJECT` and the location in `GOOGLE_CLOUD_LOCATION` (default `global`); `ego auth login vertex` mints the access token from the Google Application Default Credentials that `gcloud auth application-default login` writes (`GOOGLE_APPLICATION_CREDENTIALS` can name the file) and the token is renewed from the same file before it expires; service-account key files are not supported. Both list the current Claude models (Bedrock ids start with `anthropic.`), cache for 5 minutes or 1 hour, are priced from the litellm table, appear in `ego setup`, and keep thinking state on the platform that made it. Server-side web search, IAM SigV4 signing, and Gemini on Vertex AI are not supported.
- Show a diff for every file change. A `write` and a repeated `edit` (`replace_all`) now show the added and removed line counts and the first twelve diff rows under the tool row, and `/expand` shows the full diff. A new file shows as added lines. The diff uses a before image of at most 64 KiB read when the file is replaced; above that size, for a file that is not text, or for a protected credential file, the receipt says the diff is omitted. The `write` receipt now ends with `(+A -R lines)`. The before image is saved in the session journal with the result and is not sent to the model.
- `read` accepts a PDF path and returns its extracted text through the usual 2000-line and 32 KiB pages. A PDF over 20 MiB is refused with its size and the limit, only the first 100 pages are extracted and the result says how many pages were left out, and a malformed PDF is a tool error. The PDF is not sent to the provider as a native document.
- Add `/side QUESTION`: ask a side question while a turn runs. The answer comes from the turn's newest request and context, so a provider with prompt caching reads the cached prefix, and no tool runs. The question and the answer are not added to the session history. A usage line follows the answer, and `/usage` counts the usage with the main model. An idle session refuses `/side` and asks for a normal prompt.
- Add the `exec_monitor` tool: the model can subscribe to new complete output lines of a background command, optionally only lines that match a regular expression. Matching lines enter the model context at the next step, before the command's settlement, and wake an idle interactive session, so the model does not poll `exec_status`. One delivery has at most `max_lines` lines (default 20, at most 100) and 4096 bytes, with lines cut at 512 bytes and later lines counted as omitted. The monitor ends when the command settles or is cancelled.
- Add opt-in MCP Tasks for long tool calls. With `mcp_tasks = true` in the user configuration (or the `mcp_tasks` row in `ego setup`), an interactive terminal session declares the Tasks extension to HTTP MCP servers; a server can then run a long tool as a task. The model gets an answer with the task id at once and the turn continues; ego polls the task at each generation, under the grant of the original call, and gives the model each result once when the task ends. Tasks end with the session, and ego asks the server to cancel each live one; stdio servers, ACP and headless runs never use them, and a task that needs client input is cancelled.
- Add opt-in workspace memory. Turn it on with `ego setup` (row `memory`), `memory = true` in the user configuration, or `--memory` for one run; it is off by default and a project or local configuration file cannot turn it on. When on, one Markdown file per workspace under `$EGO_HOME/memory/` (16 KiB limit) is read at session start and at `/refresh` and sent after the instruction files, and the model gets a `memory` tool to add, replace or remove one entry. Only that tool and you change the file; plan mode refuses a write. A file over the limit is not sent. `--bare` and `--system` send no memory, and with memory off no memory file is read or written. The new `/context` command lists the system prompt sources and the memory state and size. `session.started` lists the memory file with source `memory`, its path, digest and size, never its text.
- Add `compaction.threshold` (integer 1-99, default 90): the percent of the measured context window at which a turn yields and an interactive or headless session compacts. It replaces the fixed 90% value, so the default behavior is unchanged. A configuration where `compaction.idle_threshold` is higher than `compaction.threshold` is refused; note that setting `threshold` below 50 also needs a lower `idle_threshold`. It appears as an advanced row in `ego setup`. Only user and system configuration set it.
- Probe provider network reachability in `ego doctor` and `/doctor`, when you opt in. For each configured provider (a stored credential, or the key variable its row reads) doctor checks DNS, TCP, TLS and one bare unauthenticated HTTP `GET` of the provider endpoint, with each layer bounded at 2 seconds. An unreachable provider is a warning that names the failed layer; any HTTP answer, even 401, counts as reachable. No credential is sent, so the probe never logs in, refreshes a token or spends money. The probe is opt-in: it runs only with `ego doctor --probe-network` or `doctor_network = true` in the user configuration (a row in `ego setup`; a project file cannot set it), and by default doctor makes no network call. `ego doctor --offline` and the offline posture (`network="off"`) skip every probe and win over the opt-in.
- Add the `Interrupt` hook event. It runs once when the user interrupts a turn (for example with `Ctrl+C`), after the turn closes as cancelled, and its input names the interrupted turn. It is an observer: its result cannot resume or change the turn, and a failing handler is only logged. Use it for cleanup and audit scripts.
- Add `effort` to named agent definitions and the config keys `task.model` and `task.effort`, which set a default model and reasoning effort for foreground `task` children. Precedence per field: named definition, `task.*` default, parent. Both keys are unset by default, so children keep inheriting the parent. They appear as advanced rows in `ego setup`. Background tasks and forked skills do not read them.
- Add `--no-hooks`, accepted wherever `--no-tools` is: no hook from configuration or from a skill is admitted or run for that invocation, including in task children, and the configuration is not changed. `session.started` lists `hooks_off` in `capabilities`, and the session records an empty hook catalogue.
- Add `fork: true` to the `task` tool: the child starts from the parent conversation and sends the parent's last request prefix (tools, system, history) followed by its directive, so a provider with prompt caching can read that prefix from cache. The child runs one turn, cannot delegate, and returns only its final answer; its usage is booked to the parent. `fork` with `background` or `agent` is refused. The default stays the clean child.
- Keep the terminal in raw mode while `Ctrl+C` cancels a running turn. Until the turn accepted the cancel, the terminal echoed keys typed after `Ctrl+C` and turned their Enter into a new line, so a command such as `/expand` stayed in the draft instead of running. The cancel still clears the draft and shows `cancelling active work`.
- Add `on_failure = "block" | "allow"` to a hook declaration. A pre-effect hook that fails to start, times out or crashes still blocks the guarded action by default (`block`); `allow` lets the action continue and records the failure in the event stream. The key is refused on hooks that run after the effect.
- Retry a provider capacity refusal (HTTP 429, 503 or 529, or a typed overload or rate-limit error) on the same model after 1, 2 and 5 minutes instead of after seconds, so a temporary refusal no longer stops a long run. A longer provider `Retry-After` wins; a stated wait above 5 minutes stops a finite plan. Headless runs now allow three retries by default instead of two, so the refusal is reported only after the third retry fails; `--retry N` still sets the count and `--retry 0` makes one attempt. Other server errors and connection failures keep the 1, 2, 4 second backoff. The retry notice says `provider at capacity` and shows the wait and the clock time of the next attempt; ACP `providerRetry` adds `capacity`.
- Compact a long headless `ego run` or `ego resume` automatically instead of stopping with `context_overflow` (exit 4), when you turn on `compaction.headless` (off by default; set it in the user configuration or with the `compaction.headless` row in `ego setup`). At 90% of the measured context window, at a known-full window or on a provider context-limit error, the run saves a checkpoint, continues in a clean successor session within the same budget, deadline and `--max-turns`, and reports the successor in its summary. The JSONL stream adds `session_compacted` (`source`, `source_seq`, `target`) before the successor's `session_started`; the public event schema is now version 20. A failed compaction still exits 4 and names `ego compact SOURCE` as the retry. The `ci` profile keeps explicit recovery.
- Compact a long ACP prompt automatically in the same way: the prompt continues in the successor and answers its stop reason. A `session_info_update` with `_meta.ego.compacted` (`sourceSessionId`, `sourceSeq`, `targetSessionId`) names the successor, and the connection then routes requests that name the old session id to the successor. `initialize` advertises `_meta.ego.compacted`.
- Compact an ACP prompt automatically only for a client that accepts the transition: send `clientCapabilities._meta.ego.compacted` = `{"version":1}` in `initialize`. A client that does not keeps the typed context-overflow error and explicit `_ego/compact`, because it could not follow the successor.
- Continue a managed peer that ends its turn without the report its brief requires. When the brief names a parent (`must report ... with agent action=send to="<parent>"`) and the session holds no successful `agent send` to that parent whose message starts with RESULT or BLOCKED, ego adds one reminder turn asking for it, at most twice per session. Sessions without that brief text are unchanged.
- Apply a `Session` or `Permanent` answer to the terminal root question in the same process: when the asking turn ends, ego re-admits the same session with the root, so built-in tools and new child processes reach the exact approved path without a configuration edit or restart. Earlier roots stay. Re-admission waits while a background command or agent runs. On Linux, a new read root asks once to re-admit the changed execution policy. The question now previews the perimeter after approval (workspace, kept roots, new root) and states that it adds no exec grant. The new `/config roots` lists session roots, permanent roots and owner sandbox grants with the revoke command. A session root removed from disk is dropped with a note and does not block later grants or a resume. System root ceilings, exact paths and read-only access still apply.
- Stop offering fast mode for Claude Opus 4.8, directly and through OpenRouter. Anthropic removes it on 2026-11-30, and a fast request then fails with no fallback. Selecting fast on Opus 4.8 is refused with a hint to use Opus 5.5.
- Add an opt-in capture of a provider error that arrives after HTTP status 200: `EGO_LOG=ego::body::provider_failure=debug` with `EGO_LOG_FILE` records the status, the response headers and the last 64 KiB of stream frames once per failed stream, with header values outside a public allowlist and JSON values under secret-like keys redacted. Ordinary `EGO_LOG` levels do not enable it.
- Add opt-in compaction of an idle interactive session shortly before its prompt cache expires, so the compaction reads the context at the cache-read price instead of the next turn writing it again. Turn it on in `ego setup` or with `ego config set compaction.idle=true`. It runs once per cache deadline, only when the TTL is known (sent by ego or set by `cache_ttl_seconds` in the active profile), only when no prompt, question, request, background command or background agent is pending, and only when the measured context is at least `compaction.idle_threshold` percent of the window (1-99, default 50). `compaction.idle_margin_seconds` (default 30) sets the start before expiry. No task continues after it. Only user and system configuration set these keys.

- Show the remaining prompt-cache lifetime of the session in the interactive status (`cache=M:SS`, `cache=H:MM:SS`, `cache=expired`, or `cache=unknown`). The countdown starts at the last provider request with the cache TTL that ego sent, and it resets only where the provider documents renewal on read. Providers with a provider-managed or unknown TTL show `unknown`. The new profile value `cache_ttl_seconds` sets a display-only estimate for those providers; it sends nothing to the provider. Custom status templates can use `{{ cache }}`.
- End the generation with one error when a provider returns a tool call with no id, for every provider. OpenAI Chat Completions-compatible, OpenAI Responses, Codex subscription and xAI rows dropped such a call and finished the turn as a tool use.
- Discard a usage report whose cached input exceeds its input total with one notice for every provider, and settle the generation at its bound. Gemini clamped the figure and reported it as usage, and other providers each had their own notice text.
- Record the tool calls of a response that the provider then reports as failed as not run, for every provider. OpenAI Responses and xAI rows dropped such calls without a record; other rows already recorded them as not run.
- Name the dialled address, its family (IPv4 or IPv6) and whether a proxy was used in a provider connect failure, for example `HTTP connection failed (api.x.ai): dialling 104.18.18.80:443 (IPv4, proxy: no): ...`, so a network error such as ENETDOWN can be attributed to one address. With a proxy the address is the proxy's.
- A local socket endpoint in an ordinary (not 0700) directory is no longer refused: ego leaves that directory unchanged, creates a private child `<socket name>.d` (mode 0700) beside the requested path and reports the actual socket path on stderr (`ego acp: listening on PATH`, `ego a2a: listening on PATH`; every listener endpoint is reported this way). Only the owner can connect. An existing child that is a symlink, a file or not a private directory of the user is refused unchanged; remove it to recover.
- Stop writing each streamed assistant text chunk to `events.jsonl`. The terminal, JSONL output and ACP clients still receive the chunks live; the log keeps only the assembled assistant message, which cuts a recorded 46-chunk turn from 29,300 to 13,908 bytes. Each assistant message completes once, also when the model writes text before and after a tool call. ACP `session/load` sends such a message as one chunk. Logs written by earlier versions still load, replay and export.
- Retry an xAI capacity refusal ("The model is currently at capacity due to high demand") within the bounded retry budget, keeping the provider's stated delay. The refusal stream carries more than one failure frame, and ego previously kept the class of the first, unclassified frame, so the turn ended without a retry. One refusal still shows one notice.
- Show the startup model picker notice (for example `provider 'ollama' has no startup default`) and picker error notices inside the picker frame; they were stored but never drawn.
- Add `ego resume --accept-authority-delta` (also `ego continue`) to resume a saved session whose execution authority changed, including headless jobs. The option allows the change shown in the notice for that one resume and journals it as a party decision; the new measurement becomes the saved authority and a later, different change asks again. Without the option a headless resume of wider authority still exits 3 before any provider call, and administrator ceilings still apply when the configuration loads. Rerun without the option after restoring the configuration to refuse the change.
- Compress cold sessions in the background: after 7 days with no write, `events.jsonl`, `manifest.json` and evidence blobs larger than 4 KiB become zstd `.zst` files (about four times less disk space on real sessions), and every reader handles both forms. Live sessions are never touched; resuming a cold session restores plain files first. Configure with `[sessions] compress_after_days = N`, or turn it off with `compress = false`. Older ego versions cannot read compressed sessions.
- Keep the receipt rows when the terminal is resized right after a line is accepted: the next prompt no longer repaints one row too high over them.

- Ask the Windows unconfined-execution question once per user instead of once per workspace. A `yes` covers every workspace for the current contract revision and is stored in `$EGO_HOME/windows-unconfined.json`; each later start that relies on it prints a notice that execution is unconfined. Direct and piped starts still need `--sandbox off`, `--offline` still refuses, and a `workspace`, `ro` or `adaptive` sandbox still fails on Windows. Delete `windows-unconfined.json` to be asked again. An earlier per-workspace answer in `unconfined.json` is not used, so the first start asks once more.
- Let `web_fetch` open a local or LAN address (`localhost:3000`, LAN documentation, Tailscale `100.x`) that the owner approved: a user `net(...)` rule that names hosts, or a prompt approval of an address literal. Owner rules accept a host without a port (every port), a `*.lan` subdomain pattern, and the new CIDR form `net(192.168.1.0/24)`. Addresses outside the pattern, bare `net(*)`, project rules, `auto`/`yolo` modes, redirects to another endpoint and unapproved names that resolve to private addresses remain refused; a redirect to the same scheme, host and port of an approved endpoint is followed under that approval. Remove the rule to revoke the approval.

- Add the user-only setting `web_private_network = true` (also in `ego setup` and `ego config set`) to turn the `web_fetch` block on local and LAN addresses off for every admitted call. The default keeps the block on; a project or local repository configuration that sets it is refused. Unspecified, broadcast and multicast addresses stay refused.

- Skip the repeated content re-hash of an executable after you approve a call by hand, ego reuses a session approval, or you run a terminal shell command: the check made right after the approval now compares the resolved path and file identity, and the check at the launch hashes the content for every authority. A call allowed by a stored rule or a cached judge decision keeps both content hashes, so a replaced executable is still refused. On a 64 MiB image this saves about 185 ms per approved call. A rule keeps the extra check; there is no setting to restore it for manual answers.

- State, in the remember prompt and the `rules remember` preview, that remembering a read or search of a credential file shows the values of that file, because an exact user rule is the disclosure approval. The `[d]` details, the `[a]` prompt, the ACP "Allow always" prompt and a `DISCLOSES` preview row carry the warning, and `docs/guides/permissions.md` lists every way to grant disclosure. To withdraw it, revoke the rule with `ego rules revoke ID`.
- Show why a credential helper failed: `ego auth login` and token refresh now print the helper's exit code and its bounded stderr (4 KiB), with the request, reply and credential-shaped secrets masked and terminal escapes made visible. `auth.PROVIDER.credential_timeout_secs` (1–3600, default 120) lets a slow browser login finish. Cancellation, the run deadline and process-tree cleanup are unchanged; the helper stays a trusted owner-configured program. Remove the key to restore the 120 s limit.
- Read a symlinked `AGENTS.md` from its canonical target and make the instruction file count configurable (`[instructions] max_files`, at least 1, default 8; user configuration only). A target outside the discovery scope is withheld, with a notice, until the workspace trust answer approves that exact path and digest. Links in parent directories, dangling links, link cycles, oversize files and import cycles stay refused. Remove the link or answer the trust question again to recover.
- `web_fetch` follows up to 5 redirects to the same scheme, host and port in one call, and accepts `max_output_kib` (1 to 256, default 32) for a larger page; the input limit stays 32 times the output limit (8 MiB at most). Each followed hop is a GET with no headers or credentials, uses the network decision that admitted the request, and repeats the public-address check. A redirect to another host, port or scheme, `http` to `https` included, still returns the destination for a new policy-checked call; HTTPS-to-HTTP redirects stay refused, and a sixth hop fails the call. A `net(host)` deny rule blocks the host entirely.
- `ego mcp-server` starts without numeric arguments: `--max-concurrent-runs`, `--retained-requests` and `--max-resource-bytes` default to 1, 16 and 65536. New repeatable `--allow-root DIR`, `--allow-model P/M` and `--allow-mode MODE` let the owner declare additional run targets; the directory from `-C`, the configured model and the configured mode stay allowed. A client request outside that set, or beyond the concurrency, retention and size bounds, is refused as before, and a model, mode or root outside it is never reachable. `ego://capabilities` shows the active policy. Subagent and skill runs inside an MCP run no longer count against `--max-concurrent-runs`. Restart without the `--allow-*` flags to return to the single deployment target.
- Let a project propose extra roots, executable paths and MCP tool grants in a `[grants]` table. The workspace trust question lists them and one answer approves exactly that text; a changed proposal asks again. Approved grants add to your own values, and system ceilings and denies still bind them. Without trust or with `--bare` the proposal is discarded. Delete the table or remove the workspace trust entry to revoke.

- Mask environment secrets by name segment and length: only variables with a name segment (split on `_`, any case) that ends with `TOKEN`, `KEY`, `SECRET` or `PASSWORD` qualify (`API_KEY`, `PGPASSWORD`, `NGROK_AUTHTOKEN`), so `MY_KEYBOARD` and `TOKENIZER` no longer do, and values shorter than 8 bytes are never masked. `AWS_ACCESS_KEY_ID` and `GOOGLE_APPLICATION_CREDENTIALS` and masking across output chunks are unchanged, as is child environment inheritance. To mask a short value, use one of 8 bytes or more.

- Let a trusted project or local configuration select `cache = "long"` (1-hour provider prompt-cache retention) instead of requiring a user-config duplicate. The workspace trust question now names each config file that requests it, and adding it to an already trusted workspace asks again. An untrusted workspace, `--bare` or `bare = true` still discards it and keeps `short`. To revert, answer No at the trust question or remove `cache = "long"` from the project file.

- Compact sessions on models with a 32,768-token window and no declared output maximum, such as the Ollama default, instead of refusing with "The compaction instruction and output reserve cannot fit the 32768-token model window". When the model declares no output maximum, the default response ceiling is now at most half the context window. Set `max_output_tokens` to choose another ceiling.

- Admit sandboxed work when the ego configuration file has hardlinks or a writable root contains `EGO_HOME`, instead of refusing the session. Only writes to the configuration stay denied, including through hardlink aliases inside writable roots; nothing grants them. On Linux, a root that contains the configuration is writable around it: ego cannot create, delete or rename entries directly in the directories on the way to the configuration. Select a workspace outside `EGO_HOME` to restore full write reach.

- Name the cause on stderr when the interactive terminal read fails (`terminal input failed: ...`), also for piped interactive input, or when the terminal cannot enter raw mode or be written (`terminal raw mode failed: ...`, `terminal output failed: ...`), before the session ends, instead of exiting without a message.
- Attach explicitly configured HTTP MCP servers in every sandbox mode, not only with `--sandbox off`; `--offline` still refuses every MCP server. The per-server `allow_insecure_loopback = true` opt-in now admits plaintext HTTP to any named host on the owner network, not only to loopback. HTTPS servers cannot be downgraded, redirects are not followed, and a plaintext OAuth issuer must be on the server's own host. A plaintext server off loopback with `auth = "oauth"` gets one warning per run and per login. Remove the opt-in to refuse the server again.
- Keep one live composer after a terminal height change whose cursor report arrives late. The inline view no longer leaves a stale footer copy and blank rows above the redrawn composer, and growing the terminal again no longer erases the restored transcript.

- Show credential file values after explicit owner approval. An exact user `read(path)` rule returns the file bytes unchanged, including `.ssh`, `.netrc` and JSON token files that had no key-name format, and an exact `search(path)` rule lets `grep` match and show its lines. Without that rule, credential reads and searches still return key names only. Project rules, flags, wildcards, `yolo`, sandbox `off`, session approvals and directory approvals never disclose values, and approval of one file does not disclose a sibling. Revoke the rule with `ego rules revoke ID` to return to key names.

- Honor exact user credential-file read grants in native tools. Sandbox `off` removes built-in credential admission floors; Adaptive keeps default protection against ungranted reads and recursive traversal. Explicit denies still apply. Remove the exact user rule to revoke future approval.

- Use the selected sandbox for authenticated LAN connections instead of forcing `adaptive` and `off` to `workspace`. Public binds still need explicit consent and every TCP connection still needs its bearer token. Hosts remain within the selected ceiling and backend capabilities. Restart with `--sandbox workspace` or `--sandbox ro` to restore a tighter ceiling.

- Honor exact user configuration allow rules for `.git`, `.ego` and `.claude` paths. Project rules and wildcard rules remain excluded; explicit denies, roots, Plan mode and process sandbox limits still apply. Remove the user rule to revoke future rule authority.

- Allow exact owner-approved instruction and Git control file maintenance in the Adaptive sandbox. Approval applies to a later explicit command and never replays the failed command. Directory grants, automatic judge decisions, credentials, installed toolchains and protected ancestor relocation retain their limits.

- Keep explicit process sandbox read, write and execute approvals through compaction and same-session resume after fresh admission. New and forked sessions do not inherit them. Path approval never replays a failed command; judge grants stay write-only. User configuration can adjust residual judge limits.

- Add `--trust-development-programs` to approve edits to current named workspace programs with a notice. New programs, changed declarations, command Markdown and external instruction disclosures still need approval. Use `--trust-workspace` to restore byte-bound trust, or remove the workspace ledger entry to revoke trust.

- Reuse exact owner-approved shell commands within a live session, including across generation policy captures. Changed commands still need their own approval; explicit denies, Plan mode, roots and sandbox limits remain in force.
- Explain command questions with the missing session approval and possible process effects, alongside the existing mode, operation and judge evidence.
- Headless startup with exactly one usable provider that has a default model uses
  that model for the run, names it on stderr, and does not save it. `ego setup`
  shows which providers are ready and how to enable the others, and the Homebrew
  formula prints the first-setup steps.
- Show the startup logo with a speed trail and a magenta → violet → cyan
  gradient. The logo needs 25 columns; narrower terminals and plain output do
  not show it.

- Show the arguments of built-in tool calls as short coloured fields, in the same style as shell commands, instead of raw JSON with empty fields. Glob patterns and grep queries appear as the subject; default values such as an empty search root are left out.
- Colour the added and removed line counts in compact edit receipts.
- Name the images attached to a submitted prompt in the live transcript and when a session is resumed.
- Show the whole submitted prompt. A prompt longer than three rows is no longer shortened to three rows with a "more lines" hint.
- Start reliably in terminals that do not answer the keyboard or cursor queries. ego 0.4.3 could stop after printing its version in some terminals; the keyboard query now has a deadline, and a cursor query with no answer falls back to a fresh bottom row instead of exiting silently with status 0.
- Compact an interactive session when the provider reports that its context window is exhausted, also before ego has measured the context. If the compaction fails, the session stays open and shows a `/compact` retry hint.
- Keep a question of an interactive turn open when a cursor-position reply from the terminal is late or lost. The session no longer exits with status 3 and a `resume --answer` hint.
- Run an MCP connection retry that the ACP client asks for explicitly at once, instead of answering "retry later". Automatic retries keep their backoff.
- Mark the initial ACP session configuration options as pending while sandbox measurement runs, and explain why a configured default is not supported, so a client does not take the missing options as unsupported.
- Use an access token from `OPENAI_CODEX_ACCESS_TOKEN` for `openai-codex` when it is set and not blank. ego never refreshes or stores it and does not read the saved login; an unset or blank variable keeps the saved login.
- Refuse a command name that follows launch flags, for example `ego --profile NAME acp`, with usage exit 2 instead of sending `acp` to the model as a prompt. Put the command first (`ego acp --profile NAME`), or use `ego --plain -- acp` to send the word as a prompt.
- Keep the exit status of the last turn when redirected stdin ends: a provider or stream failure exits 4 and invalid provider input exits 2, instead of 0. On a terminal, closing the session after a refusal still exits 0.
- Retry transient failures in headless runs, provider capacity refusals included, up to two more times by default, and honour the provider's stated delay. `--retry N` replaces this allowance; `--retry 0` makes one attempt. One capacity refusal now prints one notice, not three.
- Stop a runaway model response: at most 128 tool calls per response, at most 8 identical calls in it, and at most 4,096 calls per turn. Above a limit no call of that response runs, and a headless run exits 7 with reason `guard`.
- Refuse tool calls whose arguments contain a raw model control token such as `<|control28|>` before they run. No call of that response runs, and a headless run exits 7 with reason `guard`.
- Return a call to a tool that does not exist to the model as a failed tool result, so the model can correct itself, instead of ending the turn with "provider returned undeclared tool name".
- Show in `ego usage` and `/usage` an estimate of how much context each tool's requests and results use, and which reads repeat earlier reads, with the events they come from. The estimates do not change charges or the session history. The JSON report version is now 5.


## [0.4.3] - 2026-11-01

### Added

- Configure a user-owned provider protocol filter that transforms outgoing
  generation bodies and incoming streamed bytes. Filter failure stops generation;
  ego never falls back to unfiltered traffic.
- Grant an extra filesystem root from the owning terminal for the session, or
  save it permanently in the selected user profile. Revoke it with
  `/config roots revoke EXACT_PATH`.
- Send richer native terminal events to TUICommander for tools, child activity,
  turn outcomes, and permission or question prompts, without private task text,
  tool arguments, results, or answers.
- Let ACP v1 clients opt in to native child lifecycle metadata. Load and resume
  return the current durable child roster.
- Resume the most recent readable session in the workspace with
  `ego --continua`.
- Open selectors with bare `/mcp` and `/skills`, and group startup MCP notices
  in the terminal.

### Changed

- **Breaking:** the public event schema advances from 18 to 19 with typed child
  lifecycle events. ACP stays at protocol version 1.

### Fixed

- Keep Ctrl+Enter distinct from Enter through the Kitty keyboard protocol, and
  release keyboard modes before Ctrl+Z suspension.
- Recover abandoned workspace execution-probe fixtures during later capability
  measurements.
- Keep terminal output after a matched setup marker, so a later setup wait does
  not miss its marker.
- Keep submitted prompts and later input live when cursor-position replies are
  missing after initialization, including full prompt receipts that scroll beyond
  the screen. Preserve input through a provider retry and background completion
  wake.
- Exit ACP on SIGTERM and settle an active turn before shutdown.

## [0.4.2] - 2026-11-01

### Added

- Run independent subagent work in parallel. An idle interactive parent wakes
  when a background subagent settles.
- Start the requested work when you set an interactive goal, and extend an active
  goal with `/goal extend N` or `--goal-operation extend=N`.
- Run a local shell command from the idle prompt with `!COMMAND`; its output
  becomes context for the next turn. Use `!!` for a literal `!`.
- Open anchored history results and follow continuation pages back to the
  original session evidence.
- Show inline diffs for successful unique edits, resolve relative Markdown links
  against the run workspace, and show useful rows in compact process previews.
- Guide user-default and profile configuration through terminal setup, with
  review and confirmation before an atomic save.
- Add a bounded skill that reviews harness changelogs.

### Changed

- **Breaking:** license ego under Apache-2.0.
- **Breaking:** interactive setup no longer accepts a prompt argument; use
  `ego setup --profile NAME --directory PATH`.
- Publish the user guide on GitHub Pages and stop advertising the removed
  `/memory` command.

### Fixed

- Keep the Codex request prefix stable by appending hook-context changes after
  history. This removes a source of prompt-cache invalidation.
- Keep Expand responsive while a turn finishes output, and keep the last
  terminal row free below the prompt.
- Label a stopped goal continuation as paused, and strike only the recovered
  cause in a provider-error receipt.
- Keep both the canonical executable target and its invoked name, and fix the
  ACP write-scoped workspace probe.
- Accept empty unbound plan steps.


## [0.4.1] - 2026-10-30

### Fixed

- Stop reporting a worktree as in use after the ego process that held it ended.
- Release the session writer lock before its file closes on Unix, so reopening a
  session in the same process no longer reports a stale lock.


## [0.4.0] - 2026-10-30

### Added

- Show the number of hidden rows and the expansion shortcut below collapsed
  tool results; edits show actual added and removed line totals.
- Pin explicit session constraints with `/pin <text>`, list them with `/pin`,
  and remove one with `/unpin <n>`. Pins survive restart, compaction, and forks.
- Report observed context capacity and provider rate limits in
  `ego usage --json`.
- Send provider HTTPS through one explicit HTTP CONNECT proxy with `--proxy` and
  `--proxy-ca`. Ambient proxy variables are ignored.
- Give foreground `exec` calls a default two-minute limit; `timeout_ms` accepts
  up to ten minutes.
- Show one activity row for each live background task, also while the parent is
  idle.
- Colour the command syntax in `exec` receipts.
- Show the braille startup logo above the identity line.
- Pass lifecycle input and bounded external context to hooks.
- Ship and install generated shell completions with release archives.

### Changed

- **Breaking:** the public event schema advances from 16 to 18 and adds the
  durable `CostAnomaly` event.
- **Breaking:** the usage-report schema advances from 3 to 4 with context
  capacity and provider-limit observations.
- **Breaking:** collapsed transcript text includes hidden-row hints.
- Continue generations when prices are unknown, with one cost-anomaly notice per
  session and model. Monetary caps cover recorded spend only.
- Remove private coordinator and owner defaults from the shipped configuration.

### Fixed

- Keep headless resume free of implicit interactive retry defaults, and restore
  implicit retry authority when a session resumes on a terminal.
- Exclude manifest-only and unreadable sessions from the session catalogue, and
  name the backing store of a missing ACP session.
- Run advertised ACP skill commands through the shared skill-command path.
- Append host-context changes instead of repeated full inventories, and admit
  the same skill instructions once.
- Omit empty process-stream labels from expanded tool output.
- Explain the read-write root grant and the exit and resume needed for writes
  outside the workspace.
- Do not cache failed filesystem-capability probes, and complete deferred ACP
  configuration offers with an error when measurement fails.
- Persist a successful in-session model switch.
- Keep the latest user request verbatim in compaction checkpoints, and render
  fork constraints once.
- Keep unknown slash-prefixed text as a modal answer.
- Keep prompt admission after a late cursor reply, and keep the current activity
  when a modal closes.
- On macOS, admit the Apple launcher runtime in the sandbox and ask for
  permission on Seatbelt selector denials.
- Quote outside-root read hints so they can be pasted, and offer an exact read
  grant for a denied shell script read.
- Name backend revision changes in the consent prompt.
- Make content search refuse special files and bound single-file reads.
- Do not give `pnpm` an unsafe optional lock directory, and give a racing
  background promotion and foreground completion one owner.
- Accept narrowed OAuth scope grants at MCP login, and ignore unused secrets
  for explicit public dynamic client registration.
- Retry interrupted narration once on interactive terminals.
- Emit TUICommander marker boundaries only for the TUIC profile.
- Bound independent `ego doctor` health probes, and avoid schema-only claims for
  generic task failures.

### Security

- Redact encoded Basic credentials while keeping ordinary prose that mentions
  the Basic or Bearer schemes.
- Update dependencies for RUSTSEC-2026-0381, -0382, -0383, -0385 and -0414, and
  replace the yanked `clipboard-win` version.


## [0.3.0] - 2026-10-26

### Added

- Log in to an HTTP MCP server with `/mcp login NAME`; its tools reconnect in the
  current session.
- Show a separate activity row for each running task or named subagent.
- Build five stable executable assets, target-triple archives, SHA256 files, and
  a Homebrew formula. The source installer replaces the executable atomically.

### Changed

- **Breaking:** the public event schema advances from 15 to 16 with the transient
  `ChildProgress` event.
- **Breaking:** `notebook_edit`, `plan`, and `skill` advertise flat object
  schemas, so providers that reject root union schemas accept the catalogue.

### Fixed

- Wake an idle interactive session when background work settles, including
  cancelled work.
- Read skill resources relative to the captured skill root.
- Keep sanitized upstream error details in diagnostics.
- Keep a configured Anthropic gateway's base-path prefix.
- Accept public OAuth dynamic-registration replies without an authentication
  method.
- Let confined processes signal their own descendants.
- Recognize Python script read denials in the sandbox grant flow.
- Clear settled interrupt and queue indicators from the status row.
- Use plain next-step wording for refusals and login notices.


## [0.2.0] - 2026-10-25

### Changed

- Start the 0.2 version line. This version has no functional changes from 0.1.6.

## [0.1.6] - 2026-10-25

### Changed

- Align the tests for refused tool calls with the boundary continuation. This
  version has no functional changes.

## [0.1.5] - 2026-10-25

### Fixed

- Keep completed turns and deadline arbitration through deadline cleanup.
- Report unreadable sessions in history search instead of aborting the search.
- Keep private summary occupancy out of published compaction events.
- Keep sandbox exec recovery visible, and separate script-read refusals from
  launch refusals.
- Admit macOS runtime selectors and scratch ancestors in the Workspace sandbox.
- Keep derived Git writes beneath read roots.
- Keep admitted launch selections and readable consent on resume.

## [0.1.4] - 2026-10-25

### Added

- Withdraw queued prompts from the first composer line.

### Fixed

- Recover automatic compaction through shared retries, and fit over-window
  compaction requests.
- Release the composer after a provider failure, and keep the failure cause and
  host signal.
- Retry opaque and unrun transient attempts once, and return boundary refusals
  to the model.
- Ask the native terminal for session path grants, and revoke terminal asking on
  exec promotion.
- Use static, confined `pnpm` runtime grants and keep consent for nested custom
  stores.
- Report read-only root execution restrictions, and keep workspace executable
  denials and PATH discovery for MCP servers.
- Redact secrets in encoded JSON and percent-encoded evidence.
- Keep user authority steers and task authority in judge evidence, and disclose
  MCP effects to the automatic judge.
- Remember program rules from shell compounds, and label write risk from the
  canonical path.
- Search a single file with grep.
- Parse slash commands while a question is open.

## [0.1.3] - 2026-10-23

### Added

- Render configurable MiniJinja status-line templates.
- Select permission modes and scope manual session consent.
- Support explicit replace-all edits.
- Add capability-checked durable audio input over ACP.
- Render plan updates as one compact block, pin running plans above the working
  line, and replace consecutive compact shell rows in place.
- Colour retry state and recovery receipts.
- Publish a lease-owned per-process session index.
- Distinguish steering and next-turn counts in the status row.

### Fixed

- Keep sessions after transient compaction failures, stop after a required
  compaction fails, and explain automatic compaction.
- Explain unsupported capability schemas on resume, and keep session identity in
  unreadable payload diagnostics.
- Repaint the terminal fully across live activity changes, and keep the composer
  during status row updates.
- Keep slash completion open while erasing a partial command.
- Keep images with drafts while turns run.
- Keep quit responsive by batching queued output.
- Reconnect definitely-unsent reused Codex sockets once, and use system trust
  for Codex WebSocket TLS.
- Identify conflicting MCP registry tool names, and keep collapsed tool
  descriptors stable.
- Isolate the model inventory by authenticated request, and keep the startup
  picker on read-only discovery.
- Recall queued steers, normalize Read paths, and deliver bounded memory recall.
- Read only the requested session from the catalogue, and speed up session
  store appends and followed exports.
- Admit nested read-only launch grants in the sandbox.
- Identify busy work in action refusals.
- Keep pending input across cursor queries.
- Keep exec handler results after an early stdin close, and keep owned sockets.
- Leave answer prose wrapping to the viewer.
- Read the workspace version when checking release tags.


## [0.1.0] - 2026-10-18

First versioned build. ego is a provider-agnostic coding agent harness:

### Added

- Interactive terminal conversations (`ego`), headless runs (`ego run`) with
  text, JSON, or live JSONL output, and an ACP server (`ego acp`) for hosts such
  as TUICommander.
- Read, search, edit, write, and execute through prepared operations and
  permits, with separate permission modes and operating-system confinement
  (Seatbelt on macOS, Linux confinement backends).
- Durable sessions: resume, continue, fork at a completed message, compact,
  export, and restore file checkpoints.
- One model catalogue for local, hosted API, and subscription providers.
- Bounds on tokens, money, turns, and time, with retries only for
  delivery-safe failures.
- Schema-validated answers and versioned structured output for scripts.
- External MCP tools, scoped runs served over MCP and ACP, and local A2A
  discovery.
- Named skills and lifecycle hooks under the same authority boundaries.
- Inspection of configuration, effective perimeter, usage, and redacted health
  facts with `ego config`, `ego usage`, and `ego doctor`.
