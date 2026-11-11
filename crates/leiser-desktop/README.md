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
so the harness service asks the developer's login shell for it once when
it starts, and the agent CLIs run with it; after a change to the shell
profile, restart the app. The service also looks in common install
directories and, for Claude Code, at the copy the Claude desktop app
bundles (see `crates/leiser-service/src/installed.rs`). To run another
program, set `LEISER_CLAUDE_CODE` or `LEISER_CODEX` to its path.
`repo-check` runs on Claude Code, read-only, when a repository is added and
on demand; it needs the `repo-check` skill in `~/.claude/skills/`.

Host accounts sign in with the GitHub CLI `gh` (found like the agent CLIs,
or set `LEISER_GH`), each in a `gh` config directory of its own. A
repository names its host account in `.leiser/config.toml`
(`host_account = "Work GitHub"`); the harness sets the commit author and
credentials in the repository's local git config only and checks the
account before every push. On the fake agent CLI, host accounts sign in
to a fake git host instead.

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
CLI, which answers every message with a tool call, its result, an echo and
a todo it adds through the harness MCP server (refused unless the
repository's `.leiser/config.toml` has `[agent_access] todo_list = true`).
Its chats are kept apart from real ones (data directory
`fake-agent-cli/`). A fake turn waits after its tool call while the file
`fake-agent-cli/fake-agent-cli-state/turn-gate` in the data directory is
missing: delete it to hold turns open, create it again to let them finish.
Every `repo-check` gets a made-up sample report.
To try a usage limit, write
`{"limit":"5-hour limit","resets_in_seconds":3600}` to
`fake-usage-limit.json` in a subscription's config directory (its path is
the tooltip of the subscription's kind in the Subscriptions dialog): every
turn on that subscription then stops at the limit.

```bash
cargo build -p leiser-testkit --bin fake-agent-cli
LEISER_FAKE_AGENT_CLI="$PWD/../../target/debug/fake-agent-cli" npm run tauri dev
```

## Checks

- `npm run build` type-checks and builds the UI (also in CI).
- No automated UI tests in v1; check the app by hand.
- `web/src/protocol.ts` mirrors `crates/leiser-service/src/protocol.rs`;
  change both together.
