# ACP test UI

A browser harness for `ego acp`. It spawns the agent, speaks stdio JSON-RPC at
it, and shows both what the wire carried and what a host would render.

```
node tools/acp-test-ui/server.js            # http://127.0.0.1:7788
node tools/acp-test-ui/server.js --port 9000 --bin /path/to/ego
```

No dependencies, Node only. Press `start`, then click the calls in order:
`initialize` → `session/new` → `session/prompt`.

## What it does not do

It never answers the agent on the client's behalf. An agent-to-client request
(`elicitation/create`) appears as a pending card with a prefilled but editable
answer, and it stays pending until you send one — a harness that auto-answers
cannot show a client-side mistake.

## Why `$EGO_HOME` and not `.ego/config.local.toml`

`ego acp` is headless, headless always runs untrusted and bare, and an
untrusted run drops the `Project` and `Local` config layers entirely — a
`sandbox` written into the workspace is read by nobody. The user layer is the
only one an ACP host can reach, so the harness points `$EGO_HOME` at
`ego-acp-ui-home` in the system temp dir (`$TMPDIR`) and writes `model`,
`sandbox` and `mode` there. Your real `~/.ego` is never touched.

This is also the finding the harness exists to make visible: **`ego acp` takes
no `--sandbox`, no `--mode` and no `-m`, and ACP has no negotiated way to set
them**, so an ACP host inherits whatever the user layer happens to say.

## Panes

- **conversation** — `session/update` rendered the way a host would: message
  chunks coalesced per `messageId`, thoughts, tool calls with their status and
  output, `stopReason` and any error.
- **wire** — every line in both directions, unmodified, plus the agent's
  stderr and its exit code.
