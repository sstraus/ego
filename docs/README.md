# εgo documentation

εgo supplies the operational layer between a model and work on your machine.
These guides describe the implemented user surfaces in this checkout. Start with
what you want to do; each guide explains the feature before its controls.

This is also the introduction to the mdBook user guide. Its sidebar follows
[the book summary](SUMMARY.md): user tasks first, then integrations and reference.
Build with `mdbook build docs` from the repository root, or use `mdbook serve docs`
for a local preview. See [build and maintenance](maintaining.md) for requirements,
checks, and output paths. No hosted website is implied.

| Your task | Guide |
| --- | --- |
| Install, choose a model, and send a first prompt | [Getting started](getting-started/README.md) |
| Understand the units of work and uncertainty | [Concepts](concepts/README.md) |
| Work in a terminal, steer, and use slash commands | [Interactive use](guides/interactive.md) |
| Run in scripts, validate JSON, retry safely, and use agent files | [Automation](guides/automation.md) |
| Find a command, option, default, or output contract | [CLI reference](reference/README.md) |
| Configure a model, profile, perimeter, environment, or extension | [Configuration reference](reference/configuration.md) |
| Use local, API-key, or subscription models | [Providers and login](guides/providers.md) |
| Understand the model's tool surface | [Tools](guides/tools.md) |
| Set authority and confinement | [Permissions and confinement](guides/permissions.md) |
| Resume, branch, compact, inspect usage, and export | [Sessions and history](guides/sessions.md) |
| Isolate work in a retained Git checkout | [Git worktrees](guides/worktrees.md) |
| Initialize instructions and review persistent memory | [Instructions and memory](guides/memory.md) |
| Connect ACP, MCP, A2A, skills, hooks, or TUICommander | [Integrations](integrations/README.md) |
| Implement an ACP client for ego's wire extensions | [ACP extensions](integrations/acp-extensions.md) |
| Diagnose a refusal or platform limitation | [Troubleshooting](guides/troubleshooting.md) |
| Read the locally prepared release changes | [Release notes: 0.4.0](release-notes/0.4.0.md) |
| Understand crate responsibilities and design | [Architecture](architecture.md) |

The [generated command reference](reference/cli.md) remains at its generator-owned
path. [Configuration](reference/configuration.md) identifies the source schemas.
[CLI.md](../CLI.md), [SPEC.md](../SPEC.md), and [ARCHITECTURE.md](../ARCHITECTURE.md)
remain design authorities; they include planned behavior and older examples, so
use the source-backed guides and current binary help for invocation details.

[Internals](internals/README.md) contains dated design decisions and historical
research. These are not installation instructions or current provider guarantees.
