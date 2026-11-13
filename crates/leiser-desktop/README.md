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

A chat is always open. On start, and after a chat is marked done, the app
shows a new chat in the most recently used repository. It starts on the
harness service only with its first message, so an open app creates no
chats or worktrees by itself. Until then the composer chooses where it
starts (repository; new worktree or main checkout) and with what (agent
CLI and subscription, permission mode), beginning with the defaults from
the repository's `.leiser/config.toml`. While a service update waits, the
running service may not know these settings yet: a new chat then starts
with the repository's defaults, and its settings change once it has
started.

Chats start on the installed Claude Code; Codex is configured as a second
agent CLI. The composer's agent CLI picker lists every agent CLI's
default login and every subscription, grouped by provider. Choosing a
subscription or default login of the other provider moves the chat to that
agent CLI between turns, with a handoff. An app started from the Finder does not get the shell's `PATH`,
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

The top bar's "Open pull request" checks the host account, pushes the
chat's branch and opens a pull request into the repository's base branch
(`base_branch` in `.leiser/config.toml`, else `origin`'s default branch);
a chat has at most one, and "Push" updates it. Selecting a chat checks its
open pull request once; the "PR #N" button checks it again. Once it is
merged, the chat offers to remove its worktree and local branch; the chat
stays readable. In every permission mode the harness service refuses a
push to `main` or `development`, a force push and a push of a commit with
AI attribution; every agent session gets the same hard blocks as deny
rules. On the fake git host, write the pull request's number to
`fake-git-host-merged` in the host account's `gh` config directory
(`host-accounts/<id>/gh` in the data directory) to have it reported merged.

Auto-fix is on for every pull request a chat opens; the top bar's Auto-fix
button switches it per chat. While the pull request is open, the harness
service polls the git host every 30 seconds (5 on the fake git host) with
the entity tags of its last answers. A failing check or commit status, a
conflict with the base branch or a new comment by someone who can write to
the repository wakes the chat's agent in its own agent session; once its
turn has ended, the harness pushes the fix through the host account check
and the hard blocks, unless the pull request was merged or closed
meanwhile. Auto-fix stops and says why in the chat after three attempts at
checks or a conflict without a new commit, when a check fails or the branch
conflicts again on the commit it pushed, when a fix cannot be pushed, and
when five polls in a row fail. On the fake git host, write
`fake-git-host-activity.json` into the same directory to fail checks, make
the branch conflict or post comments, for example `{"1": {"failed_checks":
{"test": "assertion failed"}, "conflicting": false, "comments": [{"author":
"reviewer", "body": "Rename it", "location": {"path": "src/lib.rs", "line":
3}}]}}`.

A repository with an open wayfinder map (an issue labelled
`wayfinder:map` on the git host) shows "Wayfinder map" in the sidebar; one
without shows nothing extra. It draws the map as a graph from Start to
Destination: each ticket (the map's sub-issues) as a box with its type as
icon and color, arrows along the git host's blocking dependencies, and its
state: closed, claimed (assigned), blocked (an open blocker; one outside
the map shows as a small box of its own) or on the frontier. A picker
switches between several open maps. While the app is connected, the
harness service polls the maps every 30 seconds (5 on the fake git host)
with entity tags through the repository's host account, so a ticket closed
or claimed on the git host shows up by itself. Clicking a ticket opens it
in the browser; a frontier ticket's chat button opens a new chat in a new
worktree with the wayfinder skill invoked for the ticket in the composer
(`/wayfinder <map> <ticket>` for Claude Code, `$wayfinder` for Codex; it
follows the agent CLI chosen in the composer), ready to send: sending
starts the chat. On the fake git host, write `fake-git-host-wayfinder.json`
into the host account's `gh` config directory, for example `{"maps":
[{"number": 1, "title": "Map: v2", "tickets": [{"number": 2, "title": "Pick
a store", "type": "research"}, {"number": 3, "title": "Agree on the API",
"type": "grilling", "blocked_by": [2], "assignees": ["someone"], "open":
true}]}]}`.

A repository's CLI extensions button in the sidebar opens the CLI
extensions panel instead of the chat; the same button, or Close in the top
bar, closes it and shows what was open before. The panel lists what each
agent CLI loads from the repository (instruction files, skills, MCP servers,
hooks), one agent CLI at a time, and opens an extension's file as text.
Saving writes the agent CLI's own file; nothing is converted between agent
CLIs. A file an agent CLI could not read shows as a warning and still opens.
While the open file has unsaved changes the panel stays open. Where the
agent CLIs look is in `docs/cli-extension-locations.md`.

The top bar's Terminal button opens a terminal panel on the right with a
shell in the chat's checkout (its worktree, or the main checkout): the login
shell on macOS, PowerShell on Windows. Each chat keeps its own shell while
another chat is selected or the panel is hidden. The shells run in the app,
not in the harness service (ADR 0008): they end with "End shell", when the
shell exits, and when the app quits. The PTY code is in
`crates/leiser-terminal`; the screen is xterm.js.

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
CLI, which plays both Claude Code and Codex and answers every message with
a tool call, its result, an echo and a todo it adds through the harness
MCP server (refused unless the repository's `.leiser/config.toml` has
`[agent_access] todo_list = true`).
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

## Installers

`npm run package` (`scripts/package.mjs`) builds `leiser-harness-service`
in release mode, copies it to `binaries/` under the name Tauri expects
(`leiser-harness-service-<target>`), then runs `tauri build` with
`tauri.release.conf.json`. That file adds the service as `externalBin`, so
every installer puts it next to the app, where the app looks for it. It is
a separate file because `tauri-build` requires an `externalBin` to exist on
every build, also `cargo test`. It also limits the installers to
`.app`/`.dmg`, NSIS and `.deb`/`.rpm` (no AppImage, whose files are gone
once the app quits, while the service runs on; no MSI, which takes no beta
version), signs the macOS app ad hoc and gives the Windows installer
`windows/installer-hooks.nsh`, which moves the running service program
aside so it can be replaced.

With `TAURI_SIGNING_PRIVATE_KEY` set, the build signs the installers the
updater installs; a beta version (`X.Y.Z-beta.N`) reads the beta update
feed. `--target universal-apple-darwin` builds the service for both Mac
architectures and joins them with `lipo`. The `Release` workflow runs the
same command on each system.

## App updates

`src/app_update.rs` and `web/src/appUpdate.ts`: the app looks for a new
version in its channel's update feed (ADR 0009) when it starts, every ten
minutes while it runs and when its window gets the focus again, and offers
it as "Update to …" at the bottom of the sidebar.
Installing it restarts the app; turns keep running in the harness service,
which takes the new program as a service update. A failed install says why
on hover and can be retried. Debug builds (`npm run tauri dev`) never look
for updates.

## App icon

`icons/icon.svg` is the source of the app icon: the Leiser AI logo on
Apple's macOS app icon grid. The other files in `icons/` are generated from
it. After a change to the source, run:

```bash
npm run tauri icon icons/icon.svg -- -o ../../target/app-icon
```

Then copy `32x32.png`, `128x128.png`, `128x128@2x.png`, `icon.icns`,
`icon.ico` and `icon.png` from `../../target/app-icon/` into `icons/`. The
first five are the ones `bundle.icon` in `tauri.conf.json` lists; `icon.png`
is not bundled and is copied only so that no file keeps an old icon. The
command also writes sizes for app stores and phones, which the app does not
use.

## Styles

- `web/src/tokens.css` names every color, font size, spacing step, radius
  and shadow, for light and dark (dark follows the OS). Views use these
  variables, never raw values.
- `web/src/components.css` styles the shared pieces: buttons (`primary`,
  `ghost`, `danger`, `small`, `icon-button`, `round-button`), menus
  (`menu-button` and its `menu`, built by `web/src/menu.ts`), inputs,
  `badge`, `callout`, `code-block`, `hero-logo`, dialogs.
- `web/src/styles.css` lays out the window and the views.
- `web/src/icons.ts` draws the few line icons; no icon font or package.

## Checks

- `npm run build` type-checks and builds the UI (also in CI).
- No automated UI tests in v1; check the app by hand.
- `web/src/protocol.ts` mirrors `crates/leiser-service/src/protocol.rs`;
  change both together.
