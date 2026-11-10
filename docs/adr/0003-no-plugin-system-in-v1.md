# 0003. No plugin system in v1

Status: accepted
Date: 2026-11-10

v1 has no plugin system: no plugins and no extension points. The harness is
extended by changing its own code in this repository. One inspiration
(deepseek-harness) makes everything a plugin. We rejected that for v1
because a plugin API carries a real risk to clean code and to performance,
and its only user also develops it.

This narrows ADR 0002: the harness owns no plugins. The driver interface
from ADR 0002 stays an ordinary internal interface, not a plugin API.

## Considered options

- Rust crates compiled into the harness: adding or updating one needs a new
  build and a restart, which would interrupt running agents.
- WASM modules loaded at runtime: cannot start subprocesses, so drivers
  would not fit, and the host setup is large.
- External processes over a stdio protocol: swappable at runtime in any
  language, but needs a versioned protocol and one process per plugin.

## Consequences

- Features that were candidates for plugins (panels such as a todo list or
  statistics, tools that agents can use, checks for best practices) are
  built into the harness.
- CLI extensions (skills, MCP servers, hooks, instruction files) belong to
  the agent CLI. The harness can show and edit them but does not manage or
  translate them.
- Because every extension is a change to the harness, updating the harness
  must not interrupt running agents.
