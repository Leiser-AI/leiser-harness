# 0006. Store chats as versioned JSON files

Status: accepted
Date: 2026-11-11

The harness service keeps its data in one data directory: `state.json`
holds repositories, chats and agent session ids, and each chat's transcript
is a JSON Lines file under `chats/` that is only appended to. `state.json`
carries one storage version for the whole directory. A newer harness
service migrates older versions when it starts; an older one refuses data
of a newer version instead of damaging it. A test loads data of every
released version from a fixture, so a breaking change cannot go unnoticed.

## Considered options

- SQLite (`rusqlite`): transactions and queries, but a bundled C library in
  every build and a binary format the developer cannot read.
- One JSON file for everything: simple, but every agent event would
  rewrite the whole file.

## Consequences

- No new storage dependency; `serde_json` is already used.
- `state.json` is replaced atomically (write a temporary file, then
  rename); a crash can only cut off a transcript's last line, which is
  removed on the next start.
- Reading a part of a transcript reads the whole file. If transcripts grow
  too large for that, revisit this decision.
