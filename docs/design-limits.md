# Design limits

Check this record before filing an ego bug. Add an entry when a reported behavior is verified as part of the design, not a defect. State what was observed, why it is intentional, a practical workaround if one exists, the source location or commit, and the date. Update an entry if the design changes.

## An uncertain append can prevent session finalization

- **Boundary:** A store error can arrive after a journal record was written. Finalization refuses to return state while the record's durability is unresolved. It reports the original append error without replaying the append or claiming the record is durable.
- **Workaround:** Resolve the reported store error and use the `ego resume ID` command shown in the diagnostic to reopen the saved journal before repeating an action.
- **Source:** `SharedJournal::try_into_state` in `crates/ego-core/src/turn/journal.rs` (#1190-7d39).
- **Date:** 2026-11-23.

## An Auto tool proposal is not a preview

- **Observed:** A live Auto judge returned `allow` for a synthetic file read despite an explicit request for review only; the runtime then executed the read. The verdict was fresh, not reused from cache.
- **Boundary:** Executable tool calls go through execution admission. If the Auto judge authorizes the call, no second question is required. Natural-language review wording is not a runtime no-effect control; this does not make the judge's mistaken authorization expected behavior.
- **Workaround:** For review without effects, inspect the proposed command without submitting it as an executable tool call. Read-only confinement alone does not block reads.
- **Source:** `TurnLoop` judge settlement in `crates/ego-core/src/turn/mod.rs`; [live control and proposed review entry point](internals/2026-11-20-verifier-cache-0.4.16.md#runtime-boundary-and-prompt-mitigation).
- **Date:** 2026-11-20.

## Image publication requires filesystem no-clobber support

- **Observed:** The image filesystem adapter propagates an error if its atomic create-only publication cannot run. It never falls back to replacing the target. This is verified in source; no unsupported volume was tested.
- **Why by design:** The no-overwrite promise takes priority over producing an image on every filesystem. On Unix, the pinned `tempfile` 3.27.0 adapter uses a no-replace rename where available and, for selected unsupported-operation errors, a hard-link fallback. Failure of those operations is returned to the caller.
- **Workaround:** Select an output directory on a filesystem that supports the required operation. The image-generation request can complete before publication fails; this boundary does not cancel or retry that request.
- **Source:** `PreparedReplacement::commit_new` in `crates/ego-tools/src/fs.rs`; dependency `tempfile` 3.27.0, `src/file/imp/unix.rs`, `persist`. No specific filesystem or network volume is classified as supported or unsupported by this evidence.
- **Date:** 2026-11-10.

## Turn metadata and host activity

- **Observed:** Rich turn events do not by themselves make a host show a turn as busy.
- **Why by design:** The host activity signal is separate from native turn metadata. Ego emits a coarse busy signal at turn start and idle at turn completion.
- **Workaround:** When diagnosing an activity indicator, inspect the coarse activity signals as well as the turn events; do not infer host state from metadata alone.
- **Source:** `crates/ego-cli/src/run/interactive_driver.rs`, `Announce::signal`.
- **Date:** 2026-11-06.

## Ctrl+B promotes exec, not arbitrary tool calls

- **Observed:** Pressing `Ctrl+B` while the Working row names `call_tool` reports `no running exec to background`.
- **Why by design:** The shortcut promotes an admitted foreground `exec` process to the session-owned detached-exec registry. It does not detach an arbitrary MCP tool request; the row names the current model-requested tool, not an executable process.
- **Workaround:** For a command, request `exec` with `background=true` or promote an active foreground `exec` with `Ctrl+B`. For a standalone agent, request `task` with `background=true`. MCP Tasks require the opt-in and support from the server; they do not make `Ctrl+B` promote a live call.
- **Source:** `CLI.md`, sections on foreground `exec`, background tasks and the interactive surface; `crates/ego-cli/src/repl/seat/action_execution.rs`, `HostCommand::Background`.
- **Date:** 2026-11-06.

## Approval does not change confinement

- **Observed:** A command can still be refused after approval or in `yolo` mode.
- **Why by design:** Permission mode and operating-system confinement are separate authorities. Approval does not lift the sandbox, and `yolo` does not switch it off.
- **Workaround:** Inspect `/config perimeter` and the refused path; select an appropriate sandbox and roots when starting the session.
- **Source:** [Permissions and confinement](guides/permissions.md#permission-modes); commit `7c1a6714e`.
- **Date:** 2026-11-06.

## No approval prompt in headless work

- **Observed:** Headless work exits with status 3 when a required approval is unavailable rather than asking a person.
- **Why by design:** Headless work has no interactive approval seat.
- **Workaround:** Arrange the appropriate permission before starting the headless run, or use an interactive session.
- **Source:** [Permissions and confinement](guides/permissions.md#permission-modes); commit `b15759c63`.
- **Date:** 2026-11-06.

## Remembered shell commands are exact

- **Observed:** A remembered permission for a shell command does not match a different script, even when the scripts run similar commands.
- **Why by design:** The rule records the exact rendered argument list, including the script; it is not a general grant for the shell or commands inside it.
- **Workaround:** Preview and approve the new invocation with `ego rules remember`, or use the interactive approval prompt.
- **Source:** [Permissions and confinement](guides/permissions.md#rules-and-remembered-decisions); commit `7c1a6714e`.
- **Date:** 2026-11-06.

## A configuration error never stops ego, and never widens authority

Decided 2026-11-08. A configured root that no longer exists, or cannot be
resolved, is skipped or narrowed with one note on stderr and one in the model's first request;
ego starts. The limit is authority: an entry that restricts keeps restricting through its
nearest existing canonical ancestor plus the missing tail, and only entries that would grant
access are dropped. Other configuration errors (an unparsable file, an unknown key) keep their
own handling; this contract covers perimeter roots only.

## Session cost is not reconciled with provider invoices

- **Observed:** `ego usage` reports recorded session cost, but it does not show the amount that the provider invoice charges for that session.
- **Why by design:** The audited provider cost APIs aggregate by organization, workspace, API key, actor and model, with time buckets. They do not identify an ego session, so a session share of an invoice can only be guessed. Ego records the cost that the provider reports in a response when the wire carries it, or prices the recorded token usage at response time. Recorded cost is never repriced.
- **Workaround:** To compare with an invoice, use an API key or workspace that only ego uses, and compare totals for the same period in the provider console.
- **Source:** `crates/ego-core/src/cost.rs`; audit of the provider cost APIs.
- **Date:** 2026-11-08.

## Auto-mode judge evidence and selection

- **Observed:** An unset `permissions.judge` follows the main model and can use a small model. Judge evidence includes at most 256 roots and the 50 most recent session answers. A cross-provider judge sees a structural command skeleton instead of raw argv.
- **Why by design:** Following the main model avoids imposing a separate model choice. Root and precedent limits bound prompt size. The cross-provider projection limits disclosure of command arguments to another provider. Session approval fingerprints are separate from the bounded judge precedents.
- **Workaround:** Set `permissions.judge` to an explicit provider/model when a different judge is needed. For missing context or arguments, review the resulting question and provide the necessary authority through the normal approval flow.
- **Source:** `crates/ego-core/src/permit/judge.rs`, `crates/ego-core/src/permit/session_approvals.rs`.
- **Date:** 2026-11-08.

## The ego administration check recognises direct use only

- **Observed:** A model can still change ego's configuration through a script or an interpreter that calls `ego` itself, such as `python3 -c`, `node -e` or a shell script in the workspace, through `cargo run -- roots add`, through `sudo -u user ego`, or through an alias or a renamed binary.
- **Why by design:** The check reads the argv and shell command lines for `ego` in the position where a program runs, through launchers such as `env`, `nice`, `timeout` and `xargs`, nested `sh -c`, `eval`, command substitutions and here-documents. It is a policy against direct use, not a sandbox: the process that a script starts is not known before it runs. An unknown program is asked about anyway, and the control files of ego are protected paths for sandboxed processes.
- **Workaround:** Keep the sandbox on and the user configuration directory out of writable roots. Review an interpreter or script that mentions `ego` before approving it.
- **Source:** `crates/ego-core/src/permit/owner_administration.rs`.
- **Date:** 2026-11-09.

## The terminal host owns the scrollback viewport

- **Observed:** When the operator scrolls the terminal back and presses Down, ego does not move the view back to the input. ego has no Down action for this.
- **Why by design:** ego paints its input inline on the primary screen, so the scrollback and its display offset belong to the terminal host. No standard control sequence lets an application scroll the host viewport without writing rows. The supported host, TUICommander, already handles this: while its view is scrolled back, a Down with no modifier jumps to the bottom and is not sent to ego. ego's draft, caret, menu and history do not change. At the bottom, Down goes to ego, and ego keeps its menu, multiline and history behavior. Other terminal hosts were not verified.
- **Workaround:** In TUICommander, press Down or Cmd+End, or type a key: input also returns the view to the bottom. In another host, use the host's scroll-to-bottom command.
- **Source:** `tuicommander/src/components/Terminal/CanvasTerminal.tsx` (keydown handler and `writePty`); `crates/ego-cli/src/repl/seat/edit_mode.rs`.
- **Date:** 2026-11-14.

## Long exec answers show the last 4 KiB, not every diagnostic

- **Observed:** When the text of an `exec` stream is longer than 4 KiB after progress folding, the model answer shows only its last 4 KiB and a `read_tool_result` reference. This applies to every UTF-8 output, build or not, and to failures. On recorded failing test runs the answer kept every failing test name from the final summary and the exit code, but not most panic messages: 0 of 2, 0 of 9 and 1 of 7. A stream above 64 KiB is first cut to its first and last 32 KiB, so the middle of a long log is never in the answer.
- **Why by design:** The answer is bounded so that a long build cannot fill the context. The complete stdout and stderr stay in the session evidence, and the exit status stays visible. Line rules that are learned for one command apply only to successful runs, and on long recorded outputs their 6 KiB bound falls back to the complete answer. A selection of diagnostics from the whole log needs a reviewed contract and an owner decision first.
- **Workaround:** Read the complete output with `read_tool_result` (32 KiB pages), or run a narrower command, for example one test filter.
- **Source:** `crates/ego-core/src/exec_output.rs`, `crates/ego-core/src/permit/tool.rs` (`compact_process_progress`), `crates/ego-tools/src/exec/process.rs` (`BoundedPreview`); CLI.md section 7.5.
- **Date:** 2026-11-14.

## Empirical cache learning excludes helper-interleaved cohorts

- **Observed:** The three-cycle cache learner resets on an admitted judge, goal-check, hook or compaction request in the same session. Mirrored child request starts also reset the empirical cohort, even when their accounting records omit cache metadata.
- **Why by design:** A helper can refresh or replace the same provider prefix. Its request does not carry the main request's continuity proof, so combining observations across it would not establish one comparable cohort. Mirrored child starts represent actual requests and can refresh a shared lineage prefix.
- **Workaround:** Use a known provider TTL or an explicit profile TTL when helper-interleaved sessions need an idle deadline. Such a fallback is not empirical evidence.
- **Source:** `SessionState` `GenerationStarted` reducer.
- **Date:** 2026-11-14.

## Empirical cache learning needs full-input transport proof

- **Observed:** Incremental connection-local continuations and requests without actual dispatch metadata do not contribute to the session cache lifetime estimate. Mixing HTTP, fresh WebSocket and reused full-input WebSocket dispatches resets the cohort.
- **Why by design:** The logical input hash does not prove that a connection-local continuation has the same cache retention as a full-input request. The adapter records the actual dispatch mode in the settled journal; it does not predict the mode before the send. Three similar cold gaps also need a nearby confirmed hit before any idle deadline is published.
- **Limits:** A confirmed hit must report cache reads that cover the entire previous input total. Partial positive reads reset the cohort, including reads only one token short. If the provider reports cached input in blocks, this strict rule can prevent learning; cached-token granularity and convergence on the subscription endpoint have not been measured. Together with excluded steady-state continuations, this can leave `cache=unknown` indefinitely. No practical convergence claim is made without a live trace. The estimate assumes comparable full-input requests populate or renew the cache, applies only to the observed transport mode, and can be inflated by an unobserved actor that resends the same prefix. It is not a provider guarantee. Forward wall-clock changes can also inflate observed gaps; only backward changes are rejected.
- **Workaround:** A known request TTL or explicit profile TTL remains authoritative. No model or transport equivalence or token-block tolerance is inferred to make an estimate appear.
- **Source:** `CacheTransport`, `CacheLifetimeLearning`, adapter dispatch and attempt settlement.
- **Date:** 2026-11-14.

## Session roots do not cross an in-process session change

- **Observed:** `/compact`, `/clear`, `/new` and `/fork` refuse a session change when the source holds session roots. Same-ID resume keeps those roots and cannot remove the blocker.
- **Why by design:** The process policy and OS guard were admitted for the source. A successor must not borrow its session-only filesystem authority.
- **Workaround:** Keep the source open, save a factual handoff and settle session-owned background work, then start a fresh `ego` session with independent admission, not `ego resume SOURCE`. Alternatively, inspect `/config roots` and explicitly revoke every session-scoped root with `/config roots revoke EXACT_PATH` if that access is no longer needed. Revocation stops live exec children and re-admits the source before another transition. Neither the refusal nor fresh operator input transfers grants. A separate cold compaction excludes session roots but retains owner sandbox grants; do not use it for a zero-grant-transfer requirement.
- **Source:** `crates/ego-cli/src/run/driver_runtime.rs`.
- **Date:** 2026-11-17.

## A new interactive input phase does not retain the previous provider socket

- **Observed:** After a completed turn leaves the native seat waiting for input, the next input phase binds a new generator to its cancellation token and effect ports. The first Codex request in that phase is a full request on a fresh WebSocket, even if the encoded model, system text and tools have not changed. Subsequent requests inside that phase can continue on that socket.
- **Consequence:** The provider can still read a cached prefix on the full request. A lower read ratio does not by itself establish a local prefix invalidation; the footer reports cumulative token-weighted cache reads, not the last request's read ratio.
- **Boundary:** Reusing the socket across phases requires a connection owner independent of the prior turn's cancellation token, deadline and effect ports. Reusing the existing transport object would inherit the prior turn's authority.
- **Source:** `crates/ego-cli/src/run/driver_runtime.rs`, `crates/ego-cli/src/model.rs`, `crates/ego-ai/src/codex_session.rs`. Local journal evidence in the 2026-11-21 native task/cache report.

## An external login does not rebind an open session

- **By design:** Saving an account in another process does not change an open
  session's binding or authorize sending its history to that account.
- **Explicit selection:** Reopen `/model` in an unbound session to select an
  eligible saved account, including one saved after startup. The confirmed
  selection binds the session and its subsequently admitted background tasks;
  it does not change the configured default. A cross-provider selection in the
  terminal starts a separate fork; it does not rebind the original session.

## Subscription image access is not bound to named accounts

- **Observed implementation gap:** The subscription image backend reads a provider-wide `openai-codex` credential slot. Provider-only login creates a named connection, and targeted login repairs a named slot. Neither updates the separate slot read by this image backend.
- **Status:** This is an unfinished implementation, not an intentional final-design limit. The provider-wide credential slot is not a compatibility contract.
- **Current safety boundary:** Named-account image access is not implemented. When the captured run configuration contains any named connections or selects one, composition does not register or read the provider-wide subscription backend. An explicitly configured API image key remains a separate, consent-gated tool backend; no account or paid fallback is added.
- **Consequence:** If the provider-wide image credential expires, creating or repairing a named connection does not restore subscription image access. The expiry error states this limit; named-account image resolution remains unfinished.
- **Source:** `crates/ego-cli/src/image_access.rs::CodexSubscription::from_home`, `crates/ego-cli/src/run/composition.rs::tools_with_mode`, `crates/ego-cli/src/auth.rs::publish_login`.
- **Date:** 2026-11-17; account-isolation gate updated 2026-11-19.


## Vertex Gemini ADC refresh

- Built-in refresh supports Google user ADC with OpenID access. It verifies the account through Google's UserInfo endpoint before saving the token.
- The selected connection still reads the current ADC file. Set `GOOGLE_APPLICATION_CREDENTIALS` to the matching local file before refreshing a connection. Existing valid stored tokens remain bound to the original Google identity until expiry.
- A configured credential command owns identity verification for the tokens it supplies.
