# 0002. Drive agent CLIs instead of an own agent loop

Status: accepted
Date: 2026-11-10

## Context

The harness must work on login subscriptions without API costs, and must
also accept API keys. One inspiration (deepseek-harness) runs its own agent
loop; the other (t3code) drives the providers' agent CLIs.

## Decision

v1 drives existing agent CLIs as subprocesses: Claude Code for Anthropic,
Codex for OpenAI. It talks to them through their machine protocols, not by
embedding their terminal UI. The harness has no agent loop of its own.

The agent CLI owns the loop, its built-in tools, context and compaction.
The harness owns chats, worktrees, the choice of subscription, approval
rules, the user interface, plugins and observability.

All agent CLIs sit behind one interface with one driver per agent CLI:
start, send, read events, approve, interrupt, queue, steer, resume. Steer
falls back to queue when an agent CLI cannot take a message mid-turn.

## Considered options

- Own agent loop: full control and any provider, but loop, tools and
  compaction would have to be built and kept up to date. The agent CLIs
  already accept both login subscriptions and API keys.
- Embedding the CLI's terminal UI in a PTY: no structured view of
  messages, approvals or costs.

## Consequences

- Only providers with a supported agent CLI can be used in v1.
- The harness uses the agent CLIs installed by the user and checks a
  minimum version.
- A chat can change model between turns, and with it the agent CLI. The
  harness must hand the chat over between agent sessions.
- An own agent loop can be added later as another driver behind the same
  interface. It is out of scope for v1.
