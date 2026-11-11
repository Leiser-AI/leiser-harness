# 0005. Agent sessions run in a harness service

Status: accepted
Date: 2026-11-10

Agent sessions run in a background harness service written in Rust, not in
the desktop app. Every extension of v1 is a change to the harness itself
(ADR 0003), so updates are frequent, and an update must never interrupt a
running turn. The desktop app can be updated at any time. The service
delays its own update until no turn is running, then restarts and resumes
all agent sessions.

## Considered options

- Agent sessions inside the desktop app: simpler, but every update or
  closed window interrupts running agents.
- A service that asks before updating (as t3code does): still interrupts
  turns if the user accepts.

## Consequences

- The desktop app and the service talk over a local interface and are
  versioned separately.
- Every agent session must be resumable after the service restarts.
