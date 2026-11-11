# leiser-harness

An agent harness for one developer's daily work: it runs coding agents on
the developer's own subscriptions.

## Language

**Provider**:
A company that offers models, such as Anthropic or OpenAI.
_Avoid_: vendor, lab, backend

**Subscription**:
One way the harness is allowed to use a provider's models: a plan signed in
by login, such as "Claude Max private" or "Claude Team work", or an API key.
A provider can have several.
_Avoid_: abo, account, plan, credential

**Agent CLI**:
An existing coding agent program from a provider that the harness runs,
such as Claude Code or Codex.
_Avoid_: backend, engine, runner, adapter

**CLI extension**:
A skill, MCP server, hook or instruction file (such as `AGENTS.md`) that an
agent CLI loads by itself. The harness can show and edit it, but it is not
part of the harness.
_Avoid_: plugin, add-on

**Chat**:
One conversation of the developer with agents about one task. It keeps going
when the model, provider or agent CLI changes, and consists of one or more
agent sessions.
_Avoid_: thread, conversation, session

**Agent session**:
One conversation inside one agent CLI. A chat that changes agent CLI
continues in another agent session.
_Avoid_: session (on its own), run

**Permission mode**:
How much an agent session may do without the developer's approval:
Supervised, Auto-accept edits, Auto or Full access. Set per chat.
_Avoid_: runtime mode, approval policy, trust level

**Handoff**:
The part of a chat that the harness gives a new agent session so it can
continue: a selection of the chat's real messages, not a summary.
_Avoid_: summary, context transfer, migration

**Harness service**:
The part of the harness that runs in the background and keeps agent
sessions running while the desktop app is closed or updated.
_Avoid_: daemon, server, backend

**Git host**:
A service that hosts repositories, such as GitHub or GitLab.
_Avoid_: remote, forge, platform

**Host account**:
One user account on a git host, such as a private and a business GitHub
account. Every repository is bound to exactly one host account.
_Avoid_: GitHub account, profile, login, identity
