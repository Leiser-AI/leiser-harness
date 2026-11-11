# 0008. Terminals run in the desktop app

Status: accepted
Date: 2026-11-11

The terminal panel's shells run in the desktop app's own process, not in
the harness service. The app asks the service for the chat's checkout (its
worktree, or the repository's main checkout) and starts the developer's
shell there in a pseudo terminal: the login shell on macOS, PowerShell on
Windows. The shells end when the developer ends them and when the app
quits. If the app dies, the operating system closes the pseudo terminals,
which hangs up the programs in them. The PTY code lives in the
`leiser-terminal` crate, on `portable-pty` (wezterm, MIT), which supports
both Unix PTYs and Windows `ConPTY`.

## Considered options

- Shells in the harness service, streamed over the local interface: they
  would survive a closed app. But a service update would end them at a
  moment the developer did not choose, closing the app would have to end
  them anyway so that none are orphaned, and every keystroke and screen of
  output would cross the local interface, which drops events for a client
  that falls behind.

## Consequences

- A terminal does not survive closing or updating the desktop app; agent
  sessions still do (ADR 0005).
- No new commands, responses or events in the local interface.
- The terminal's behaviour is tested through `leiser-terminal`'s API, not
  through the service seam.
