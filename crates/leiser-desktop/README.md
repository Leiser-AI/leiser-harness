# leiser-desktop

The desktop app (ADR 0004): a Tauri shell with a TypeScript UI in `web/`.
It renders what the harness service reports and holds no chat state of its
own.

The harness service runs as its own process, `leiser-harness-service`
(ADR 0005, ADR 0007), so closing or updating the app never stops a turn.
The app's Rust side connects to it over the local interface and forwards it
1:1 (`execute` command, `service-event` events). When no service runs, the
app starts the one next to its own program (or `LEISER_SERVICE_PROGRAM`).
The service keeps its data, its endpoint file and `service.log` in the app's
data directory (on macOS `~/Library/Application Support/ai.leiser.harness`)
and keeps running after the app quits.

When the service program is replaced, for example by `cargo build`, the
service shows that an update waits, applies it once no turn runs and
resumes every chat.

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

`npm run tauri dev` also builds `leiser-harness-service`. To stop the
service, quit the app and run:

```bash
pkill -f leiser-harness-service
```

To try the app without a subscription, run it on the testkit's fake agent
CLI, which answers every message with a tool call, its result and an echo.
Its chats are kept apart from real ones (data directory
`fake-agent-cli/`). A fake turn waits after its tool call while the file
`fake-agent-cli/fake-agent-cli-state/turn-gate` in the data directory is
missing: delete it to hold turns open, create it again to let them finish.

```bash
cargo build -p leiser-testkit --bin fake-agent-cli
LEISER_FAKE_AGENT_CLI="$PWD/../../target/debug/fake-agent-cli" npm run tauri dev
```

## Checks

- `npm run build` type-checks and builds the UI (also in CI).
- No automated UI tests in v1; check the app by hand.
- `web/src/protocol.ts` mirrors `crates/leiser-service/src/protocol.rs`;
  change both together.
