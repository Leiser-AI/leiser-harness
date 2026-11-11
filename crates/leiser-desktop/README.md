# leiser-desktop

The desktop app (ADR 0004): a Tauri shell with a TypeScript UI in `web/`.
It renders what the harness service reports and holds no chat state of its
own.

For v1 the app's Rust side hosts the harness service in-process and
forwards its local interface 1:1 (`execute` command, `service-event`
events). The service keeps its data in the app's data directory (on macOS
`~/Library/Application Support/ai.leiser.harness`).

Chats start on the installed Claude Code; Codex is configured as a second
agent CLI. An app started from the Finder does not get the shell's `PATH`,
so the harness service also looks in common install directories and, for
Claude Code, at the copy the Claude desktop app bundles (see
`crates/leiser-service/src/installed.rs`). To run another program, set
`LEISER_CLAUDE_CODE` or `LEISER_CODEX` to its path.

## Run it

Needs Node.js (LTS) and, on Linux, WebKitGTK (see `.github/workflows/ci.yml`).

```bash
cd crates/leiser-desktop
npm install
npm run tauri dev
```

To try the app without a subscription, run it on the testkit's fake agent
CLI, which answers every message with a tool call, its result and an echo.
Its chats are kept apart from real ones.

```bash
cargo build -p leiser-testkit --bin fake-agent-cli
LEISER_FAKE_AGENT_CLI="$PWD/../../target/debug/fake-agent-cli" npm run tauri dev
```

## Checks

- `npm run build` type-checks and builds the UI (also in CI).
- No automated UI tests in v1; check the app by hand.
- `web/src/protocol.ts` mirrors `crates/leiser-service/src/protocol.rs`;
  change both together.
