# Integrations

Integrations connect another tool or host to εgo's existing authorities. They do
not create a second permission model or turn remote content into trusted inputs.

| Integration | Direction and purpose |
| --- | --- |
| [IDE setup](ide.md) | Launch an existing ACP client without an ego extension; attach untrusted editor snapshots |
| [ACP and TUICommander](acp.md) | A host drives durable sessions and renders prompts, tools, permissions, and controls |
| [Standard ACP client libraries](acp-clients.md) | Drive ego with pinned TypeScript, Python, and Rust libraries, without an ego SDK |
| [MCP](mcp.md) | Attach external model tools, or expose εgo's rooted run controller to an MCP client |
| [A2A](a2a.md) | Publish εgo discovery on an admitted local endpoint; task delegation is not implemented |
| [Skills](skills.md) | Add named, progressively disclosed instruction capabilities |
| [Hooks](hooks.md) | Add subtractive lifecycle checks/feedback under existing authority |

For configuration fields, defaults, and source files, see
[configuration reference](../reference/configuration.md). The
[generated command reference](../reference/cli.md) lists launch surfaces;
current binary help covers nested management commands.
