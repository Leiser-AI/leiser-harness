# 0007. The harness service runs as its own process

Status: accepted
Date: 2026-11-11

The harness service (ADR 0005) runs as its own program,
`leiser-harness-service`, apart from the desktop app. One service runs per
data directory; a lock file in it keeps a second one out. The desktop app
starts the service when none runs and connects to it over a local interface:
line-delimited JSON over TCP on `127.0.0.1`. The service writes its port and
a random token to `local-interface.json` in its data directory, readable by
the developer only; a client must send the token first. Both sides send
their local interface version in a hello. The version changes only when an
existing frame, command, response or event changes shape; new ones keep it,
and a client skips frames it cannot read.

An update replaces the service's program file, for example with the desktop
app's bundle. The service checks its program file every second. When it
changed, the service announces that an update waits, keeps every turn
running, including queued messages and turns that wait for an approval, as
well as worktree setup and repository scripts. Once none of that runs, it
ends its agent sessions, starts the new program on the same data directory
and exits. The
new service resumes every agent session. Closing or updating the desktop app
only closes its connection; the app connects again, and starts a service
itself if none comes back within a few seconds.

## Considered options

- Unix domain socket and Windows named pipe: file permissions instead of a
  token, but two transports to build and test, or a new dependency.
- The app tells the service about an update: the update then needs the app
  to be open. Watching the program file also covers updates while the app
  is closed.
- Hold new turns while an update waits, so the update cannot be delayed
  without end: the developer would then be blocked by an update. Messages
  still start turns; revisit if updates wait too long in practice.

## Consequences

- No new dependency: TCP and JSON come from `tokio` and `serde_json`.
- The service keeps running after the app quits, until it is stopped or
  updated.
- The app bundle must ship `leiser-harness-service` next to the app's own
  program (not set up yet for release bundles).
