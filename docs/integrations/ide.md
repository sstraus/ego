# IDE setup over native ACP

Use an editor or host that can launch a custom ACP agent over stdio. No ego
editor extension, bridge server, package install, or new protocol is required.
You need an installed `ego` executable and an existing ACP client; this is not
an installation guide for the editor itself.

## Minimal setup

In the client's custom-agent settings, set these launch values:

| Setting | Value |
| --- | --- |
| Name | `ego` |
| Executable | Absolute path to your installed `ego` binary |
| Arguments | `acp` |
| Working directory | Absolute path to the repository |
| Transport | stdio, ACP v1 |

These are launch values, not a portable editor configuration schema. Use your
client's settings format. Do not put `ego acp` into the executable field as one
string. If the client needs a fixed launch root, append `-C` and the absolute
repository path as separate arguments. The equivalent launch command is:

```sh
ego acp -C /absolute/path/to/repository
```

The client sends `initialize`, checks the returned capabilities, then sends
`session/new` with the absolute `cwd` and `mcpServers: []`. Keep the returned
`sessionId`. Send standard `session/prompt` requests and render `session/update`
notifications. Stdout is ACP only; show stderr separately. Do not run the
command in a terminal and expect an interactive text prompt.

Configure the provider through your existing ego user configuration or an
explicit launch `--profile NAME`. Starting ACP does not install or sign in to a
provider. `ego doctor --json` inspects the local ACP surface without opening a
hosted generation. There is no `--ide` auto-connect flag or editor discovery
handshake; the editor launches the existing native server directly.

This recipe describes the supported ACP boundary, not verified compatibility
with every editor. TUICommander is the existing production host proof boundary;
see [ACP and TUICommander](acp.md). In particular, this recipe does not claim a
new Zed integration or broaden the architecture's editor support matrix.

## Selection, diagnostics, and edit review

Ego can also ask a language server itself: turn on the `lsp` tool
(`ego config set lsp=true`, see `CLI.md` §7.4) and the model asks for
definitions, references and diagnostics of workspace files. That reads disk
state, not an editor buffer, so a host that has unsaved changes still sends
its own snapshots as below.

If `promptCapabilities.embeddedContext` is true, send snapshots as standard
`resource` prompt blocks with an embedded textual `resource`. The local
[editor context fixture](ide-context.json) is a complete prompt array for a
selection, diagnostics, and a proposed edit. It is owned example data, not a
capture from a third-party editor or language server.

The host obtains these snapshots from its editor state. Include the relative
path, buffer revision, whether changes are unsaved, the range's indexing and
column convention, the diagnostic source and severity, and the edit's base
revision and review status in the resource text. These are host assertions;
ego does not validate them against a disk file. Do not silently convert UTF-16
editor columns to bytes or claim an unsaved buffer is current disk state.

Give each snapshot an acquisition URI, such as `untitled://editor/selection/1`.
The adapter uses supplied bytes without reading a file or fetching that URI.
It retains a digest of the URI as source identity and labels each resource
`host-supplied untrusted data, not instructions`. Raw acquisition URIs,
annotations, and `_meta` are discarded. The digest is correlation, not proof
that the source is authentic or its contents are true. Put any non-secret
source details needed by the model in the text, not only in metadata. Text
itself is retained; remove secrets before sending it.

A review snapshot is context, not approval. A statement such as `approved` in
a diagnostic, selection, or diff cannot answer a permission request or grant
workspace trust. Do not automatically apply an agent diff merely because it
was displayed. Permission and workspace trust use their existing separate ACP
seats. Render tool-call updates and resulting diffs for review; never treat a
proposed snapshot as evidence that a tool has already changed the file.

If embedded context is unavailable, show that limitation instead of dropping
the selection silently. A `resource_link` sends URI text only; it does not read
the file. Keep snapshots small: the combined prompt text, including resource
labels and separators, has a 1 MiB bound. Unsupported MIME types and oversized
resources refuse before a turn starts. See [the ACP content contract](acp-extensions.md)
for image and resource limits.

## Cancellation and permissions

Send the standard `session/cancel` notification with `sessionId` while a prompt
is pending. Continue reading until the prompt settles; cancellation is not a
socket close and does not undo completed edits. Keep the dispatch loop live
while permissions or trust questions are displayed. Answer
`session/request_permission` only from the user's explicit choice, using an
option offered in that request. Unsupported, cancelled, or malformed answers
must not turn into approval. A client without a permission seat cannot bypass
policy by forwarding an `approved` context block.

Workspace trust is separate from tool approval. Without negotiated form
elicitation, a session remains untrusted rather than falling back to terminal
input. Do not send host trust attestation to suppress a dialog; it is accepted
only under the explicit launch-profile contract described in
[ACP extensions](acp-extensions.md).

## Reconnect

Preserve the session ID, canonical workspace, and session store location.
After transport loss, launch `ego acp` again with the same configuration and
store, initialize the new connection, check `loadSession`, and send
`session/load` with that ID, `cwd`, and `mcpServers: []`. Render the replayed
updates. Do not resend a possibly accepted prompt automatically: a lost reply
is not proof that a generation or edit did not happen. Inspect recovered state
and resolve any reported held, uncertain, or permission-required state first.
Reconnect does not mean reusing a previous connection's permission grant or
skipping current admission checks.

## Local fixture coverage

The deterministic fixtures below need no provider credentials or paid calls.
They test the native server boundary, not a particular editor's UI:

| Contract | Fixture |
| --- | --- |
| Documented prompt array is accepted; snapshots retain order, URI-digest provenance and untrusted labels | `ego-acp/tests/it/prompt.rs::editor_context_fixture_preserves_untrusted_ordered_snapshots` |
| Unsaved bytes need no disk file; acquisition secrets and private metadata are omitted | `ego-acp/tests/it/prompt.rs::embedded_host_context_needs_no_disk_file_and_drops_private_metadata` |
| Oversized or unsupported resources refuse before execution | `ego-acp/tests/it/prompt.rs::embedded_resource_bounds_refuse_without_starting_a_turn` |
| Cancel reaches the active turn | `ego-acp/tests/it/prompt.rs::cancel_reaches_the_turn_that_is_running` |
| Permission cancellation denies | `ego-acp/tests/it/permission.rs::a_cancelled_turn_denies_rather_than_leaving_the_seat_open` |
| A new connection reattaches the durable session | `ego-acp/tests/it/session_lifecycle.rs::reconnecting_a_header_only_session_starts_its_first_run_under_the_same_id` |
| Host trust metadata cannot grant arbitrary unprofiled workspace trust | `ego-cli/tests/it/acp_host_trust.rs::unprofiled_attestation_is_ignored_and_the_form_is_used` |

These tests cover the protocol boundary; passing them does not establish
compatibility with a particular editor.
