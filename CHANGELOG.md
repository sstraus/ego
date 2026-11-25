# Changelog

All notable user-visible changes to ego are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/). Published versions show the tag
date (UTC); other versions show the release-preparation date.

Every change that a user, script, or host integration can observe adds an entry
under `Unreleased` in the same commit. A release moves those entries to a dated
version section. `scripts/check-changelog.py` refuses a release when this file is
not complete. An `<!-- internal: ... -->` comment in a version section lists the
story IDs of feat/fix commits that have no user-visible effect, such as test or
conformance repairs, or that an earlier version section already lists. The release procedure is in `docs/maintaining.md`, section "Prepare a release".

## [Unreleased]

## [0.4.20] - 2026-11-25

<!-- internal: #732-5e09 is already covered in 0.4.19. -->

### Added

- Turn `!` red when you type a space after it at the start of a single-line terminal prompt, before Enter runs the local shell command (#755-2ff5).

- Review skill instructions and resources with an isolated model request before first use. Report the result and ask before allowing high-risk content. `--no-skill-safety-check` skips only the review.

- Show skill origins in slash completion and `/skills`, with muted badges on true-color terminals and readable labels without color.

- Discover project skills in `.github/skills` after workspace approval. `/skills diagnose` reports missing or empty roots, load errors, and which definition wins when names match.

- Resume an interactive HTTP MCP task after the operator answers its form, without repeating the original tool call (#1206-4d09).

### Fixed

- Open a fresh provider connection after a root approval instead of reusing a connection that may have closed while waiting for the answer.
- Keep the startup trust answer on the same line as `[y/N]` when a long workspace path fills the terminal row.
- Preserve the `/side` cache prefix when the active turn offers provider-hosted web search. `/side` can use that search but never runs local tools (#867-af6f).
- Start subagents on the first valid launch request instead of discarding the call and asking the model to repeat it.

## [0.4.19] - 2026-11-25

<!-- internal: 1152-8424 -->

### Added

- Configure Azure OpenAI v1 deployments with explicit Chat or Responses mappings and resource-bound API key or Entra credentials (#1154-6000).
- Configure Gemini 4 Argon on Google Cloud Vertex AI with an explicit project and location and a named OAuth connection. Named OAuth refresh uses the connection route’s credential source, and Vertex Gemini requests carry its configured auth headers (#1155-78d9).
- Support configured Mistral Large 3 Chat Completions on the us-east-1 Bedrock Runtime and explicit SigV4 authentication for regional runtime instances (#1156-68e1).
- Group independent MCP lookups into one model call on routes limited to one tool call per response. Ego runs the lookups in order and checks permission for each one (#1188-612f).
- Add configurable Moonshot Responses and Mistral Conversations contracts for hosted Web Search. Offer explicit search on Z.ai Chat and on DeepSeek Chat without local function tools (#1137-50b5).
- Show the last three lines of foreground command output below `Working` while the command runs.
- Keep one headless session open with `--input-format jsonl`: read prompt objects from stdin in order and emit a result after each turn with `--output jsonl` (#1182-bb0a).
- Fork a saved session and run a prompt in the fork with `ego resume ID --fork PROMPT` (#1181-f262).

### Changed

- Report the phase, category, and next action for provider protocol filter failures in the CLI and session journal without including child stderr or filter payloads (#1203-1901).
- Clarify current saved-account commands and native search limits. Use `--account N` with the provider, rename via `connections.ID.name`, and sign in again instead of using the historical `auth migrate` command. Native search limits on OpenAI subscription and xAI Responses are best effort, as described under #1204-3001 (#1117-f125).
- Document Mistral and Moonshot API-key routes, deployment facts and pricing for configured gateways and local servers, and provider-specific exclusions (#1153-cd2b).
- Let a new Codex session read the cached system prompt on its first request: sessions with the same model, system text and tools now share one cache routing key (#1177-f2d8).
- Report the remaining xAI request and token limits from response headers, as for OpenAI API keys (#1066-3552).

### Fixed

- Compact a session under the sandbox it was started with, as resume does. `ego compact`, ACP `_ego/compact` and MCP compaction used the configured default instead, so on Windows a session started with `--sandbox off` could not be compacted (#732-5e09).
- Report an instruction path that is a directory as not a file on Windows, not as unreadable (#732-5e09).
- Treat an `OLLAMA_HOST` bind address (`0.0.0.0` or `[::]`, with or without scheme and port) as the local machine and connect to `127.0.0.1` or `[::1]`. Other hosts are unchanged.
- Report a protocol filter that reads a request, crashes and exits non-zero as an exit failure rather than early EOF, still without child stderr (#1203-1901).
- Name the right fault in protocol filter diagnostics: an empty filtered request body is an invalid result, not an output limit; a local journal or notice-sink failure is a `host` failure, not the filter's; an egress start failure says `launch`; and a failing response filter no longer journals a provider stream error that did not happen (#1203-1901).
- Send the native search budget as an agentic turn limit to xAI Responses and stop reading OpenAI subscription responses when a search exceeds the budget. Neither route guarantees a cap on individual provider searches or charges (#1204-3001).
- Count only search actions against the native search budget, so a page open or in-page find no longer cuts an answer that used exactly the budgeted searches. A search-budget cutoff now exits with the budget code (5) and the new `local_limit` error kind instead of the usage code (2) (#1204-3001).
- ACP sends every ego notice (cost anomaly, hook error, replayed failure, execution policy notice and out-of-turn card) as `session_info_update` with `_meta.ego.notice`, not as agent message text. A host that reads the answer from `agent_message_chunk` now gets only the model's words; hosts that show cards read `_meta.ego.notice` (#1202-c67e).
- Offer the models of every signed-in provider in the ACP model option when the session is bound to a saved account. A pick on another provider before the first prompt moves the session to that provider's account. After the first prompt, the pick is refused with a `model` control error that says to start a new session (#1201-2022).
- Return `Cancelled` or `RunTimeout` from a configured protocol filter that is cancelled or reaches the run deadline while a request is being sent to it. Under load the request could wait forever (#1198-5949).
- Resolve the new provider’s account when resuming with an explicit model from another provider. Keep the saved account for model changes within the same provider. If account selection is ambiguous, the error shows how to retry with an account number (#1193-6939).
- Require explicit kernel-backed UDP isolation evidence before offline admission and try another local address when a host control fails, including on VPN hosts (#1191-91e1).
- Keep the unconfined UDP control alive until the host confirms delivery, so offline admission tolerates delayed local packet delivery.
- Report the original session-store error when a failed append leaves the journal unable to finalize. The error identifies the failed operation and explains how to reopen the saved session after resolving the cause (#1190-7d39).
- Withhold configured Bedrock SigV4 credential variables from built-in exec children and keep Vertex Gemini ADC refresh bound to the Google account verified at login. Existing named Vertex Gemini ADC connections require a new login with `ego auth --connection CONNECTION_ID login` to establish the binding (#1117-f125).
- Show configured provider readiness in setup using the instance's named credential source (#1157-e8a0).
- Reject Azure Entra helper options that can override the deployment resource or token scope, and report that Azure OpenAI subscription usage is unavailable (#1154-6000).
- Omit repeated execution policy details from terminal root approvals when both choices leave the measured policy unchanged (#1205-14dd).
- Preserve a resumed MCP result or follow-up question if Ego exits before it can close the continuation. Reopening uses the recorded outcome without issuing the MCP call again (#1188-612f).
- Encode Bedrock Converse model IDs before SigV4 signing and redact signing credentials from provider errors (#1156-68e1).
- Explain HTTP MCP authentication and access refusals to the model. Name the HTTP status, direct credential or access checks to the operator, and warn that the failed tool call may still have taken effect (#1172-1c1d).
- Resume suspended stdio MCP tool calls across CLI restarts. Ego checks the executable again before using the saved permission and sends the answer in the protocol's continuation fields (#1188-612f).
- Refuse a Windows path that names an NTFS alternate data stream (a `:` after the drive prefix) for read, write, glob and grep. `.git::$DATA` or `a.exe:x.md` passed the write rules as written, but NTFS created the base file `.git` or `a.exe`. The error names the path and says streams are not supported (#1189-c213).
- Save remembered and Keep blocked path rules on Windows in the form that matches the path. They were written with the drive spelling and never applied, so the same question came back (#1179-f858).
- Pass standard Windows paths to Git when creating and cleaning up an owned worktree, and run the removal from the source repository so Windows can delete the checkout (#1180-cdb5).
- Keep background task activity visible when the elapsed counter grows and there is room without the task name (#1187-4cd5).
- Apply absolute permission rules on Windows. A pattern that starts with `/` now covers that path on every drive and UNC share, and matching ignores case as NTFS does, so the default credential denies (`.ssh`, `.aws`, `.env*`, `.netrc`) refuse those reads on Windows too. Before, no absolute rule matched a Windows path (#1179-f858).
- Keep the prompt cache of a session when a skill is installed or removed: a resume or fork sends the skill list of its first request again, and the model gets the added and removed skills after the history (#1183-2815).
- Keep the prompt cache of a session that delegated work when it is forked: the fork sends the source's system prompt unchanged, including the coordinator role (#1184-063f).
- Let `ego worktree` record its receipt on Windows. Flushing the receipt directory no longer fails with `Access is denied` (#732-5e09).
- Make `/goal stop` pause all automatic work from the admitted request: no worklist or report continuation starts after it, and the running reply cannot create a new goal, whichever moment the stop lands. Worklist rows and running tools stay; a new user request or an explicit goal enables automatic work again (#1167-2801).
- Admit multiple complete tool schemas from one collapsed MCP search, and keep search results available when an inline schema is invalid or exceeds admission limits.
- Keep optional tool parameters optional on OpenAI and Codex Responses requests. The endpoint no longer makes every parameter required, so the model can send a completion report without the `question` field that only a blocked report accepts. An MCP call that does not match the tool schema now names the tool, says the server was not called, and tells the model to correct the named fields instead of retrying unchanged (#1172-1c1d).
- Tell the model what happened when an MCP call fails in transit: the tool and server, the cause in plain words, and whether the tool may have run. A server that exits mid-call, a lost connection and an HTTP error answer such as 429 or 503 now keep the status, the `Retry-After` value and the server's own text, instead of `Transport closed` or a JSON deserialisation error (#1172-1c1d).
- Offer native Web Search by default on official OpenAI Responses, Anthropic Messages, and xAI Responses routes without filtering by model name (#1137-50b5).
- Tell the model when an MCP server could not connect or complete discovery and no tools from it were admitted. The notice explains the failure without passing upstream response text to the model (#1172-1c1d).
- Let native background tasks use a unique saved account for an explicitly selected provider other than the parent provider (#1185-4789).
- Keep the session ID when switching providers before the first prompt. For a settled conversation, show the destination account and configured endpoint before asking to share history, admit the destination before opening the fork, and report how to resume the original. Wait for pending work before switching and run the post-switch hook on success (#1186-ba6c).

## [0.4.18] - 2026-11-20

### Fixed

- Read the system prompt from the prompt cache on Codex GPT-6.2 Sol: send it as the first developer input item with a short constant `instructions` field, so new sessions no longer resend 10-16k uncached tokens on every request. (#1175-d721)
- Show known models from other providers in `/model` and the session catalogue when a saved account binds the session, with guidance to open a new session to use them. (#1157-e8a0)

## [0.4.17] - 2026-11-20

### Fixed

- Omit the cost from the terminal status bar when no price or estimate is available; keep the pricing status in `/usage`.
- Show a Standard API cost estimate for new GPT-6.2 Sol Codex usage without treating it as a subscription charge.

- Show and allow editing the numeric API-key/OAuth login-method choice before Enter, while keeping subsequent API-key input hidden. (#1165-5e9f)

- Tell the Auto permission judge that `allow` can trigger immediate execution and that review-only requests do not grant execution authority. (#1174-2ca0)

## [0.4.16] - 2026-11-20

### Fixed

- Request provider-supported prompt-cache reuse for goal checks and Auto permission analysis, keeping their routing separate from the main conversation. (#1173-0ba7, #1174-2ca0)

- Stop a turn when it retries a tool after seven consecutive failed outcomes, even when the model changes arguments or the error text changes.

- Report an OAuth account as missing in credential status, readiness/startup discovery and model/catalogue projections when its saved entry is an API key, matching the generation resolver without falling back to another source. (#1165-5e9f)

- Keep MCP inventory available inside an open question or permission prompt without answering it, render permission List/Explain inspections during active replies, and show an explicit refusal for Init/Setup shortcuts instead of silently dropping them. Explicit Allow/Deny/Revoke controls publish and stage next-generation policy during the reply without changing its captured authority; Remember retains confirmation ownership. Session name set/clear also applies during the reply through the existing leased journal owner, without a model prompt. Export renders sanitized or local Markdown/JSON/JSONL from the committed session prefix during a reply, without writing a file or starting another generation. Model/thinking selection runs its existing hooks and durable future-generation publication beside the active reply, preserving its captured request and account restrictions. Bare `/login` provider inventory also renders during a reply through the existing login owner; provider authorization, Setup and Init remain unavailable while busy. (#1169-f859)

## [0.4.15] - 2026-11-20

### Added

- Select saved provider accounts by stable number with `--account N` or from `/model`. A provider with one account needs no selector, and removing an account never renumbers the others. (#1117-f125)
- Stop goal continuation immediately with `/goal stop` during replies or goal checks, preserving the session, current work and durable history. (#1167-2801)
- Let the model create and explicitly cancel automatic (model-owned) goals by exact identity, without cancelling operator goals or claiming achievement. (#1168-7017)
- Register API keys through `ego auth login PROVIDER` with hidden terminal input and private named credential storage, without importing an ambient key or changing environment-source connections. Preserve OAuth login and explicit method selection. (#1165-5e9f)

### Removed

- **Breaking:** Remove compatibility with provider-keyed saved logins. Sign in again to create a named account; old credentials are left untouched. (#1117-f125)

### Fixed

- Background tasks inherit the exact saved account chosen in `/model`, including accounts saved after the session started. (#1117-f125)

- Keep native catalogue inspection, future-session config saves, per-session fast selection, next-generation mode changes and session-owned task cancellation responsive during active replies; preserve captured authority and genuine modal/session conflicts. (#1169-f859)

- Enforce one direct active worklist action, require real live task ownership for additional active rows, and retain started identity and suspension causes across updates and session seeds. (#1171-7ede)
- Select models backed by a saved API key from `/model` without restarting an unbound session. Confirm the numbered account when several match, and keep an already-bound session on its existing account. (#1165-5e9f)

- Show `Compacting` during manual and automatic context compaction instead of the generic `Working` activity label, retaining elapsed time and interrupt controls. (#1170-b044)
- Keep ACP sessions usable when a stronger optional sandbox cannot be configured. Continue refusing an invalid selected sandbox without falling back to a weaker one. (#1164-5260)
- Restore the README's concise command-family overview alongside its runnable quickstart examples. (#1166-5299)

## [0.4.14] - 2026-11-19

<!-- internal: #1153-cd2b — Let conformance diagnostic paths traverse native error arrays while preserving object keys and explicit malformed-path refusals; no new provider admission. -->

### Changed

- Shorten tool file labels relative to the active workspace, with a home-relative `~` fallback outside it. Keep live output and replay aligned across workspace switches without changing actual arguments, queries or access checks. (#1161-ef37)

### Removed

- Remove the `ego --continua` flag; use `ego continue` to resume a session (#1162-2b2c).

### Fixed

- Hook matchers written as permission rules, such as `exec(git *)` or `read(src/**)`, now run for `PreToolUse`, `PermissionRequest`, `PermissionDenied`, `PostToolUse` and `PostToolUseFailure` when the rule covers the tool call, and not for other calls. Before, they never ran. A matcher with the tool name keeps matching every call of that tool (#1163-d71e).

## [0.4.13] - 2026-11-19

### Changed

- Refresh the builtin provider catalogue with GPT-6.2 Astra and Sol for API and subscription routes, plus the newly published hosted-plan models and updated Kimi limits. Preserve account selection, startup defaults and conservative backend limits. (#1157-e8a0)

### Added

- Images returned by MCP tools, including asynchronous task results, now reach the model as images instead of text. They are stored with the session and replay unchanged.

### Fixed

- Runs with a named connection no longer use the old subscription login for image generation. Private history of a configured provider instance is replayed only to the connection that produced it; history from another or an unknown connection is refused before the request (#1117-f125).

- After you approve a root, the interrupted request now continues even when its turn was not yet finished. An approval still applies once and never to a completed turn; a refused approval names what did not match without showing permission values (#1148-01fc).

- Keep other supplied providers visible in the interactive model picker when changing models, while preserving explicit named-connection routing and captured account isolation (#1157-e8a0).

- Tool images that a model cannot accept, or that exceed its image limit, are replaced by a short note in the request, also for recap and compaction; the newest images that fit are kept in order and the originals stay in the session. Text returned by a tool stays inside its tool result, and every tool result of a batch comes before its images. Images you attach are still checked strictly.

## [0.4.12] - 2026-11-18

### Added

- Add an explicit native `bedrock-runtime` / `bedrock-converse` configured contract with bearer API-key presentation, binary eventstream for tool-free turns and synchronous Converse for tool turns. Require explicit model facts without inventing discovery, default models or prices (#1156-68e1).

- Add Z.ai (GLM-5.3, `zai`) and Moonshot Kimi API (K3.1, `kimi-api`) as API-key providers (`ZAI_API_KEY`, `MOONSHOT_API_KEY`), with their exact context limits and effort controls. Reasoning and tool calls are replayed only to the same provider; unsupported controls are refused, and these providers never use a subscription login (#1151-1bf7, #1152-8424, #1153-cd2b).

- Configured compatible endpoints that need `max_tokens` instead of `max_completion_tokens` can declare the `chat-max-tokens` contract (text and tools, no reasoning). Its model limits must be declared; requests to OpenAI itself are unchanged (#1151-1bf7).

- Add trusted provider instances with independent endpoint/credential references, explicit model aliases and contracts, strict model facts, and atomic expected-value configuration updates. A running session keeps the instance settings it started with; authentication methods that custom instances do not support stay unavailable (#1150-9950).

### Fixed

- Preserve assistant-declared contextual tab titles across new user and automatic messages by removing the synthetic turn-start intent that reset the title to the product name.

- Before you approve a root, ego now shows exactly which permissions change. The approval applies once, to that session, pending call and unfinished turn; if the permissions changed in the meantime or cannot be measured, the approval is refused and grants nothing further (#1148-01fc).

- Render opaque web citation controls as readable source references in terminal Markdown, preserving real source links, code literals, and byte-for-byte redirected output.

- Configured provider instances now appear in `ego models`, setup readiness and ACP/MCP sessions. A running ACP session or MCP server keeps the endpoint, credential, model alias and limits it started with after you edit the configuration; named accounts behave as before (#1157-e8a0).

- DeepSeek tool continuations no longer fail after reasoning display was enabled: only the exact `reasoning_content` is sent back to DeepSeek. Other chat-completions routes are unchanged (#1151-1bf7, #1152-8424).

- Align the `worklist` tool's declared schema and runtime validation: require all four input fields, use explicit `null` for fields inactive in the selected operation, and reject invalid or unexpected fields without changing the worklist or goal. Keep durable typed updates and replay unchanged (#1160-f930).

- A prompt sent just as a turn ends now shows exactly one complete receipt and runs as the next turn. Multiline input, other drafts and recall of queued messages are unchanged (#1159-711a).

- `web_search` now always means the provider's native search, also after a model change, in child agents and on resume. Select `web_search_external` for independent Exa search; a model without native search never falls back to another search service or credential. This replaces the 0.4.11 behaviour in which an explicit `--tools web_search` used Exa (#1135-5f22).

- Resume keeps the tools selected on the command line, and a session picked from the resume list gets back its saved tools instead of the launching command's tool options; switching sessions in the same workspace keeps the active model (#1135-5f22).

- The consent prompt for paid search no longer presents itself as a reusable web fetch approval (#1135-5f22).

- Keep the provider retry countdown visible before long error causes, including narrow terminals and live resizing, without changing retry delays or headless diagnostics (#1158-2076).

<!-- internal: #1149-ef5e reconciles the validated provider-definition path already delivered by #1110-7c05. -->

<!-- internal: The following changes are already documented in the historical 0.4.11 section; the history gate starts at its early version bump.
#1030-4fed #1046-f438 #1047-3e8c #1054-d371 #1055-a043 #1056-4ddc #1057-ba1f #1060-85d0
#1061-1934 #1063-c613 #1064-1e4e #1065-00ec #1066-3552 #1067-79fc #1068-2001 #1069-df69
#1070-56ed #1071-2262 #1072-3357 #1073-898a #1074-0083 #1075-ac44 #1076-c308 #1077-e96a
#1078-09c6 #1079-6d59 #1080-7d8a #1082-7c09 #1084-8bb4 #1085-50bf #1086-5a53 #1087-3af6
#1088-d15c #1089-c34a #1090-fccf #1091-c91a #1096-9e49 #1097-7bbc #1098-5383 #1099-0ff9
#1100-e872 #1101-5c9f #1102-a6de #1103-5fed #1104-603e #1106-6da4 #1107-03c6 #1109-dffd
#1111-bfa0 #1112-a347 #1113-d8c0 #1114-db9c #1115-05a5 #1119-f27f #1124-054f #1125-391a
#1132-225c #1136-25e6 #1137-50b5 #1139-7d31 #1140-2eb0 #1141-00e2 #1142-773e #1143-a0d0
#1144-6e73
-->

## [0.4.11] - 2026-11-17

### Added

- Add versioned `ego auth status --json` with explicit legacy/vendor and named-account presence, including missing sources and orphaned slots. Keep human status output unchanged and omit secret metadata from JSON (#1143-a0d0).

- Expose execution tracking as `worklist`, with waiting/cancelled actions and progress-bound natural-stop continuation that preserves input priority and compaction state. (#1139-7d31)

- Offer native web search by default for supported Claude models with OAuth as well as API keys, and for `grok-4.7` on the official xAI route. Keep custom endpoints opt-in and honor explicit tool exclusions; provider acceptance and account entitlement remain separate (#1137-50b5).

- Add the `deepseek` provider: `deepseek/deepseek-flash` and `deepseek/deepseek-v4-pro` on `https://api.deepseek.com/chat/completions` with the key in `DEEPSEEK_API_KEY`. Thinking is `off`, `low`, `high` (default) or `max`; ego refuses `minimal`, `medium` and `xhigh` before the request. Reasoning goes back to DeepSeek on later requests that offer tools. Images, audio, parallel tool calls and hosted tools are not supported. `402` (empty balance) is not retried; `429` and `503` are. No price is stated, because DeepSeek's price changes with the time of day: the builtin catalogue has none and the third-party price table is not used for this row, so the cost stays unknown unless you give your own rates; the builtin catalogue now reads models.dev `9afe121d` (read 2026-11-14) (#1112-a347).

- Show a compact end-of-turn background activity count for native interactive text sessions, with bounded task labels and overflow counts. Keep the complete row within terminal width, including its display prefix. Completed work stays absent (#1125-391a).

- Save multiple named OAuth or explicit key-environment connections per vendor. Select a stable ID with `--connection`, edit its display name in user configuration, and keep credentials, model discovery and resumed ACP metadata isolated. Forks and compaction successors retain the source account ID rather than adopting the current default. New login creates a separate connection; legacy credential conversion is explicit and named connections never adopt an ambient account. Named auth management requires `--connection`; key sources require a secret-marked environment name and at least eight bytes so tool output can mask them. Key-minting built-in login is not offered as OAuth (#1113-d8c0, #1114-db9c, #1115-05a5).

- Add native web-search requests, citation decoding and private stateless replay for OpenAI subscription and xAI Responses connections. Preserve selected-account history boundaries and route-specific search usage. Whether a given account is entitled to search on these routes is decided by the provider (#1116-7592).

- Learn a per-session cache lifetime estimate after three comparable warm-to-cold cycles and a nearby confirmed hit. Require a hit covering the full preceding input and use its confirmed lower bound, not the loss interval, and distinguish actual full-input transport modes; unproven incremental continuations do not participate. Custom cache countdowns label the estimate and its three samples; known request TTLs take precedence, and opt-in idle compaction uses the same deadline (#1107-03c6).

- Delete old sessions: a session that nobody opened or wrote for 30 days is deleted with its rows in `history.sqlite`, and the file is compacted. The pass runs in the background at most once a day when `ego` starts, and on demand with `ego rm --expired --yes`; it logs each deleted session and the space freed. Open, held and recently resumed sessions stay, and so do the sources of kept forks. Configure the period with `[sessions] retention_days = N` (#1087-3af6).

- Add the `--mcp-only` run flag: the session attaches only the servers that `--mcp` selects and keeps every enabled configured server detached (#1077-e96a).

- Add `ego auth usage [provider] --json`: it shows your subscription quota as reported by the last response, without sending a request; stale values are marked expired, and retry cooldowns are shared with running sessions. The CLI help, command reference and shell completions describe its arguments (#1067-79fc).

- Add the `deny_prompts` setting for unattended interactive sessions: a call that would wait on a permission prompt is denied, the model receives the denial as the tool result and the turn continues. Set it in the user file or with `-c deny_prompts=true`; it is off by default (#1085-50bf).

### Changed

- DeepSeek models selected through `openrouter` keep their reasoning across tool calls: the reasoning of each assistant turn is stored with the session and sent back as `reasoning_content` on later requests that offer tools, also after resume. Other models routed through OpenRouter replay no reasoning, as before (#1109-dffd).

- A chat-completions stream that ends with `finish_reason` `insufficient_system_resource` or `aborted` now fails the turn with a retryable server error that names the reason, and keeps no reasoning of that generation for replay. Before, it failed as an unknown finish reason that was not retried (#1111-bfa0).

- **Breaking:** goal checks no longer receive audio bytes. Each audio clip in the evidence of a model check or a `--goal-check-command` is a fixed-size `audio_evidence` descriptor (blob reference, media type, `available_to_checker: false`); a 64 KiB clip shrank checker input from 131,358 to 267 bytes. An `achieved` verdict over evidence with audio needs `"audio_dependent": false`; otherwise the goal blocks with `audio evidence unavailable to the goal check: REASON`. Main-model requests keep the audio unchanged (#1089-c34a).

- A goal check builds its image descriptors from the recorded image metadata and no longer reads, decodes or hashes image blobs. A missing image blob no longer stops the goal check: the image keeps its descriptor with `available_to_checker: false`, and a success that rests only on that image is still not accepted (#1090-fccf).

- **Breaking:** Remove Anthropic OAuth from login discovery and login-method pickers, and disable direct login by default. Set `auth.anthropic.oauth_login_enabled = true` in user configuration to opt in; `false` disables login for any provider. Login discovery and direct CLI/session login use the same policy before effects. Saved credentials, refresh, API keys, provider visibility, status, logout and generic troubleshooting hints are unchanged (#1088-d15c).

- ACP `session/list` reads one page from a session table in `history.sqlite` and opens only the manifests of that page, not every manifest in the store. Manifests stay the source of truth: the table is kept current when a manifest changes, each listed row is checked against its manifest, and a missing or damaged table is rebuilt from the manifests on the next listing (#1086-5a53).

- Refresh the builtin model catalogue to models.dev `e726fbb3` (read 2026-11-13): add Claude Fable 5.2 (`anthropic/claude-fable-5-2`, Bedrock, Vertex and GitHub Copilot), `gpt-6.1-terra` (OpenAI, Codex subscription, GitHub Copilot), Gemini 4 Argon, Carbon and Hydrogen, and `MiniMax-M3.1-Flash`. `minimax-oauth/MiniMax-M3.1-Flash-Preview` leaves the catalogue; selecting it names its replacement (#1080-7d8a).

- Keep a readable session in the ACP catalogue when its ancestor is missing. Omit unavailable lineage details instead of hiding the session or guessing the ancestor's state; strict history lookups remain unchanged (#1079-6d59).

- ACP `session/list` no longer replays every session log, and session lists load faster: lineage details are shared across pages and manifests are read in parallel without execution history. (Which manifests a page opens is described by the #1086-5a53 entry above.) The manifest now also records the hold state and the lineage root of a session; a manifest without them is completed once, from its log, on the first listing. A session whose log is missing or unreadable is skipped and logged instead of failing the whole list with `-32002` (#1078-09c6).

- An interactive session compacts again when a turn of its compaction successor is cut at the context threshold after it measured usage, and continues the task without input; before, it waited at the prompt after the first compaction (#1076-c308).

- On TUICommander, an interactive session whose turn stopped at the context limit and whose automatic compaction failed (or whose successor overflowed before it measured usage) sends OSC 7770 `error=context_overflow` and `state=error` until the next turn starts, instead of waiting as idle (#1076-c308).

- On TUICommander, a turn that ends refused (exit reason `usage`) sets OSC 7770 `state=error` until the next turn starts, and sends the refusal text as `error=<message>` before the state on every refused turn (#1073-898a).

- **Breaking:** usage JSON advances to version 6. It, the status-line templates and their command input, and ACP status show the subscription quota of the account in use (#1066-3552).

- Interactive startup shows which authentication is in effect, the protocol filter state, where permissions come from, directory grants and MCP servers that need attention, with key hints that open the full details (#1068-2001).

- **Breaking:** When goal-check evidence contains an image, an `achieved` verdict must also carry `"image_dependent": false`. Without it the goal blocks with explicit visual uncertainty instead of completing on an image the checker cannot see; `--goal-check-command` evaluators must add the field (#1047-3e8c).

### Fixed

- After you approve a root for the session or permanently, the same request continues without another message. The new permissions and sandbox apply before the next model request, a read-to-write upgrade you just approved is not asked again, completed tool calls are not repeated, and background work keeps running (#1148-01fc).

- Accept typed builtin slash command names and aliases regardless of ASCII case, including `/Clear` and `/CONFIG perimeter`. Keep arguments, skill names and pasted prompt text unchanged (#1147-a847).

- Keep path-approval questions short: show only the requested access and path, explain grant lifetimes, and submit valid numbered single-choice answers without Enter. Keep arrow-key confirmation, multiple-choice toggles, and permission boundaries unchanged.

- Show the saved conversation after `/resume`, including selection from the session picker, using the same bounded display-only replay as startup resume (#1146-ee4e).

- A protocol filter that times out is retried up to three times, each in a fresh process, before the request is sent. Cancellation, run deadlines and filter rejections are not retried, and only the notices of the last attempt are shown (#1145-64b9).

- When an old subscription login has expired, ego now explains that signing in again creates a new named connection, which older sessions without a connection cannot use; recovery advice for a named connection names its ID. A refused fresh login no longer claims an expired login, and the image tool gives the same advice (#1117-f125).

- A waiting worklist resumes once when a monitored background command or an MCP task produces new output; repeated delivery and empty polls do not count as progress. Goal, worklist and report continuation are decided once per stop: your input and cancellation come first, and a worklist never overrides a goal that is waiting, failed its check or used its allowance (#1141-00e2).

- A goal set by the model keeps its text and owner unchanged, also across compaction, and pauses after 20 automatic continuations without claiming success; goals you set have no default cap (see `--goal-turns`). Resume an exhausted automatic goal with `/goal resume` or metadata-only `--goal-operation resume`; repeated tool output no longer keeps automatic work running (#1140-2eb0).

- Automatic compaction under context pressure waits for a running command to finish and includes its result, instead of losing it (#1144-6e73).

- When session roots block compaction or `/clear`, ego explains the two ways forward (start a fresh session, or revoke the root) and keeps the session open; it no longer retries the automatic compaction on every new prompt while the roots are unchanged (#1142-773e).

- Resume keeps the account a session started with; a session without an account never silently adopts a named connection. Startup names the account in use, and cloud logins that need a project and location keep working (#1113-d8c0, #1114-db9c, #1115-05a5).

- Subscription quota of a named connection is reported under its own provider and connection ID in machine output, and the status line shows only the quota of the account in use. The selected account is saved with the session and survives recovery (#1143-a0d0).

- Remove consumed queue previews even when an unmatched earlier preview remains, and reconcile visible pending input with the session ledger (#1136-25e6).

- Successful commands with stderr-only output now show neutral collapsed previews, while warnings remain visible and expanded details retain stream labels (#1124-054f).

- Keep provider-hosted web search out of recap, side-question and compaction requests while preserving it for the main conversation (#1131-b748).

- Offer default native search for documented OpenAI API/subscription and Anthropic API-key models on canonical endpoints without an Exa key. Keep unknown models and custom gateways opt-in; preserve tool exclusions and offline policy. Default composition no longer exposes configured independent Exa search on non-native routes; in this version an explicit `--tools web_search` still used Exa, which 0.4.12 replaced with `web_search_external` (#1135-5f22). Subscription defaults use cached search; explicit selection retains live access. The tools guide now describes cached search correctly (#1126-23d0, #1132-225c).

- Preserve observed Anthropic native-search usage when a response stream is cut. Provider totals and distinct observed search calls are combined without double-counting; missing input usage still stays unknown (#1116-7592).

- Keep active goal waits distinct from terminal stops in terminal receipts and JSON summaries. Repeated wait detail updates do not print another stop notice; a resumed goal can report a new wait (#1128-89bd).

- Deliver accepted prompts before scheduling another automatic goal turn or managed-report reminder, including input acknowledged while the goal checker is running (#1129-9655).

- Accept an active `waiting` outcome from model and command goal evaluators without completing or blocking the goal; new input and completion evidence can resume work (#1119-f27f).

- A question the model asked and you have not answered stays pending: a new prompt no longer sends the question to the provider again, and `ego resume ID --answer ...` answers it and continues the original turn (#1108-1b95).

- Ignore trailing whitespace in typed slash commands and their arguments, so `/help ` no longer rejects a final space and subcommands such as `/pin list ` parse correctly. Trailing whitespace is removed from free-text command arguments too; pasted text and leading-space escapes stay unchanged (#1106-6da4).

- A Codex subscription session over WebSocket now publishes its quota. The WebSocket handshake carries no quota headers, so ego now reads the `codex.rate_limits` stream event into the shared `rate_limits` snapshot of usage JSON, the status line and ACP status (#1066-3552).

- A large command output whose preview cut falls inside a multibyte UTF-8 character now keeps a text preview. Before, the whole 64 KiB preview became base64 and the output was not compacted. The partial character is left out of the preview and counted in the omitted bytes (#1104-603e).

- When a permission stop ends a turn, the tool calls that the model requested after the stopped call in the same batch are now recorded as `interrupted: not run`. Before, they stayed open, and the next request told the model that they "may already have taken effect" although they never ran (#1091-c91a).

- A failed provider HTTP call now writes one `warn` record with `EGO_LOG=warn` or a more detailed level. The record has the failure kind, the endpoint host, the HTTP status when the answer had started, and the cause chain of the transport, for example `kind=Connect host="127.0.0.1" detail="... tcp connect error: Connection refused"`. Before, no level logged the failure, and a failure other than a connect error showed no cause. The record never contains the URL path or query, headers, the body or a credential (#1099-0ff9).

- `ego mcp ls` and startup notices name why an MCP server was not admitted: a server that fails the MCP exchange, for example with a `-32000` error on `server/discover`, shows class `protocol` ("communication with the server failed"); a stdio server that exits at once or cannot be started shows `transport` ("could not connect to the server"); a malformed answer shows `invalid_response`. Before, all three showed `invalid_value` ("server configuration is invalid") and told the user to fix a configuration that was correct (#1102-a6de).

- An MCP server that answers `server/discover` with an error other than "method not found" (`-32601`), for example a bridge that cannot reach its backend (`-32000`), is now reported as a server error with its code and message. Before, ego reported every such error as "server does not implement MCP 2026-07-28" and told the user to update the server (#1097-7bbc).

- An MCP tool result that the server marks as an error (`isError: true`) is now a failed call: the model receives the error and the turn continues. Before, ego recorded it as a success, so a refused TUIC `progress` report of type `blocked` ended the turn with no final message (#1100-e872).

- Claude Fable requests no longer fail as "the provider is temporarily unavailable": ego accepted only letters, digits and `-` in HTTP response header names and refused the Anthropic header `anthropic-ratelimit-unified-7d_oi-utilization`. Every header name that HTTP allows is now accepted (#1080-7d8a).

- A ChatGPT subscription error with code `budget_reached` (streamed, or HTTP 400) is now classified as a quota refusal, like the other documented quota codes, instead of an unclassified failure (#1082-7c09).

- `ego rules` receipts and in-session rule changes print `durable` or `durability_uncertain`, the same words as `ego config`, instead of `published_durably` and `published_durability_uncertain` (#1072-3357).

- A session resumed or switched onto a model of another provider no longer fails every turn on the reasoning state that the earlier provider stored. The request leaves out only that opaque state; the visible text and tool calls stay, and the session history keeps the state. Compaction and the away recap of such a session apply the same rule (#1071-2262).

- A session that used hosted web search on one provider no longer fails every turn on another provider. The other provider gets only the visible answer text of those search items; the original provider still gets the items unchanged (#1074-0083).

- Escape revokes automatic wakes only of background commands, agents and MCP tasks that the stopped turn started; background work of earlier, completed turns still wakes the session. Escape on a turn that background output woke stops every background wake, as before (#1070-56ed).

- Return terminal input after Escape races turn completion, and revoke automatic wakes from existing native background commands, agents and MCP tasks across later explicit turns; results and explicit queued requests are preserved. Queued resumes also wait for the previous owner's checkout lease instead of failing during session shutdown (#1064-1e4e).

- Keep successful live session compaction silent. Report failures without routine session identifiers, skill digests, or continuation notices, and preserve timing receipts for ordinary turns. (#1069-df69)

- The request filter also checks HTTP MCP calls and external hooks, so restored secrets are not sent there; MCP arguments are otherwise passed unchanged (#1030-4fed).

- An active goal no longer waits while `exec_status` reads new output of a live, unmonitored background process: the result records typed output byte totals, and only output that grew since the last read counts as progress (#1046-f438).

- An ACP host sees a hook failure notice during the turn as a card message, not only after it loads the session again (#1054-d371).

- When an exec command ends and leaves a `nohup` or `&` child running, exec still stops it with the command, and the result now says so and names `exec background=true` as the way to keep a process running (#1057-ba1f).

- `/tools` and `/tools details` list the admitted MCP tools next to the builtin and skill tools, the same set that the next provider request carries (#1063-c613).

- `ego config edit` prints `durable` or `durability_uncertain` in its receipt, the same words as `ego config batch`, instead of the internal names `published_durably` and `published_durability_uncertain` (#1055-a043).

- Clarify `--no-tools` in help and shell completions: standalone run/resume disable all tools, while ACP removes builtins but still permits session host-admitted MCP tools. The ACP host-only mode is unchanged (#1050-0a85).

- MCP tool calls and ACP history replay share a durable `tuic/call_id`, so a host can correlate an interrupted call after restart without replaying its effect. Legacy records use the ego item identity (#1051-3465).

<!-- internal:
#1105-c0d2 repairs paired TCP test receiver lifetime and denial observation; no production behavior change.
#1098-5383 selects the explicit session journal in the missing-price regression fixture.
#1101-5c9f selects authoritative source and compaction-successor identities in the PTY fixtures.
#1095-8893 proves with a recorded PTY capture that goal receipt tests read terminal rows, not a Working repaint between two printer handoffs.
#1103-5fed gives the isolated worktree tests a git wrapper that a guarded host git can dispatch from.
#1065-00ec records redacted provider quota responses and the shared rate_limits contract.
#1048-9dfd records matched offline baseline/final-candidate counts and bytes without provider-cost claims.
#1045-4b1c records accepted positional and wire RED, exact continuation sizes and a neutral harness evaluation.
#1075-ac44 repairs a test fixture that lacked the turns the receipt gate requires.
#1081-c1c6 proves that an MCP OAuth refresh does not follow a token endpoint redirect to another host.
#1096-9e49 makes the repository harness-watch collector page until its saved cursor, so a long release gap recovers without a reset.
#1062-1bd8 proves that a truncated exec preview reference reads back unchanged.
#1061-1934 makes the Esc receipt PTY tests wait for the settled frame.
#1084-8bb4 lets the repository release-review collector reach its saved baseline after a long release burst; no runtime change.
#1048-9dfd preserves incomplete measurement evidence.
#1053-2671 identifies the local development workspace as 0.4.11; nothing was installed or published.
#1066-3552 also keeps matched passive quota readable when user configuration is malformed.
#1140-2eb0 and #1143-a0d0 align the generated CLI reference and shell completions.
#1056-4ddc makes the evaluation harness report its Python 3.11 minimum; #1060-85d0 lets the documentation link check skip Git-ignored local records.
#1126-23d0 and #1132-225c restore an internal default-search export.
#1045-4b1c, #1046-f438, #1047-3e8c, #1049-25be and #1052-513f are already listed in the historical 0.4.10 section; this preparation commits that existing source without declaring incomplete stories finished.
-->

## [0.4.10] - 2026-11-11

### Added

- Add a native `image_gen` tool: it generates one PNG from a text prompt and writes it into the workspace, whichever chat model is active. It uses the signed-in ChatGPT subscription (`ego auth login openai-codex`) when there is one, with no API key and no per-image charge, and otherwise OpenAI `gpt-image-2` through `OPENAI_API_KEY`, which needs `allow_paid=true`. It needs network and file-write permission and never overwrites a file. The subscription backend chooses the image size itself, and the result reports the real width and height. Editing an existing image is not supported yet, and an expired subscription login must be renewed with `ego auth login openai-codex` (#1005-fe42).
- The startup line and `/version` now show the short commit and profile of the running binary (`ego 0.4.7 · a1b2c3d4e · release`), so you can tell which build hosts a session. A build outside a git checkout shows `unknown`; `EGO_BUILD_COMMIT` sets it explicitly. `ego --version` is unchanged (#1013-6a2d).
- Skills accept optional identity fields, inert extensions, reference boolean and tool-list forms, inherited models, shell metadata, and source-directory substitution. A description over 1,536 characters is cut instead of refused, `argument-hint` may be a list, `metadata` and `license` never refuse a skill, `effort` ignores letter case, and a reference tool name in `allowed-tools` is refused with a message that says the field narrows in ego. `${CLAUDE_SESSION_ID}`, `${CLAUDE_PROJECT_DIR}` and `${CLAUDE_EFFORT}` expand in skill text, `paths` is now an activation hint (the resource policy is `resource-paths`), frontmatter that is not YAML loads with no fields unless it names a field that restricts the skill, `agent: general-purpose` means the ordinary child, and Markdown commands read the same frontmatter fields as skills. New `[skills]` configuration keys `hide` and `off` hide a skill by name from the model, or from the model and the user, without editing it, and `ego doctor` counts the catalogue that hiding leaves (#996-8e27).
- Protocol filters can be set once globally and turned off per provider. The filter's startup message names the generation role and the actual wire format; its notices are bounded, and a filter rejection is final, without retry or bypass (#1009-4c56).
- Add `ego usage --session ID`: it reports one local session without scanning the workspace store (#1037-5927).
- Add `ego rm --incompatible --yes` to remove unsupported-format sessions without deleting readable sessions. Session listings show one actionable incompatibility count instead of one error per session; shell completions and the command reference include the option (#1001-99d9).

### Fixed

- Return PNG and JPEG files read by the native file tool as model images instead of text bytes (#1024-edce).
- Keep the prompt marker and status row stable when deleting draft text or closing completion (#1025-998d).

<!-- internal: #1036-0f96 repairs terminal and permission-readmission test observations. #992-3b8a repairs Windows lint, target gating and test fixtures. #990-6f1e is already listed in an earlier version section. #1039-42e3 identifies the local development workspace as 0.4.9 and #1053-2671 installs the local 0.4.10 snapshot; neither publishes a release. #963-824e returns the permission judge to its prior prompt and keeps the development variant for simulation only. #732-5e09 also makes offline TCP capability probes wait for the receiver's acknowledgement. -->

- Exclude opaque provider reasoning from goal-check evidence instead of repeatedly sending it as text; retain the exact state for ordinary model continuation (#1052-513f).

- Keep automatic goal continuations at their original position in conversation history without repeating the active objective in each continuation (#1045-4b1c).
- Bound goal continuations driven only by admitted background-process inspections, while collecting ready process and task evidence before waiting (#1046-f438).
- Send bounded image metadata instead of binary image payloads to goal checkers and `--goal-check-command`; unavailable image contents are explicitly marked as inaccessible, not visual proof (#1047-3e8c).

- Synchronize composer redraws so supported terminals do not show intermediate erased or partly reprinted frames while typing long drafts (#1049-25be).
- Registering `exec_monitor` after an owned background command finishes returns a normal settled receipt instead of an invariant error; its output and exit status remain readable (#1041-66b0).
- `ego ls` omits session directories with no saved journal instead of repeating a `session not found` error. Readable sessions and corruption diagnostics are preserved (#1044-31e4).
- Collapsed MCP servers now state when a context refresh requires schema rediscovery before the next tool call, rather than silently forgetting schemas still in conversation history (#1040-5876).
- Preserve private startup diagnostics in a session directory when a CLI launch fails after acquiring its writer lease but before publishing session state. The record names the startup phase and error; an interrupted startup remains explicitly unresolved (#1043-c834).

- Resumed sessions on the unconfined backend admit ego version changes without an approval prompt and show the old and new version. Confining backend changes still require approval (#1038-a997).

- Piped command tables retain complete cells instead of truncating them to the width of an unrelated controlling terminal (#1035-0bf6).

- A durable plan with pending or in-progress steps adds a fixed status note after ten tool calls without a plan update. Replay preserves the count; forks and compaction successors start a fresh window (#1006-a166).

- The status row shows a parked composer draft as `prompt parked` (several: `N prompts parked`) instead of `drafts=N`. The `drafts` template variable stays a number (#1033-d949).

- Cursor-position queries keep their bounded wait across unrelated terminal wakes, so a resize does not discard receipt rows or duplicate the live footer (#732-5e09).

- Active goals wait for new input after two consecutive continuation turns without tool calls or inbound events, instead of spinning on reworded unmet conditions (#1032-f23c).

- Protocol filters reserve a distinct turn index for automatic checkpoint continuation, so later user turns do not reuse its captured state. In a compacted session, a filtered automatic continuation no longer invents a user prompt (#1010-fad1).
- Keep an idle interactive prompt asleep until input or an asynchronous producer wakes it, without delaying streamed output, metadata, repaint or interruption (#1020-f0b1).

- Silent MCP stdio servers no longer hold startup indefinitely; initialization stops at the deadline (#1026-6fff).

- Queued prompts: resizing while their previews grow keeps earlier transcript rows intact; a queued prompt enters the transcript only when it is consumed, so recalling it leaves no duplicate row; input accepted during a running turn keeps its recall history without a stale composer footprint; and queued completion notices show their bounded receipt instead of the raw transport envelope (#1028-e0ae).

- Interrupting a turn while its final output is queued no longer deadlocks the terminal and blocks keyboard input, and a terminal failure no longer leaves shutdown waiting forever (#1017-c9b6).



- Image generation now publishes its PNG without replacing a file created while the image request was running; ordinary write and edit tools still replace files as before (#1005-fe42).

- Provider retry receipts now distinguish successful recovery, failure and cancellation; cancellation no longer repeats a stale provider error (#1016-2ac8).
- Runs with `--no-tools` no longer start configured MCP servers or print their startup warnings. Non-empty `--tools LIST` selections keep automatic MCP attachment (#1029-896e).
- Resume lists this workspace's sessions in lazy pages of ten, newest log write first. Page Down loads the next page; unreadable and in-flight sessions are skipped silently without repairs or listing writes. Loading leaves the picker responsive, and `/quit` works inside it. Headless resume reads only its first readable candidate (#1018-ce3e).

- When the owner dialog for a root cannot ask or finish, the model is now told that no approval is possible, to stop and report to you, and not to run `ego` commands that change roots, rules or configuration. The outside-workspace refusal says that only the owner grants a root (#1021-a690).
- Outside `yolo`, an `exec` of an administrative `ego` command (`ego roots add`, `ego config set`, `ego rules allow`, `ego mcp add`, `ego auth logout`, `ego setup` and similar) is never approved by a mode, a wildcard allow rule or the `auto` judge: you are asked, or a deny rule refuses it, and an allow rule that names the exact command allows that command only. Only an exact allow rule from your user file or a launch option can allow one; a project, local or agent file cannot. A nested `ego` started with `--yolo`, `--sandbox off`, `--allow-dir` and similar options is held the same way. Read-only commands such as `ego roots ls` and `ego config get` are unchanged, and so is `yolo` (#1014-de9e).
- With `yolo` and sandbox `off`, ordinary native external writes and edits no longer ask for a root grant. Explicit denies, read-only roots, protected-file floors and captured administrator write ceilings remain binding. Remembered denies update the live policy, and a profile's first stored-rule block preserves inherited user rules after restart (#1007-8db3).

- The `plan` tool result now ends with a recap instruction when an update leaves every step completed, once per finished plan, so the model summarizes the work instead of reading bare JSON. A plan with an open or blocked step gets no cue (#1015-4f0e).

- Protocol filters accept automatic continuation and new user prompts after compaction using the new session's local turn index and isolated state, instead of comparing them with the parent journal's sequence numbers (#1019-5d74).



- The command menu opens again when Backspace returns into the prefix of a command you typed in full, and the file menu closes when the text after an `@` is a sentence instead of staying on `NO RECORDS FOUND` (#1000-5eb4, #1011-d570).



- Workspace `.env*` values are masked again unless the owner explicitly grants disclosure, which reverses the 0.4.7 change that showed them after a user read rule. Masked previews show `KEY=<redacted>` for non-empty values and `KEY=` for empty values, so hidden values do not look empty (#955-6d78).
- Terminal hosts now see busy until ego-owned subagents and background commands settle, even after a model turn ends, the empty composer receives Enter, or a local modal closes (#993-216f).

- ACP stdio sessions no longer overflow a worker stack when a changed server executable requires reconnection (#991-cfd1).
- Running foreground commands show elapsed time and their latest output on the terminal progress row, before the compact result replaces it (#987-6ec6).



### Changed

- **Breaking:** store MCP OAuth and confidential continuation credentials in an encrypted local file with a separate local key by default; set `EGO_CREDENTIAL_BACKEND=native` to opt in to the OS credential store. Existing native logins are not imported automatically (#1003-56d1).

- Background command completion prints one aggregate line without per-command IDs or outcome rows (#1027-3045).

- Goals continue without a default automatic-turn allowance. `--goal-turns N` still sets a finite limit for a new goal; saved historical limits remain finite. The `/goal extend N` and metadata `extend=N` operations are no longer available; 0.4.11 adds a pause after 20 continuations for goals the model sets (#985-dda0).

- Setting a new operator goal replaces an active goal instead of rejecting it. The replacement starts with a new identity and fresh counters; the old goal remains in the session history (#989-b9a9).

- Filtered sessions can resume goal work after background results without rejecting the internal turn as a missing user prompt (#994-f022).

- Setup lists support page and boundary keys, show aligned key hints, and offer review/save from every settings page (#1002-c6de).
- `ego mcp add NAME ...` on an entry that has `enabled = false` keeps it disabled instead of silently attaching the server at the next session start (#1008-8f16).

- `ego a2a` no longer accepts `--session-dir`, which it never read. The root `ego --help` lines for `acp` and `setup` now match the commands, `ego mcp add` usage lists `--profile`, and the generated command reference lists the options that a command shares with `ego` once instead of repeating them and no longer names hidden options in conflict lists (#1008-8f16).

- The note for `--read-only-dir` says that only that directory is read-only, and the refusal of a `task` call mixed with other tool calls says to send the task call alone (#1008-8f16).

<!-- internal: #999-ec48 aligns metadata grammar and setup confirmation tests with the current CLI. #1031-2b74 protects default MCP startup from native credential authorization waits; the behavior fix is #1003-56d1. -->
- Make `ego setup` settings searchable with compact catalogue previews and full saved or staged values in setting details, separate from effective values. Root details list saved directories in order and let the owner stage one path or access change or removal at a time; the existing review and atomic save still apply (#984-e73a, #986-9b17).


- Increase the default protocol-filter exchange timeout to 10 seconds and allow a user-configured value up to 30 seconds. Generation deadlines and cancellation are unchanged (#1004-ddad).
- An idle interactive session no longer wakes about 330 times per second to probe a retained stdio server; its supervisor now waits for exit, cancellation, interruption or the deadline (#997-b545, #998-6b27).

- Show error notices in red on colour terminals; redirected and informational notices remain unchanged, and `--no-color` disables error colours (#1012-ea44).

- Slash command lists line up in columns and fit the terminal width; hook identities and task output are shown in the detail views (#995-b8aa).

## [0.4.7] - 2026-11-08

### Added

- Recorded permission-judge decisions keep the exact judge instructions and the redacted judge input (facts JSON, instructions SHA-256, model, context sizes), so a live decision can be replayed offline; older records without them remain readable and sanitised exports still omit the input (#980-7817, #963-824e).

- Add opt-in learned build-output recipes (`build_recipes = true`). The model can save a checked, declarative recipe for one exact `exec` argv with the `build_recipe` tool; later successful runs of that argv give the model only the lines the recipe keeps, plus full-output references. Lines naming errors, warnings, failures, panics, exceptions or deprecations are never omitted, any doubt keeps the complete answer, and resumed sessions replay the answer that was sent. Recipes are private to each workspace; `ego build-recipes ls|show|disable|enable|rm` manages them (#930-6d69).

- Add `ego rewind ID --at ITEM_ID`: return the conversation and the files to the same turn in one command. It restores, newest first, the file checkpoints that the session's builtin writes recorded after that turn, then forks the conversation after it. Before it changes anything it refuses, and lists the files, when a file changed outside ego after that turn or when a write after the turn is not recorded (for example a `task` child's write). The source session is kept, and the new session's lineage names it. Builtin write, edit and notebook-edit results now record their checkpoint id in the session journal (#868-b2a5).

- Add an opt-in recap after an absence: with `recap.enabled = true` (a row in `ego setup`; user or system configuration only), the first interactive prompt sent at least `recap.after_minutes` (default 30) after the last completed turn first shows a short recap of the session. The absence counts across restarts. The recap reuses the cached request prefix, shows a usage line, is counted by `/usage` with the main model, and is not added to the session history; a failure is shown and the prompt still runs (#871-ae31).

- Add the `checkpoints` configuration key, `ego setup` row and `--no-checkpoints` flag. False (or the flag) stops `write` and `edit` from saving private Git checkpoints in the repository; the default is unchanged: checkpoints stay on. Only user configuration and the command line set it (#881-200d).

- Compress cold sessions in the background: after 7 days with no write, `events.jsonl`, `manifest.json` and evidence blobs larger than 4 KiB become zstd `.zst` files (about four times less disk space on real sessions), and every reader handles both forms. Live sessions are never touched; resuming a cold session restores plain files first. Configure with `[sessions] compress_after_days = N`, turn it off with `compress = false` (also the `sessions.compress` row in `ego setup` and `ego config`), or skip it for one run with `--no-session-compression`; it is on by default. Older ego versions cannot read compressed sessions (#841-a65f, #882-c71b).

- Add the `tools` configuration key and `ego setup` row: the same tool names as `--tools`, an empty list for `--no-tools`. It limits the model tools of every run; `--tools` or `--no-tools` overrides it for one run. Only user configuration and the command line set it (#883-99be).

- Add the opt-in `lsp` tool: the model asks a language server for the definition of a symbol, its references, or the diagnostics of a file, instead of searching text. Turn it on with the `lsp` row in `ego setup` or `ego config set lsp=true`; only the user configuration can set it, and it is off by default. ego starts `rust-analyzer`, `gopls`, `clangd`, `pyright-langserver` or `typescript-language-server` by file extension, from `PATH`, at the first query, under the run's confinement like an MCP stdio server, and reuses it while ego runs. Only workspace files are answered; an answer lists at most 50 entries and 16 KiB (#865-93a4).

- Add `--title TITLE` to `ego resume` and `ego continue`: resume the session of this workspace whose title is exactly TITLE (case-sensitive), for example `ego resume --title "release notes" "next step"`. When several sessions have the title, ego lists them with their ids, most recent first, resumes none and exits 2; no match also exits 2 (#870-9981).

- Add `ego resume ID --queue PROMPT` (also `-f FILE` or `-` for stdin): a script queues a prompt for a saved session without a terminal. It waits while another process (an interactive session, a resume, an ACP connection or an earlier queued prompt) owns the session, then runs the prompt headless with the same admission as `ego resume ID PROMPT` and exits with that turn's status. Queued prompts on one session run one turn at a time, in no guaranteed order. The wait counts toward `--timeout` (exit 124). `--queue` needs a prompt and is refused with `--answer` and with profile `ci` (#856-e2cc).

- Add the `bedrock` and `vertex` providers for Claude through Amazon Bedrock and Google Cloud Vertex AI. `bedrock` sends the Messages API to `https://bedrock-mantle.REGION.api.aws/anthropic/v1/` with an Amazon Bedrock API key from `AWS_BEARER_TOKEN_BEDROCK`; the region comes from `AWS_REGION`, then `AWS_DEFAULT_REGION`, else `us-east-1`. `vertex` sends to the project in `GOOGLE_CLOUD_PROJECT` and the location in `GOOGLE_CLOUD_LOCATION` (default `global`); `ego auth login vertex` mints the access token from the Google Application Default Credentials that `gcloud auth application-default login` writes (`GOOGLE_APPLICATION_CREDENTIALS` can name the file) and the token is renewed from the same file before it expires; service-account key files are not supported. Both list the current Claude models (Bedrock ids start with `anthropic.`), cache for 5 minutes or 1 hour, are priced from the litellm table, appear in `ego setup`, and keep thinking state on the platform that made it. Server-side web search, IAM SigV4 signing, and Gemini on Vertex AI are not supported (#863-a3a9).

- Add `/side QUESTION`: ask a side question while a turn runs. The answer comes from the turn's newest request and context, so a provider with prompt caching reads the cached prefix, and no tool runs. The question and the answer are not added to the session history. A usage line follows the answer, and `/usage` counts the usage with the main model. An idle session refuses `/side` and asks for a normal prompt (#867-af6f).

- Add the `exec_monitor` tool: the model can subscribe to new complete output lines of a background command, optionally only lines that match a regular expression. Matching lines enter the model context at the next step, before the command's settlement, and wake an idle interactive session, so the model does not poll `exec_status`. One delivery has at most `max_lines` lines (default 20, at most 100) and 4096 bytes, with lines cut at 512 bytes and later lines counted as omitted. The monitor ends when the command settles or is cancelled (#854-5f4a).

- Add opt-in MCP Tasks for long tool calls. With `mcp_tasks = true` in the user configuration (or the `mcp_tasks` row in `ego setup`), an interactive terminal session declares the Tasks extension to HTTP MCP servers; a server can then run a long tool as a task. The model gets an answer with the task id at once and the turn continues; ego polls the task at each generation, under the grant of the original call, and gives the model each result once when the task ends. Tasks end with the session, and ego asks the server to cancel each live one; stdio servers, ACP and headless runs never use them, and a task that needs client input is cancelled (#866-653b, #888-dd64, #886-78a2, #887-3ab1, #889-cf82).

- Add opt-in workspace memory. Turn it on with `ego setup` (row `memory`), `memory = true` in the user configuration, or `--memory` for one run; it is off by default and a project or local configuration file cannot turn it on. When on, one Markdown file per workspace under `$EGO_HOME/memory/` (16 KiB limit) is read at session start and at `/refresh` and sent after the instruction files, and the model gets a `memory` tool to add, replace or remove one entry. Only that tool and you change the file; plan mode refuses a write. A file over the limit is not sent. `--bare` and `--system` send no memory, and with memory off no memory file is read or written. The new `/context` command lists the system prompt sources and the memory state and size. `session.started` lists the memory file with source `memory`, its path, digest and size, never its text (#852-84a6).

- Add `compaction.threshold` (integer 1-99, default 90): the percent of the measured context window at which a turn yields and an interactive or headless session compacts. It replaces the fixed 90% value, so the default behavior is unchanged. A configuration where `compaction.idle_threshold` is higher than `compaction.threshold` is refused; note that setting `threshold` below 50 also needs a lower `idle_threshold`. It appears as an advanced row in `ego setup`. Only user and system configuration set it (#851-0b8f).

- Add the `Interrupt` hook event. It runs once when the user interrupts a turn (for example with `Ctrl+C`), after the turn closes as cancelled, and its input names the interrupted turn. It is an observer: its result cannot resume or change the turn, and a failing handler is only logged. Use it for cleanup and audit scripts (#860-0fd7).

- Add `effort` to named agent definitions and the config keys `task.model` and `task.effort`, which set a default model and reasoning effort for foreground `task` children. Precedence per field: named definition, `task.*` default, parent. Both keys are unset by default, so children keep inheriting the parent. They appear as advanced rows in `ego setup`. Background tasks and forked skills do not read them (#858-b6f8).

- Add `--no-hooks`, accepted wherever `--no-tools` is: no hook from configuration or from a skill is admitted or run for that invocation, including in task children, and the configuration is not changed. `session.started` lists `hooks_off` in `capabilities`, and the session records an empty hook catalogue (#872-caed).

- Add `fork: true` to the `task` tool: the child starts from the parent conversation and sends the parent's last request prefix (tools, system, history) followed by its directive, so a provider with prompt caching can read that prefix from cache. The child runs one turn, cannot delegate, and returns only its final answer; its usage is booked to the parent. `fork` with `background` or `agent` is refused. The default stays the clean child (#849-5434).

- Add `on_failure = "block" | "allow"` to a hook declaration. With `block`, a pre-effect hook that fails to start, times out or crashes blocks the guarded action; with `allow`, the action continues and the failure is recorded in the event stream. The key is refused on hooks that run after the effect. This entry first made `block` the default; within this release the default became `allow` (#916-5920) (#859-476c).

- Add an opt-in capture of a provider error that arrives after HTTP status 200: `EGO_LOG=ego::body::provider_failure=debug` with `EGO_LOG_FILE` records the status, the response headers and the last 64 KiB of stream frames once per failed stream, with header values outside a public allowlist and JSON values under secret-like keys redacted. Ordinary `EGO_LOG` levels do not enable it (#844-7e93).

- Add opt-in compaction of an idle interactive session shortly before its prompt cache expires, so the compaction reads the context at the cache-read price instead of the next turn writing it again. Turn it on in `ego setup` or with `ego config set compaction.idle=true`. It runs once per cache deadline, only when the TTL is known (sent by ego or set by `cache_ttl_seconds` in the active profile), only when no prompt, question, request, background command or background agent is pending, and only when the measured context is at least `compaction.idle_threshold` percent of the window (1-99, default 50). `compaction.idle_margin_seconds` (default 30) sets the start before expiry. No task continues after it. Only user and system configuration set these keys (#803-c64f).

- Add `ego resume --accept-authority-delta` (also `ego continue`) to resume a saved session whose execution authority changed, including headless jobs. The option allows the change shown in the notice for that one resume and journals it as a party decision; the new measurement becomes the saved authority and a later, different change asks again. Without the option a headless resume of wider authority still exits 3 before any provider call, and administrator ceilings still apply when the configuration loads. Rerun without the option after restoring the configuration to refuse the change (#786-2a2c).

- Add the user-only setting `web_private_network = true` (also in `ego setup` and `ego config set`) to turn the `web_fetch` block on local and LAN addresses off for every admitted call. The default keeps the block on; a project or local repository configuration that sets it is refused. Unspecified, broadcast and multicast addresses stay refused. (#784-ff9a)

- Add `--trust-development-programs` to approve edits to current named workspace programs with a notice. New programs, changed declarations, command Markdown and external instruction disclosures still need approval. Use `--trust-workspace` to restore byte-bound trust, or remove the workspace ledger entry to revoke trust (#777-3299).

- Show a diff for every file change. A `write` and a repeated `edit` (`replace_all`) now show the added and removed line counts and the first twelve diff rows under the tool row, and `/expand` shows the full diff. A new file shows as added lines. The diff uses a before image of at most 64 KiB read when the file is replaced; above that size, for a file that is not text, or for a protected credential file, the receipt says the diff is omitted. The `write` receipt now ends with `(+A -R lines)`. The before image is saved in the session journal with the result and is not sent to the model (#861-a746).

- `read` accepts a PDF path and returns its extracted text through the usual 2000-line and 32 KiB pages. A PDF over 20 MiB is refused with its size and the limit, only the first 100 pages are extracted and the result says how many pages were left out, and a malformed PDF is a tool error. The PDF is not sent to the provider as a native document (#864-9836).

- Probe provider network reachability in `ego doctor` and `/doctor`, when you opt in. For each configured provider (a stored credential, or the key variable its row reads) doctor checks DNS, TCP, TLS and one bare unauthenticated HTTP `GET` of the provider endpoint, with each layer bounded at 2 seconds. An unreachable provider is a warning that names the failed layer; any HTTP answer, even 401, counts as reachable. No credential is sent, so the probe never logs in, refreshes a token or spends money. The probe is opt-in: it runs only with `ego doctor --probe-network` or `doctor_network = true` in the user configuration (a row in `ego setup`; a project file cannot set it), and by default doctor makes no network call. `ego doctor --offline` and the offline posture (`network="off"`) skip every probe and win over the opt-in (#857-e1a6).

- Show the remaining prompt-cache lifetime of the session in the interactive status (`cache=M:SS`, `cache=H:MM:SS`, `cache=expired`, or `cache=unknown`). The countdown starts at the last provider request with the cache TTL that ego sent, and it resets only where the provider documents renewal on read. Providers with a provider-managed or unknown TTL show `unknown`. The new profile value `cache_ttl_seconds` sets a display-only estimate for those providers; it sends nothing to the provider. Custom status templates can use `{{ cache }}` (#802-8be0).

- Use an access token from `OPENAI_CODEX_ACCESS_TOKEN` for `openai-codex` when it is set and not blank. ego never refreshes or stores it and does not read the saved login; an unset or blank variable keeps the saved login (#818-e820).

- **Breaking:** the public event schema is now version 20. Compact a long headless `ego run` or `ego resume` automatically instead of stopping with `context_overflow` (exit 4), when you turn on `compaction.headless` (off by default; set it in the user configuration or with the `compaction.headless` row in `ego setup`). At `compaction.threshold` (default 90%) of the measured context window, at a known-full window or on a provider context-limit error, the run saves a checkpoint, continues in a clean successor session within the same budget, deadline and `--max-turns`, and reports the successor in its summary. The JSONL stream adds `session_compacted` (`source`, `source_seq`, `target`) before the successor's `session_started`. A failed compaction still exits 4 and names `ego compact SOURCE` as the retry. The `ci` profile keeps explicit recovery (#850-9beb).

- Compact a long ACP prompt automatically in the same way, for a client that accepts the transition by sending `clientCapabilities._meta.ego.compacted` = `{"version":1}` in `initialize`: the prompt continues in the successor and answers its stop reason. A `session_info_update` with `_meta.ego.compacted` (`sourceSessionId`, `sourceSeq`, `targetSessionId`) names the successor, and the connection then routes requests that name the old session id to the successor. `initialize` advertises `_meta.ego.compacted`. A client that does not opt in keeps the typed context-overflow error and explicit `_ego/compact` (#850-9beb, #878-830f).

### Changed

- **Breaking:** `on_failure` now defaults to `allow`. A hook that cannot run (killed by a signal, non-zero exit other than `2`, missing binary, timeout, malformed output) no longer fails the turn or the session on any event. ego continues and shows one notice that names the event, the hook, the failure kind and the cause, such as the exit code or signal. Only an explicit block decision from a hook that ran blocks; `on_failure = "block"` opts a gate hook back in to blocking on failure (#916-5920).

- A configured provider protocol filter can now run during compaction after a committed user turn. The private compaction generation uses the source session's next turn index and retains durable filter variables; ordinary generations still require their own committed user prompt (#982-5221).

- ACP clients can confirm a permanent sandbox root through a separate permission request (#732-5e09).

- Named session lookup waits for an in-progress journal append instead of treating its partial record as corruption (#732-5e09).

- Interactive `/login [PROVIDER]` lists and authorizes built-in OAuth providers without launching another Ego process. A successful login is available to the next model turn in the current session. A browser provider whose authorization URL has no state parameter can complete its loopback callback; a supplied incorrect state is still refused (#975-524c).

- Linux sessions can re-admit an exact grant for a new file without widening the child sandbox to its parent; the write tool can then create the file (#976-dec1).

- Terminal root approval is offered for writes under a read-only root; exact approved writes and new-file session roots survive re-admission. Sandboxed read-only git pipelines can use captured system tools with unrelated PATH entries (#976-dec1).

- ACP sandbox prompts can keep a refused path blocked using a persistent deny rule. A permanent sandbox approval is confirmed as a root and committed through the root receipt. Terminal and ACP sessions can offer a root for a writable read-only-root refusal; hard floors report only their class (#977-20e0).

- Long process output that remains after progress folding now shows an explicitly bounded tail excerpt, including for unknown formats; complete output remains available through its evidence reference (#978-b810).

- The native terminal reports background commands that settled together (in one turn, or found at the next idle wake) as one bounded outcomes block instead of one full notice each or a replay of stdout and stderr: failures and timeouts first, each with process ID and true exit, timeout or cancel state and a short excerpt; lines an `exec_monitor` already delivered and build progress are not repeated, and the agent can inspect full output by ID. Model-visible settlement is unchanged. Superseded in 0.4.10, where completion prints one aggregate line (#1027-3045) (#969-2bfb).

- A run that nobody could ask (headless exit 3) now lists three exact remedies after the denied action: allow for this run (`--allow RULE`), allow always (`ego rules allow RULE`), keep blocked (`ego rules deny RULE`); omitted when the exact rule would disclose credential values (#962-e24b).

- `exec_status` of a background command now compacts its stdout and stderr tails: a carriage return overwrites the line as in a terminal, and each run of build progress lines (`Compiling`, `Building [`, `Downloading`…) folds into its last line and a count. Warnings, errors, test results and any unknown output stay verbatim, and the complete output stays recoverable through `read_tool_result`. This is a live-output presentation, separate from the opt-in learned build recipes that shorten a later successful foreground answer The answer to the model for a successful foreground `exec` gets the same fold on the first run, with a pointer to the complete output, with no recipe or external wrapper; the owner's background status view uses it too; a failed command folds its progress too but never takes a recipe, and keeps its exit status, warnings and errors (#964-0fb4).

- Network friction (#958-d9a2): approving a native web fetch once now covers later GET fetches from the same host and port in the session, whatever the path (a POST, a credential-bearing request or another host asks again). Fetches from common documentation and package hosts (`docs.rs`, `crates.io`, `registry.npmjs.org`, `pypi.org`, `docs.python.org`, `developer.mozilla.org` and similar, port 443) need no question; they are listed by `ego rules ls` as `default` allow rules, and a `net(...)` deny still wins. The prompt says so (`[y] yes, and every later fetch from HOST:PORT this session`); `[a]` still saves a permanent `net(HOST:PORT)` rule.

- Instruction files (#959-bc4d): in `adaptive` sandbox a command run by exec can now write `AGENTS.md` and `CLAUDE.md` inside the workspace, as the edit tool already could. Instruction files beyond the workspace and Markdown files under the home's `.codex`, `.claude` and `.claude-private` stay read-only. `docs/guides/permissions.md` states this.

- The terminal approval prompt offers `[k] keep blocked`: it stores a deny rule for what the request names (the exact path for a sandbox widening, the call's own rules otherwise), shows the rule, and the same refusal is not asked again. A sandbox widening that a stored deny rule or an earlier answer already refuses is never asked Root approval prompts say that keeping a root blocked remembers the denial (#962-e24b, #977-20e0).

- `ego run --allow-dir PATH[:ro|rw]` lets one run reach an existing canonical directory (read-only unless `:rw`) without storing a root. A headless refusal of a path outside the workspace now lists the exact commands: allow for this run (`--allow-dir`), allow always (`ego roots add`), and keep blocked (`ego rules deny`) (#962-e24b).

- A sandbox refusal of a write or read outside the workspace now offers `[a]` in the terminal: it keeps that exact path grant for this workspace, shown before saving, and the next session starts with it and asks nothing. Execution grants stay session-only. ACP already offered the same choice and shares the store (#962-e24b).

- `/model QUERY` selects the unique available model whose name contains the query, without opening the picker or changing the saved default; ambiguous matches and models without a compatible reasoning effort require an explicit choice (#973-a6ed).

- A user-owned protocol filter can emit headers without a second allowlist or blanket name-category exclusions, including credential replacements. Output framing and bounds still apply; replacement credentials are public header values in Ego (#972-0168).

- **Breaking:** a user-owned protocol filter now declares request and response interest independently in a bounded v1 startup exchange. Uninterested directions pass through without sending body bytes to the executable; missing or malformed startup fails before provider dispatch. Existing filter executables must adopt the new v1 framing (#970-6430).

- Interactive question, model, session and slash-command completion menus now mark the selected option with bold muted-blue text instead of an inverted bright background (#971-1498).

- The agent now directs blocked effects to their native owner approval, prefers a session grant, and does not change saved roots, rules or sandbox settings on its own. A request for lasting access still requires the owner to approve the actual scope; `ask_question` alone is not a permission grant (#962-e24b).

- MCP friction (#957-0130): approving a server tool call once now covers later calls of that same tool in the session whatever the arguments (a changed server admission or transport asks again). A stdio server that does not answer `server/discover` is reported as a protocol-version mismatch that names the method. `ego mcp ls --mcp NAME|@FILE` and `/mcp` list invocation-selected servers. The startup note for a server that needs attention appears once per cause, not at every launch, also in a one-shot `ego run "prompt"` on a terminal, and names the `enabled = false` setting; a server that recovers is forgotten, so the note returns if it fails again with the same cause. The approval prompt now says so (`[y] yes, and every later call of SERVER/TOOL this session`). A call to a collapsed tool with no admitted schema now tells the model to call `get_tool_schema` first.

- On macOS the `adaptive` and `workspace` sandboxes now let ordinary development commands run: cargo through the mbx shim (shim directory, mbx cache and target views, `CARGO_HOME`/`RUSTUP_HOME` toolchain and caches), git commit with SSH signing (`~/.ssh/*.pub` and `allowed_signers` are readable; private keys stay closed), the `tuic` CLI inside a TUICommander session, and HTTPS from `curl` and `git fetch` under `workspace` (system TLS configuration). Cargo credentials stay unreadable (#952-d347).

- `ego acp --exclusive-read-only` serves an unattended read-only session that no configuration layer or host request can widen: no native tool, hook, skill, custom command or MCP server, and an environment cleared to an allow-list (platform values, the configured provider's credential variables and each `--env-allow NAME`) (#950-32d4).

- A session that starts inside a git working tree now treats the whole repository as one workspace: the main checkout, every linked worktree and the common git directory get the workspace's access for built-in tools and exec in every mode and sandbox (`ro` stays read-only). Membership comes from validated git metadata (`commondir`, worktree back pointers), so a sibling repository or a forged pointer never joins. `ego config perimeter` lists these roots with source `same-repository`. Credential floors and git hook/config protection are unchanged. A worktree created after the session starts joins without a restart: the roots are derived again before each spawn and each policy check (#951-534e).

- A generation whose input exceeds a model's long-context threshold is now recorded and budgeted at that tier's rates when the litellm price table states them (`*_above_Nk_tokens`), instead of the base rate. Earlier recorded costs are unchanged (#941-4fb4).

- `ego usage` now estimates the cost of generations whose price was unknown when they ran. The recorded cost is unchanged; the report adds a separate `estimated` figure (text: `~X estimated`) priced from the recorded token usage with the newest cached price table that arrived before the request (`request-time table`), else the current table (`current-table estimate`), else `estimate_unavailable` with its reason. The estimate is never stored and never counts toward a budget. Estimates apply the long-context tier rate above a model's threshold (#940-403c).

- An accepted interactive `/goal <objective>` now shows one confirmation, `Goal (operator): <objective>`, instead of two (#936-cec0).

- A busy-turn steer now shows `Queued message continues the work` instead of `Interrupted by user`; a plain Esc or Ctrl+C keeps the interruption receipt (#935-829b).

- `ego --help` now lists a description for `build-recipes` instead of repeating its name (#937-f5fc).

- `ego setup` and `/setup` now keep a review truthful until the save. Setup refuses a save and reloads the stored values when the user file changed after setup read it, and refuses a publication when a reviewed effective value changed during confirmation. The review marks security-sensitive keys and states that a running session keeps its captured perimeter. Session origins of redacted values show as unknown. The provider row is presence-only help without a ready count. In a terminal, a long review scrolls with ←/→, confirmation defaults to No and accepts Yes only after the end of the review was shown, a terminal smaller than 20x6 gets a refusal instead of a hidden review, Ctrl-C leaves setup without saving, and notices stay on screen above the next view. In `/setup` the whole review is the prompt and ends with `Save these changes? [y/N]`; a blank answer declines (#926-b908).

- Activate root orchestration guidance only when the first internal `task` or admitted MCP `spawn` is attempted: hold the original call, let the model brief a worker under the new role, and do not launch the original call. Give internal children their own worker guidance and keep Ego-authored model prompts in one annotated source module (#933-7db0).

- Give every session ego's own working rules in its stable system prompt: granted authority persists, authorized work is finished before one consolidated question, independent actions go out in the same step, only work that can start now is delegated, and long waits run in the background instead of blocking the turn. A user question does not cancel an unfinished authorized task, and a failing validation gate is not completion while the task needs it green (#932-6641, #938-796b).

- Explain hook failures with their causes and readable signal names, keep hook notices out of usage errors, and honor run cancellation during pre-effect hooks (#918-a8c8).

- Refuse buffered control-plane HTTP responses over 32 MiB instead of accumulating an unlimited body; the error states the limit (#922-bccd).

- Describe previously undocumented CLI arguments in command help and the generated reference (#925-da7c).

- Reuse the approved project-file trust decision in a registered linked Git worktree when its executable authority matches the trusted main checkout. Only checkouts that declare no project command inherit; a checkout with a hook, program or other command, or with a worktree-specific answer, still needs its own trust decision; revoking main trust also removes the derived approval (#929-0551).

- A goal with one item awaiting the operator now continues independent authorized work: the goal check names the next useful action without authorizing the blocked item, and returns `needs_operator` with the owner action only when no authorized useful action remains. The goal tool description states the same rule (#917-a5b8).

- A completed background agent whose answer is longer than 8,192 bytes now enters the parent context as a short receipt with an evidence reference instead of the full text. Read the masked answer with `read_tool_result`. Shorter answers and failures are delivered inline as before (#923-b46e).

- Let `Ctrl+S` park a prompt while attached to a task child, then restore it automatically after the next accepted submission. A refused child submission keeps both the parked draft and the text available for retry (#914-81ba).

- Keep useful stdout visible in successful mixed-stream command previews and summarize stderr with its line count and expansion hint, without changing model evidence or expanded diagnostics (#906-c766).

- Show the actual executing model and reasoning effort in cyan task rows and `/tasks`, including recovered background children. Keep unresolved profiles explicit and retain elapsed time and token counts when narrow rows have room for them. Freeze agent catalogue facts at admission and preserve host model/effort ceilings for child selections (#911-85fc).

- Honor per-call model and effort for background tasks and retain the admitted selection through durable recovery and attachment (#910-564b).

- Let foreground and forked `task` calls select a canonical `model` and supported reasoning `effort` without changing the parent selection. Omitted fork fields inherit the actual parent profile; incompatible requests fail before generation (#909-f175).

- Expose known canonical model IDs, configured provider identities and supported reasoning efforts through the read-only internal `model_catalogue` tool, without implicit network or authentication effects (#908-2d84).

- Make `/clear` start a fresh conversation and clear the composer, retaining the previous session for resume (#907-d152).

- Give background agents stable parent-session aliases (`t1`, `t2`, …). `/tasks t1` attaches direct text input to the live child; `/detach` returns to the parent without cancelling it. Full IDs and `/tasks cancel t1` remain available; task cancellation retains the existing parent-turn mutation boundary (#904-89c4).

- Name the restricting root and its configuration source when a built-in write is refused by a read-only root, and the host profile that made a root read-only when that source is known (#903-ebf4).

- **Breaking:** the `edit` tool takes a batch: edit up to sixteen existing files with exact original-text chunks. The batch requires admission before effects and reports ordered outcomes and recovery evidence (#894-3fb6).

- Show the observed token-weighted cache hit rate as a percentage, marked `♻`, in the default status line and the compact footer. Custom cache countdown templates retain their existing behavior (#895-03ff, #915-e945).

- Show preceding prose and valid requested commands before completion, omit empty command rows for partial requests, and preserve answer boundaries across messages, queued turns and cancellation (#898-3644).

- **Breaking:** the `write` tool requires a `files` array and writes one to sixteen complete files. Each path needs admission before writing; the result records ordered outcomes and stops at the first error (#893-6753).

- **Breaking:** the `read` tool requires a `files` array and reads one to sixteen named files. Results identify each item, retain explicit per-file failures and continuation offsets, and share a serialized 32 KiB budget. (#892-d5e9).

- Apply the configured permission mode to command and HTTP hook admission. An uncovered command hook can run in Yolo; unresolved approval questions end without an interactive prompt or automatic retry (#901-79df).

- Report a failed prompt-submission hook instead of ending the prompt silently as a successful turn. Keep intentional veto feedback visible (#896-5c27).

- A failing completion hook no longer makes the model generate another answer again and again; the turn ends with a notice. Intentional completion vetoes still continue with their feedback (#899-3dcd).

- Treat native task delegation as the default: explicit user or applicable workspace instructions can select another available coordination mechanism while retaining that mechanism's contract (#897-5676).

- Tell TUIC once that ego is busy at every actual turn start, including resumed and automatic turns without a new user message; avoid a duplicate busy signal in the headless opening preamble (#900-242b).

- With `mcp_tasks` on, an interactive session that waits at its prompt polls its live MCP tasks when they are due. A task that ends starts one generation that carries its result, instead of waiting for the next prompt; a poll that finds the task still working starts none (#890-7df7).

- `ego setup` and `ego config set/get/unset` now manage `cache`, `compaction.idle_margin_seconds`, `compaction.idle_threshold` and `instructions.max_files` as typed rows instead of only naming them; `auth.PROVIDER.protocol_filter` is listed as an advanced row (#884-78dd).

- Enroll OAuth services that host login, token or registration on a different origin than the issuer: repeatable `--oauth-host ORIGIN` on `ego mcp add`, or `oauth_hosts` in the server entry. Only configuration enrolls hosts, per server; discovery metadata never adds one. Issuer, resource and audience binding, PKCE, state, the callback issuer check and the public-client requirement still apply. Remove the origin and log in again to revert (#788-8562).

- Continue a managed peer that ends its turn without the report its brief requires. When the brief names a parent (`must report ... with agent action=send to="<parent>"`) and the session holds no successful `agent send` to that parent whose message starts with RESULT or BLOCKED, ego adds one reminder turn asking for it, at most twice per session. Sessions without that brief text are unchanged (#874-0251).

- Apply a `Session` or `Permanent` answer to the terminal root question in the same process: when the asking turn ends, ego re-admits the same session with the root, so built-in tools and new child processes reach the exact approved path without a configuration edit or restart. Earlier roots stay. Re-admission waits while a background command or agent runs. On Linux, a new read root asks once to re-admit the changed execution policy. The question now previews the perimeter after approval (workspace, kept roots, new root) and states that it adds no exec grant. The new `/config roots` lists session roots, permanent roots and owner sandbox grants with the revoke command. A session root removed from disk is dropped with a note and does not block later grants or a resume. System root ceilings, exact paths and read-only access still apply (#758-819c).

- Stop offering fast mode for Claude Opus 4.8, directly and through OpenRouter. Anthropic removes it on 2026-11-30, and a fast request then fails with no fallback. Selecting fast on Opus 4.8 is refused with a hint to use Opus 5.5 (#848-26d4).

- Name the dialled address, its family (IPv4 or IPv6) and whether a proxy was used in a provider connect failure, for example `HTTP connection failed (api.x.ai): dialling 104.18.18.80:443 (IPv4, proxy: no): ...`, so a network error such as ENETDOWN can be attributed to one address. With a proxy the address is the proxy's (#822-e36f).

- Stop writing each streamed assistant text chunk to `events.jsonl`. The terminal, JSONL output and ACP clients still receive the chunks live; the log keeps only the assembled assistant message, which cuts a recorded 46-chunk turn from 29,300 to 13,908 bytes. Each assistant message completes once, also when the model writes text before and after a tool call. ACP `session/load` sends such a message as one chunk. Logs written by earlier versions still load, replay and export. (#842-a5b9)

- Ask the Windows unconfined-execution question once per user instead of once per workspace. A `yes` covers every workspace for the current contract revision and is stored in `$EGO_HOME/windows-unconfined.json`; each later start that relies on it prints a notice that execution is unconfined. Direct and piped starts still need `--sandbox off`, `--offline` still refuses, and a `workspace`, `ro` or `adaptive` sandbox still fails on Windows. Delete `windows-unconfined.json` to be asked again. An earlier per-workspace answer in `unconfined.json` is not used, so the first start asks once more. (#797-a31f)

- Let `web_fetch` open a local or LAN address (`localhost:3000`, LAN documentation, Tailscale `100.x`) that the owner approved: a user `net(...)` rule that names hosts, or a prompt approval of an address literal. Owner rules accept a host without a port (every port), a `*.lan` subdomain pattern, and the new CIDR form `net(192.168.1.0/24)`. Addresses outside the pattern, bare `net(*)`, project rules, `auto`/`yolo` modes, redirects to another endpoint and unapproved names that resolve to private addresses remain refused; a redirect to the same scheme, host and port of an approved endpoint is followed under that approval. Remove the rule to revoke the approval. (#784-ff9a)

- Skip the repeated content re-hash of an executable after you approve a call by hand, ego reuses a session approval, or you run a terminal shell command: the check made right after the approval now compares the resolved path and file identity, and the check at the launch hashes the content for every authority. A call allowed by a stored rule or a cached judge decision keeps both content hashes, so a replaced executable is still refused. On a 64 MiB image this saves about 185 ms per approved call. A rule keeps the extra check; there is no setting to restore it for manual answers (#796-c9b6, #846-514b).

- State, in the remember prompt and the `rules remember` preview, that remembering a read or search of a credential file shows the values of that file, because an exact user rule is the disclosure approval. The `[d]` details, the `[a]` prompt, the ACP "Allow always" prompt and a `DISCLOSES` preview row carry the warning, and `docs/guides/permissions.md` lists every way to grant disclosure. To withdraw it, revoke the rule with `ego rules revoke ID` (#839-9a1b).

- Show why a credential helper failed: `ego auth login` and token refresh now print the helper's exit code and its bounded stderr (4 KiB), with the request, reply and credential-shaped secrets masked and terminal escapes made visible. `auth.PROVIDER.credential_timeout_secs` (1–3600, default 120) lets a slow browser login finish. Cancellation, the run deadline and process-tree cleanup are unchanged; the helper stays a trusted owner-configured program. Remove the key to restore the 120 s limit. (#792-a140)

- Read a symlinked `AGENTS.md` from its canonical target and make the instruction file count configurable (`[instructions] max_files`, at least 1, default 8; user configuration only). A target outside the discovery scope is withheld, with a notice, until the workspace trust answer approves that exact path and digest. Links in parent directories, dangling links, link cycles, oversize files and import cycles stay refused. Remove the link or answer the trust question again to recover. (#790-a79f)

- `web_fetch` follows up to 5 redirects to the same scheme, host and port in one call, and accepts `max_output_kib` (1 to 256, default 32) for a larger page; the download limit stays 8 MiB whatever the output size (#905-98fc). Each followed hop is a GET with no headers or credentials, uses the network decision that admitted the request, and repeats the public-address check. A redirect to another host, port or scheme, `http` to `https` included, still returns the destination for a new policy-checked call; HTTPS-to-HTTP redirects stay refused, and a sixth hop fails the call. A `net(host)` deny rule blocks the host entirely (#798-0f96).

- Let a project propose extra roots, executable paths and MCP tool grants in a `[grants]` table. The workspace trust question lists them and one answer approves exactly that text; a changed proposal asks again. Approved grants add to your own values, and system ceilings and denies still bind them. Without trust or with `--bare` the proposal is discarded. Delete the table or remove the workspace trust entry to revoke (#793-f7ce).

- Let a trusted project or local configuration select `cache = "long"` (1-hour provider prompt-cache retention) instead of requiring a user-config duplicate (#789-a690). The workspace trust question now names each config file that requests it, and adding it to an already trusted workspace asks again. An untrusted workspace, `--bare` or `bare = true` still discards it and keeps `short`. To revert, answer No at the trust question or remove `cache = "long"` from the project file.

- Attach explicitly configured HTTP MCP servers in every sandbox mode, not only with `--sandbox off`; `--offline` still refuses every MCP server. The per-server `allow_insecure_loopback = true` opt-in now admits plaintext HTTP to any named host on the owner network, not only to loopback. HTTPS servers cannot be downgraded, redirects are not followed, and a plaintext OAuth issuer must be on the server's own host. A plaintext server off loopback with `auth = "oauth"` gets one warning per run and per login. Remove the opt-in to refuse the server again. (#785-8798)

- Show credential file values after explicit owner approval. An exact user `read(path)` rule returns the file bytes unchanged, including `.ssh`, `.netrc` and JSON token files that had no key-name format, and an exact `search(path)` rule lets `grep` match and show its lines. Without that rule, credential reads and searches still return key names only. Project rules, flags, wildcards, `yolo`, sandbox `off`, session approvals and directory approvals never disclose values, and approval of one file does not disclose a sibling. Native tools honour the same exact grants; sandbox `off` removes the built-in credential floors, while Adaptive keeps protection against ungranted reads and recursive traversal. Revoke the rule with `ego rules revoke ID` to return to key names (#783-4404, #782-be36).

- Use the selected sandbox for authenticated LAN connections instead of forcing `adaptive` and `off` to `workspace`. Public binds still need explicit consent and every TCP connection still needs its bearer token. Hosts remain within the selected ceiling and backend capabilities. Restart with `--sandbox workspace` or `--sandbox ro` to restore a tighter ceiling (#781-0559).

- Honor exact user configuration allow rules for `.git`, `.ego` and `.claude` paths. Project rules and wildcard rules remain excluded; explicit denies, roots, Plan mode and process sandbox limits still apply. Remove the user rule to revoke future rule authority (#780-8406).

- Allow exact owner-approved instruction and Git control file maintenance in the Adaptive sandbox. Approval applies to a later explicit command and never replays the failed command. Directory grants, automatic judge decisions, credentials, installed toolchains and protected ancestor relocation retain their limits (#779-5cf8).

- Keep explicit process sandbox read, write and execute approvals through compaction and same-session resume after fresh admission. New and forked sessions do not inherit them. Path approval never replays a failed command; judge grants stay write-only. User configuration can adjust residual judge limits (#778-9079).

- Reuse exact owner-approved shell commands within a live session, including across generation policy captures. Changed commands still need their own approval; explicit denies, Plan mode, roots and sandbox limits remain in force (#775-b0a5).

- Explain command questions with the missing session approval and possible process effects, alongside the existing mode, operation and judge evidence (#775-b0a5).

- Headless startup with exactly one usable provider that has a default model uses that model for the run, names it on stderr, and does not save it. `ego setup` shows which providers are ready and how to enable the others, and the Homebrew formula prints the first-setup steps. (#828-8be5)

- Show the startup logo with a speed trail and a magenta → violet → cyan gradient. The logo needs 25 columns; narrower terminals and plain output do not show it (#721-7fa3).

- Show the arguments of built-in tool calls as short coloured fields, in the same style as shell commands, instead of raw JSON with empty fields. Glob patterns and grep queries appear as the subject; default values such as an empty search root are left out (#739-d1b1).

- Colour the added and removed line counts in compact edit receipts (#817-f443).

- Name the images attached to a submitted prompt in the live transcript and when a session is resumed (#756-c019).

- Mark the initial ACP session configuration options as pending while sandbox measurement runs, and explain why a configured default is not supported, so a client does not take the missing options as unsupported (#816-70ca).

- Refuse a command name that follows launch flags, for example `ego --profile NAME acp`, with usage exit 2 instead of sending `acp` to the model as a prompt. Put the command first (`ego acp --profile NAME`), or use `ego --plain -- acp` to send the word as a prompt (#819-069b).

- Retry transient failures in headless runs, provider capacity refusals included, up to three more times by default (two in an earlier build of this release, see the capacity retry entry), and honour the provider's stated delay. `--retry N` replaces this allowance; `--retry 0` makes one attempt. One capacity refusal now prints one notice, not three (#824-b247).

- Stop a runaway model response: at most 128 tool calls per response, at most 8 identical calls in it, and at most 4,096 calls per turn. Above a limit no call of that response runs, and a headless run exits 7 with reason `guard` (#825-9d33).

- Refuse tool calls whose arguments contain a raw model control token such as `<|control28|>` before they run. No call of that response runs, and a headless run exits 7 with reason `guard` (#826-12cf).

- **Breaking:** the usage JSON report version is now 5. Show in `ego usage` and `/usage` an estimate of how much context each tool's requests and results use, and which reads repeat earlier reads, with the events they come from. The estimates do not change charges or the session history. (#829-9773).

- Retry a provider capacity refusal (HTTP 429, 503 or 529, or a typed overload or rate-limit error) on the same model after 1, 2 and 5 minutes instead of after seconds, so a temporary refusal no longer stops a long run. A longer provider `Retry-After` wins; a stated wait above 5 minutes stops a finite plan. Headless runs now allow three retries by default instead of two, so the refusal is reported only after the third retry fails; `--retry N` still sets the count and `--retry 0` makes one attempt. Other server errors and connection failures keep the 1, 2, 4 second backoff. The retry notice says `provider at capacity` and shows the wait and the clock time of the next attempt; ACP `providerRetry` adds `capacity` (#855-9071).

- Auto mode friction (#961-8b4e): `auto` now approves recognized development commands without the judge (cargo, npm/pnpm/yarn scripts, go, swift, make/just, test, lint and format tools, local git including `git config --list`). A judge deny is no longer final: you are asked, with the judge's reason, and the question names the command (a refusal of a command no longer prints its directory). The judge prompt treats builds, caches, temp files, git and package installs as normal work; it receives a repository profile (stacks, worktrees, project instructions), your earlier answers of the session, every admitted root, and runs with low reasoning effort on the main model, never the provider's smallest one. `/config` shows the effective judge (`permissions.judge`) even when it is unset. 0.4.10 returned the judge to its earlier prompt (#963-824e); the other changes stay.

- `ego mcp-server` starts without numeric arguments: `--max-concurrent-runs`, `--retained-requests` and `--max-resource-bytes` default to 1, 16 and 65536. New repeatable `--allow-root DIR`, `--allow-model P/M` and `--allow-mode MODE` let the owner declare additional run targets; the directory from `-C`, the configured model and the configured mode stay allowed. A client request outside that set, or beyond the concurrency, retention and size bounds, is refused as before, and a model, mode or root outside it is never reachable. `ego://capabilities` shows the active policy. Subagent and skill runs inside an MCP run no longer count against `--max-concurrent-runs`. Restart without the `--allow-*` flags to return to the single deployment target. (#794-7efe)

### Fixed

- Outside-workspace write approvals now refuse protected Git controls and instruction files that the exec sandbox also refuses, instead of offering a grant that would bypass that floor (#953-fc94).

- Workspace `.env` files now expose their values when an effective user read rule allows the file. A masked credential file explicitly explains how its owner can grant an exact read Reversed in 0.4.10: workspace `.env*` values are masked again unless the owner explicitly grants disclosure (#955-6d78).

- `ego run --timeout` now also bounds the read of a prompt from stdin: an open pipe that never closes no longer holds the run past its deadline, and the run exits 124 (#960-faf1).

- Granting a root for a file that does not exist yet (a new `NOTES.md` in a sibling directory) no longer fails with `root path changed or disappeared`: a read-write root may name the exact file under its existing canonical parent. The parent directory itself is not granted, and a missing parent, a symlinked parent or a read root is still refused (#954-4d0a).

- A configured root that no longer exists, or cannot be resolved, no longer stops ego from starting. It is skipped or narrowed, reported once on stderr, once in the model's first request (`Configuration notices`) and by `ego config ls --effective` and `ego doctor`. A missing read root keeps restricting writes through its nearest existing canonical ancestor, and an entry that would grant access is dropped, so a skip never widens authority (#966-97ad).

- A hook request larger than 64 KiB (for example a PostToolUse hook after a large tool output) no longer fails with `hook request exceeds 64 KiB`. The largest text fields are shortened with a visible `[ego: N bytes removed to fit the 64 KiB hook request]` marker and the hook runs (#968-829f).

- A read-only exec no longer asks a question in `default` and no longer fails in `plan`, whenever the sandbox is in force (`adaptive`, `workspace`, `ro`): `cat`, `head`, `tail`, `wc`, `ls`, `grep`, `rg`, `git status|log|diff|show` with plain options and `git config --get|--list` run straight away, because the operating system still refuses credential and control files. A command with a write flag, a helper or output flag (`--output`, `--ext-diff`, `--pre`, `git -c`) or a sensitive name still asks. With sandbox `off` only the narrower earlier proof applies. `[a]` on `make`, `just`, `swift`, `dotnet`, `uv`, `bun`, `mvn` and `gradle` now offers a program-and-subcommand rule, like `cargo` and `npm` (#956-a721).

- Unknown model prices no longer interrupt an uncapped text session with an irrelevant spending-cap warning. The `unpriced` cost status and machine-readable anomaly remain available; configured caps still warn (#974-1bcc).

- A sandbox (`workspace` or `ro`) no longer refuses to start a stdio MCP server that you selected with `--mcp NAME|@FILE` or enabled in configuration: the exact canonical executable of each such server, resolved once at start, joins the sandbox exec perimeter. No directory, wildcard or later lookup is admitted, so another file with the same name stays refused. A server that runs as `interpreter script` (`python server.py`, `node server.js`) also starts when its script lies outside the workspace: each argument that names an existing file joins the perimeter as that exact canonical file only. Before, such servers were reported as "server configuration is invalid" and their tools were absent (#957-0130).

- A provider response that documents an exhausted quota, credit or spend limit is no longer retried as a rate limit. It ends the request as an account refusal, and the cause says that retrying will not help until the limit is raised or resets. This covers OpenAI 429 codes such as `credit_balance_exhausted` and `insufficient_quota`, the Anthropic monthly spend cap (`enforced_spend_limit_reached`) and operator spend limits (HTTP 400), Gemini `quota_exceeded` and `payment_required`, and the ChatGPT subscription `usage_limit_reached`. A 429 with an unknown or unreadable body is still retried (#921-010d).

- A session that TUICommander manages (`TUIC_SESSION` set) no longer gets the ego missing-report reminder on top of the host's own reprompt; ego alone still reminds (#939-8c87).

- Keep the `web_fetch` download limit at 8 MiB independently of `max_output_kib`. Requesting a short rendered answer no longer rejects a larger supported page; rendered output remains bounded to the requested size (#905-98fc).

- Preserve colored headless tool diffs as pre-laid-out diagnostic rows. ANSI styling no longer causes premature hard line breaks in wide terminals; ordinary notices retain their wrapping (#902-bbdd).

- An extra read root equal to the active workspace no longer blocks authorized builtin writes there, including when a broader writable root is configured. This restores workspace write precedence in ordinary permission modes as well as Yolo (#891-5e40).

- The generated CLI reference no longer prints "Repeatable Repeatable." for `--allow`, `--deny` and `--oauth-host`: a flag whose help already says Repeatable gets no second mark (#885-231c).

- A local socket endpoint in an ordinary (not 0700) directory is no longer refused: ego leaves that directory unchanged, creates a private child `<socket name>.d` (mode 0700) beside the requested path and reports the actual socket path on stderr (`ego acp: listening on PATH`, `ego a2a: listening on PATH`; every listener endpoint is reported this way). Only the owner can connect. An existing child that is a symlink, a file or not a private directory of the user is refused unchanged; remove it to recover. (#795-86ac)

- Keep the receipt rows when the terminal is resized right after a line is accepted: the next prompt no longer repaints one row too high over them (#814-82fe).

- Mask environment secrets by name segment and length: only variables with a name segment (split on `_`, any case) that ends with `TOKEN`, `KEY`, `SECRET` or `PASSWORD` qualify (`API_KEY`, `PGPASSWORD`, `NGROK_AUTHTOKEN`), so `MY_KEYBOARD` and `TOKENIZER` no longer do, and values shorter than 8 bytes are never masked. `AWS_ACCESS_KEY_ID` and `GOOGLE_APPLICATION_CREDENTIALS` and masking across output chunks are unchanged, as is child environment inheritance. To mask a short value, use one of 8 bytes or more. (#791-4509)

- Keep one live composer after a terminal height change whose cursor report arrives late. The inline view no longer leaves a stale footer copy and blank rows above the redrawn composer, and growing the terminal again no longer erases the restored transcript (#808-1e9b).

- Show the whole submitted prompt. A prompt longer than three rows is no longer shortened to three rows with a "more lines" hint (#809-12f4).

- Keep a question of an interactive turn open when a cursor-position reply from the terminal is late or lost. The session no longer exits with status 3 and a `resume --answer` hint (#811-94a1).

- Return a call to a tool that does not exist to the model as a failed tool result, so the model can correct itself, instead of ending the turn with "provider returned undeclared tool name" (#837-58a4).

- A provider stream that sends no bytes or frames for 300 seconds no longer leaves the turn waiting indefinitely. Before visible output, the existing retry policy can retry; after visible output, the turn shows an error and never replays the attempt (#990-6f1e).

- Keep the terminal in raw mode while `Ctrl+C` cancels a running turn. Until the turn accepted the cancel, the terminal echoed keys typed after `Ctrl+C` and turned their Enter into a new line, so a command such as `/expand` stayed in the draft instead of running. The cancel still clears the draft and shows `cancelling active work` (#879-94e2).

- End the generation with one error when a provider returns a tool call with no id, for every provider. OpenAI Chat Completions-compatible, OpenAI Responses, Codex subscription and xAI rows dropped such a call and finished the turn as a tool use (#838-79af).

- Discard a usage report whose cached input exceeds its input total with one notice for every provider, and settle the generation at its bound. Gemini clamped the figure and reported it as usage, and other providers each had their own notice text (#838-79af).

- Record the tool calls of a response that the provider then reports as failed as not run, for every provider. OpenAI Responses and xAI rows dropped such calls without a record; other rows already recorded them as not run (#838-79af).

- Retry an xAI capacity refusal ("The model is currently at capacity due to high demand") within the bounded retry budget, keeping the provider's stated delay. The refusal stream carries more than one failure frame, and ego previously kept the class of the first, unclassified frame, so the turn ended without a retry. One refusal still shows one notice (#827-f537).

- Show the startup model picker notice (for example `provider 'ollama' has no startup default`) and picker error notices inside the picker frame; they were stored but never drawn (#835-bd95).

- Compact sessions on models with a 32,768-token window and no declared output maximum, such as the Ollama default, instead of refusing with "The compaction instruction and output reserve cannot fit the 32768-token model window". When the model declares no output maximum, the default response ceiling is now at most half the context window. Set `max_output_tokens` to choose another ceiling (#843-a0e5).

- Admit sandboxed work when the ego configuration file has hardlinks or a writable root contains `EGO_HOME`, instead of refusing the session. Only writes to the configuration stay denied, including through hardlink aliases inside writable roots; nothing grants them. On Linux, a root that contains the configuration is writable around it: ego cannot create, delete or rename entries directly in the directories on the way to the configuration. Select a workspace outside `EGO_HOME` to restore full write reach (#787-b8ee).

- Name the cause on stderr when the interactive terminal read fails (`terminal input failed: ...`), also for piped interactive input, or when the terminal cannot enter raw mode or be written (`terminal raw mode failed: ...`, `terminal output failed: ...`), before the session ends, instead of exiting without a message (#774-6ca5).

- Start reliably in terminals that do not answer the keyboard or cursor queries. ego 0.4.3 could stop after printing its version in some terminals; the keyboard query now has a deadline, and a cursor query with no answer falls back to a fresh bottom row instead of exiting silently with status 0 (#805-3ea3, #813-f467).

- Compact an interactive session when the provider reports that its context window is exhausted, also before ego has measured the context. If the compaction fails, the session stays open and shows a `/compact` retry hint (#810-593e).

- Run an MCP connection retry that the ACP client asks for explicitly at once, instead of answering "retry later". Automatic retries keep their backoff (#776-f56b).

- Keep the exit status of the last turn when redirected stdin ends: a provider or stream failure exits 4 and invalid provider input exits 2, instead of 0. On a terminal, closing the session after a refusal still exits 0 (#821-dddb).

<!-- internal: #876-c3bf restored a code comment with no user-visible effect. -->
<!-- internal: #911-85fc task profile event documentation and field-exhaustive export conformance repairs; #912-f0d2 model catalogue invalid-input behavioral coverage. -->
<!-- internal: #766-0878 #773-6527 #800-8eba #801-2625 #807-9774 #815-fe6b #834-de99 #836-59e9 #845-d455 #847-d1e0 #875-c969 -->
<!-- internal: #942-6c23 #943-1bc2 #944-8095 #945-07c4 #946-9180 harness evaluation tooling, frozen task suite and procedure; no user-visible effect. -->
<!-- internal: #983-cf2e adds the maintainer `make bump` and `make release-local` commands. #934-5ed5 refreshes compatible Rust dependencies in the lockfile. #927-e011 hardens the repository harness-watch link step. #937-f5fc also boxes `ToolResult` in the internal `ProcessError` variants. -->

## [0.4.3] - 2026-11-01

### Added

- Configure a user-owned provider protocol filter that transforms outgoing
  generation bodies and incoming streamed bytes. Filter failure stops generation;
  ego never falls back to unfiltered traffic. (#766-0878)
- Grant an extra filesystem root from the owning terminal for the session, or
  save it permanently in the selected user profile. Revoke it with
  `/config roots revoke EXACT_PATH`. (#758-819c)
- Send richer native terminal events to TUICommander for tools, child activity,
  turn outcomes, and permission or question prompts, without private task text,
  tool arguments, results, or answers. (#771-b309)
- Let ACP v1 clients opt in to native child lifecycle metadata. Load and resume
  return the current durable child roster. (#772-d55a)
- Resume the most recent readable session in the workspace with
  `ego --continua`. (#761-6ad8)
- Open selectors with bare `/mcp` and `/skills`, and group startup MCP notices
  in the terminal. (#762-a6ce)

### Changed

- **Breaking:** the public event schema advances from 18 to 19 with typed child
  lifecycle events. ACP stays at protocol version 1. (#771-b309, #772-d55a)

### Fixed

- Ctrl+Enter now works as a separate key in terminals that support the Kitty
  keyboard protocol, and suspending with Ctrl+Z restores the terminal's keyboard
  mode. (#760-5312)
- Remove leftover sandbox probe directories that an interrupted ego left in the
  workspace. (#764-c1ab)
- Keep the prompt usable in terminals that stop answering cursor-position
  queries after startup, including full prompt receipts that scroll beyond
  the screen. Preserve input through a provider retry and background completion
  wake. (#807-9774)
- Exit ACP on SIGTERM and settle an active turn before shutdown. (#773-6527)

<!-- internal: #770-301d keeps terminal output after a matched setup marker in the setup confirmation tests. -->

## [0.4.2] - 2026-11-01

### Added

- Run independent subagent work in parallel. An idle interactive parent wakes
  when a background subagent settles. (#741-9295, #757-4add)
- Start the requested work when you set an interactive goal, and extend an active
  goal with `/goal extend N` or `--goal-operation extend=N`. (#749-faa2, #765-3b8f)
- Run a local shell command from the idle prompt with `!COMMAND`; its output
  becomes context for the next turn. Use `!!` for a literal `!`. (#755-2ff5)
- History search results link to the exact message in the original session;
  follow the continuation pages to read more. (#738-722a)
- Show inline diffs for successful unique edits, resolve relative Markdown links
  against the run workspace, and show useful rows in compact process previews.
  (#743-fcc2, #745-bb58)
- Guide user-default and profile configuration through terminal setup, with
  review and confirmation before an atomic save. (#747-2fe1)

### Changed

- **Breaking:** license ego under Apache-2.0. (#734-636b)
- **Breaking:** interactive setup no longer accepts a prompt argument; use
  `ego setup --profile NAME --directory PATH`. (#747-2fe1)
- Publish the user guide on GitHub Pages and stop advertising the `/memory`
  command, which 0.4.0 removed. (#736-a63c, #740-09d3, #715-e1f8)

### Fixed

- Hook output no longer invalidates the Codex prompt cache: hook-context
  changes are appended after the history. (#769-679d)
- Keep Expand responsive while a turn finishes output, and keep the last
  terminal row free below the prompt. (#742-5bb5, #737-7a70)
- Label a stopped goal continuation as paused, and strike only the recovered
  cause in a provider-error receipt. (#754-0adc, #753-d540)
- Programs that check the name they were started with (`argv[0]`) now work
  under the sandbox, and the ACP write-scoped workspace probe is fixed.
  (#752-47c9, #759-2f0b)
- Accept empty unbound plan steps. (#751-56f1)

<!-- internal: #733-bc4b #735-2cd2 #732-5e09 #731-9296 #750-876a (a maintainer skill that reviews harness changelogs) -->

## [0.4.1] - 2026-10-31

### Fixed

- Stop reporting a worktree as in use after the ego process that held it ended.
  (#731-9296)
- Release the session writer lock before its file closes on Unix, so reopening a
  session in the same process no longer reports a stale lock. (#729-0ee2)

<!-- internal: #724-57cc #725-d066 #726-79ee #727-6987 #728-afc6 #730-9514 #095-5a9c -->

## [0.4.0] - 2026-10-30

### Added

- Show the number of hidden rows and the expansion shortcut below collapsed
  tool results; edits show actual added and removed line totals. (#656-caa5)
- Pin explicit session constraints with `/pin <text>`, list them with `/pin`,
  and remove one with `/unpin <n>`. Pins survive restart, compaction, and forks.
  (#663-107a)
- Report observed context capacity and provider rate limits in
  `ego usage --json`. (#662-8b00)
- Send provider HTTPS through one explicit HTTP CONNECT proxy with `--proxy` and
  `--proxy-ca`. Ambient proxy variables are ignored. (#694-bb12, #695-f9eb)
- Give foreground `exec` calls a default two-minute limit; `timeout_ms` accepts
  up to ten minutes. (#657-aa93)
- Show one activity row for each live background task, also while the parent is
  idle. (#644-5b91)
- Colour the command syntax in `exec` receipts. (#708-bed3)
- Show the braille startup logo above the identity line. (#721-7fa3)
- Pass lifecycle input and bounded external context to hooks. (#718-0496)
- Ship and install generated shell completions with release archives.
  (#720-67be)

### Changed

- **Breaking:** the public event schema advances from 16 to 18 and adds the
  durable `CostAnomaly` event. (#658-04fa, #684-dc46)
- **Breaking:** the usage-report schema advances from 3 to 4 with context
  capacity and provider-limit observations. (#662-8b00)
- **Breaking:** collapsed transcript text includes hidden-row hints. (#656-caa5)
- Continue generations when prices are unknown, with one cost-anomaly notice per
  session and model. Monetary caps cover recorded spend only. (#658-04fa,
  #698-0beb, #683-76cb)
- The shipped configuration no longer contains the author's personal
  coordinator and owner settings. (#714-cad3)

### Fixed

- Keep headless resume free of implicit interactive retry defaults, and restore
  implicit retry authority when a session resumes on a terminal. (#659-24ef,
  #676-234b)
- Session lists skip empty or unreadable sessions, and a missing ACP session
  names the store that was searched. (#661-dc9a, #681-1be1)
- ACP clients can run every advertised skill command, including
  instruction-only skills. (#665-cbea)
- Send only changes to the host context instead of repeating it in full, and
  send the same skill instructions once, saving tokens. (#666-8001)
- Omit empty process-stream labels from expanded tool output. (#664-a9ef)
- Explain the read-write root grant and the exit and resume needed for writes
  outside the workspace. (#674-8ea1)
- A failed sandbox capability check is retried instead of remembered, and ACP
  configuration options that wait for it end with an error when it fails.
  (#673-a5ba, #675-a29c)
- Persist a successful in-session model switch. (#678-aff6)
- Keep the latest user request verbatim in compaction checkpoints, and render
  fork constraints once. (#670-64e7, #682-b1d1)
- Keep unknown slash-prefixed text as a modal answer. (#677-b92d)
- Keep the prompt usable when the terminal answers a cursor query late, and
  keep the current activity when a modal closes. (#696-3d3a, #697-8064)
- On macOS, admit the Apple launcher runtime in the sandbox and ask for
  permission on Seatbelt selector denials. (#620-7b20)
- Quote outside-root read hints so they can be pasted, and offer an exact read
  grant for a denied shell script read. (#685-b36a, #690-db76)
- Permission prompts say when the sandbox backend changed since you consented.
  (#687-f3b0)
- Make content search refuse special files and bound single-file reads.
  (#693-e660)
- Do not give `pnpm` an unsafe optional lock directory. (#691-d568)
- A command moved to the background just as it finishes reports its result
  once. (#692-c8aa)
- Accept narrowed OAuth scope grants at MCP login, and ignore unused secrets
  for explicit public dynamic client registration. (#476-3479, #711-26ac)
- When a provider stream breaks while the model is only writing text, ego
  retries once on interactive terminals. (#710-6bc0)
- Emit TUICommander marker boundaries only for the TUIC profile. (#719-894c)
- Bound independent `ego doctor` health probes, and doctor no longer blames the
  schema for unrelated task failures. (#723-8046)

### Removed

- Remove automatic MDKB recall in prompts and the `/memory` command.
  (#715-e1f8)

### Security

- Redact encoded Basic credentials while keeping ordinary prose that mentions
  the Basic or Bearer schemes. (#686-8446)
- Update dependencies for RUSTSEC-2026-0381, -0382, -0383, -0385 and -0414, and
  replace the yanked `clipboard-win` version. (#660-d9c8, #722-ab10)

<!-- internal: #419-41d4 #689-d383 #699-1cf1 #702-97b3 #706-2b28 #709-1646 #095-5a9c -->

## [0.3.0] - 2026-10-26

### Added

- Log in to an HTTP MCP server with `/mcp login NAME`; its tools reconnect in the
  current session. (#650-0bc6)
- Show a separate activity row for each running task or named subagent.
  (#643-905e)
- Build five stable executable assets, target-triple archives, SHA256 files, and
  a Homebrew formula. The source installer replaces the executable atomically.
  (#649-a1e6)

### Changed

- **Breaking:** the public event schema advances from 15 to 16 with the transient
  `ChildProgress` event. (#643-905e)
- **Breaking:** `notebook_edit`, `plan`, and `skill` advertise flat object
  schemas, so providers that reject root union schemas accept the catalogue.
  (#647-4807, #652-8e6d)

### Fixed

- Wake an idle interactive session when background work settles, including
  cancelled work. (#648-56e5)
- Read skill resources relative to the captured skill root. (#619-b1b3)
- Keep sanitized upstream error details in diagnostics. (#645-7ab3, #646-b82e)
- Keep a configured Anthropic gateway's base-path prefix. (#636-46de)
- Accept public OAuth dynamic-registration replies without an authentication
  method. (#476-3479)
- Let confined processes signal their own descendants. (#631-5302)
- Recognize Python script read denials in the sandbox grant flow. (#620-7b20)
- Clear settled interrupt and queue indicators from the status row. (#642-325c)
- Use plain next-step wording for refusals and login notices. (#651-d235)

<!-- internal: #095-5a9c #419-41d4 #653-ab6b #655-171a -->

## [0.2.0] - 2026-10-25

### Changed

- Start the 0.2 version line. This version has no functional changes from 0.1.6.

## [0.1.6] - 2026-10-25

### Changed

- No user-visible changes.

## [0.1.5] - 2026-10-25

### Fixed

- Keep completed turns and deadline arbitration through deadline cleanup.
  (#639-605b)
- Report unreadable sessions in history search instead of aborting the search.
  (#638-055c)
- Compaction events no longer expose internal summary sizes. (#637-2d3e)
- Sandbox refusals stay visible and say whether a script could not be read or
  could not be started. (#629-f90a)
- More macOS developer tools run under the Workspace sandbox. (#633-04b6)
- Keep derived Git writes beneath read roots. (#630-e3a8)
- Resume keeps the sandbox choices and consents you already gave. (#632-e9e0)

## [0.1.4] - 2026-10-25

### Added

- Withdraw queued prompts from the first composer line. (#614-7595)

### Fixed

- Recover automatic compaction through shared retries, and fit over-window
  compaction requests. (#635-b46c, #613-2751)
- Release the composer after a provider failure, and keep the failure cause and
  host signal. (#634-bcab, #615-071b)
- Retry once when a request failed before any tool ran, and return sandbox
  refusals to the model instead of ending the turn. (#617-9454, #616-1ec3)
- Ask the native terminal for session path grants, and revoke terminal asking on
  exec promotion. (#620-7b20)
- `pnpm` works in the sandbox, including custom store locations. (#621-0f41)
- Report read-only root execution restrictions, and keep workspace executable
  denials and PATH discovery for MCP servers. (#627-353f)
- Redact secrets in encoded JSON and percent-encoded evidence. (#622-67c2)
- Auto mode's judge sees your steering messages and what MCP tools will do.
  (#623-755e)
- Remember program rules from shell compounds, and label write risk from the
  canonical path. (#624-b940, #626-a4a0)
- Search a single file with grep. (#625-c676)
- Parse slash commands while a question is open. (#628-aab8)

## [0.1.3] - 2026-10-23

### Added

- Render configurable MiniJinja status-line templates. (#525-7399)
- Select permission modes and scope manual session consent. (#586-abe3)
- Support explicit replace-all edits. (#581-4a71)
- Add capability-checked durable audio input over ACP. (#484-2cb4)
- Render plan updates as one compact block, pin running plans above the working
  line, and replace consecutive compact shell rows in place. (#588-1b19,
  #600-5cae, #592-2116)
- Colour retry state and recovery receipts. (#583-adae)
- Publish which session each running ego process owns, for host integrations.
  (#584-6bdc)
- Distinguish steering and next-turn counts in the status row. (#609-203a)

### Fixed

- Keep sessions after transient compaction failures, stop after a required
  compaction fails, and explain automatic compaction. (#607-8db6, #590-480b,
  #606-ffc1)
- Resume explains when a saved model capability file has an unsupported
  version, and diagnostics of unreadable data name the session. (#605-396d,
  #603-9fec)
- Repaint the terminal fully across live activity changes, and keep the composer
  during status row updates. (#601-34ec, #598-8a03)
- Keep slash completion open while erasing a partial command. (#611-044d)
- Keep images with drafts while turns run. (#576-9f83)
- Keep quit responsive by batching queued output. (#571-c2d9)
- Reconnect definitely-unsent reused Codex sockets once, and use system trust
  for Codex WebSocket TLS. (#420-ae5f, #414-72f9)
- Identify conflicting MCP registry tool names, and keep collapsed tool
  descriptors stable. (#610-a485, #414-72f9)
- The model list no longer mixes models from different accounts, and the
  startup picker discovers models without changing anything. (#612-43cc,
  #594-7eb5)
- Recall queued steering messages, accept relative Read paths, and deliver
  bounded MDKB recall (removed in 0.4.0). (#597-5166)
- Read only the requested session from the catalogue, and speed up session
  store appends and followed exports. (#599-0faf, #603-9fec)
- Admit nested read-only launch grants in the sandbox. (#591-a30d)
- Identify busy work in action refusals. (#580-a8ca)
- Keep pending input across cursor queries. (#578-2ad6)
- Keep exec handler results after an early stdin close, and keep owned sockets.
  (#579-a197, #595-dd2e)
- Leave answer prose wrapping to the viewer. (#582-6893)

<!-- internal: #570-9159 #2008-5317 #2010-30a7 #2013-73f9 #419-41d4 #572-f3ba #575-8363 #585-284e #589-875a #602-a391 #604-2046 -->

## [0.1.0] - 2026-10-18

First versioned build. ego is a provider-agnostic coding agent harness.

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
- Model-requested context transitions in interactive sessions: the model can
  compact, or with `--context-clear` clear, its conversation;
  `--context-interval N` sets the minimum completed turns between two
  transitions (default 10), and `--no-context-tool` turns them off. (#417-84dd)
- Inspection of configuration, effective perimeter, usage, and redacted health
  facts with `ego config`, `ego usage`, and `ego doctor`.
