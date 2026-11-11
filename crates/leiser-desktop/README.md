# leiser-desktop

The desktop app (ADR 0004): a Tauri shell with a TypeScript UI in `web/`.
It renders what the harness service reports and holds no chat state of its
own.

For v1 the app's Rust side hosts the harness service in-process and
forwards its local interface 1:1 (`execute` command, `service-event`
events). Until the real drivers land (#26, #27), chats run on the testkit's
fake agent CLI, which answers every message with a tool call, its result and
an echo.

## Run it

Needs Node.js (LTS) and, on Linux, WebKitGTK (see `.github/workflows/ci.yml`).

```bash
cd crates/leiser-desktop
npm install
npm run tauri dev
```

`tauri dev` builds `fake-agent-cli` first and finds it next to the app
binary. Set `LEISER_FAKE_AGENT_CLI` to use another path.

## Checks

- `npm run build` type-checks and builds the UI (also in CI).
- No automated UI tests in v1; check the app by hand.
- `web/src/protocol.ts` mirrors `crates/leiser-service/src/protocol.rs`;
  change both together.
