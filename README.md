# leiser-harness

A desktop app that runs coding agents in chats: Claude Code and Codex, on
the logins and subscriptions you already have, each chat in its own git
worktree. Written in Rust (Tauri) for macOS, Windows and Linux.

> **Source-available, not open source.** Free for private, noncommercial
> use under the [PolyForm Noncommercial License 1.0.0](LICENSE.md).
> Commercial use requires a separate license. See
> [ADR 0001](docs/adr/0001-polyform-noncommercial-license.md).

## What it does

- **Your agent CLIs, your subscriptions.** Chats run on the installed
  Claude Code and Codex. Add several subscriptions per provider; when one
  hits its usage limit, the chat continues on another, also across agent
  CLIs with a handoff.
- **One worktree per chat.** Every chat works in its own git worktree (or
  the main checkout), with checkpoints per turn, a todo list and a
  terminal.
- **Permission modes and hard blocks.** From asking before every command to
  full access. In every mode the harness refuses a push to `main` or
  `development`, a force push and commits with AI attribution.
- **Pull requests and Auto-fix.** A chat opens its pull request through the
  repository's host account (GitHub). Failing checks, conflicts and review
  comments wake the chat's agent to fix them; merging stays with you.
- **CLI extensions and wayfinder maps.** See and edit what each agent CLI
  loads from a repository (instructions, skills, MCP servers, hooks), and
  see a repository's wayfinder map as a graph.
- **Turns survive the app.** The agents run in the harness service, a
  process of its own, so closing or updating the app does not stop a turn.

## Install

Download the installer for your system from the
[latest release](https://github.com/Leiser-AI/leiser-harness/releases/latest):

| System                          | File                                   |
| ------------------------------- | -------------------------------------- |
| macOS (Apple silicon, Intel)    | `leiser-harness_<version>_universal.dmg` |
| Windows 10/11 (x64)             | `leiser-harness_<version>_x64-setup.exe` or `.msi` |
| Linux (x64)                     | `.deb` or `.rpm`                       |

The installers are not signed with a paid certificate yet, so the system
warns on the first start.

### macOS

1. Open the `.dmg` and drag **leiser-harness** into **Applications**.
2. Open it. macOS blocks it the first time: open **System Settings →
   Privacy & Security** and click **Open Anyway**. Or run once:

   ```bash
   xattr -dr com.apple.quarantine /Applications/leiser-harness.app
   ```

### Windows

1. Run `leiser-harness_<version>_x64-setup.exe` (or, for managed
   installs, `leiser-harness_<version>_x64_en-US.msi`).
2. If SmartScreen shows "Windows protected your PC", click **More info →
   Run anyway**. The setup installs Microsoft Edge WebView2 if it is
   missing.

### Linux

```bash
# Debian, Ubuntu
sudo apt install ./leiser-harness_<version>_amd64.deb
# Fedora
sudo dnf install ./leiser-harness-<version>-1.x86_64.rpm
```

Both install WebKitGTK 4.1, which the app needs. There is no AppImage: the
harness service keeps running after the app quits, and an AppImage's files
are gone by then.

### What else you need

- **git**.
- **At least one agent CLI, signed in:**
  [Claude Code](https://docs.anthropic.com/en/docs/claude-code) (`claude`)
  or [Codex](https://github.com/openai/codex) (`codex`). The harness finds
  them through your login shell's `PATH` and common install directories; to
  use another program, set `LEISER_CLAUDE_CODE` or `LEISER_CODEX` to its
  path.
- **GitHub CLI** [`gh`](https://cli.github.com), for pull requests and
  Auto-fix. Optional.
- **A `repo-check` skill** in `~/.claude/skills/repo-check/`, for the
  repository check that runs when you add a repository. Optional; without
  it the check fails and everything else still works.

### Update and uninstall

The harness service keeps running after you quit the app, so turns are not
interrupted.

- **Update:** install the new version over the old one. The running harness
  service switches to it as soon as no turn runs. On Windows, quit the app
  and stop the service first (below), since Windows cannot replace a
  running program.
- **Stop the service:** `pkill -f leiser-harness-service` (macOS, Linux) or
  `taskkill /IM leiser-harness-service.exe /F` (Windows).
- **Uninstall:** stop the service and remove the app. To remove your chats
  too, delete the data directory
  (`~/Library/Application Support/ai.leiser.harness` on macOS,
  `%APPDATA%\ai.leiser.harness` on Windows,
  `~/.local/share/ai.leiser.harness` on Linux) and the chats' worktrees in
  `~/.leiser/worktrees`.

## Build from source

Needs [Rust](https://rustup.rs) (the version in `rust-toolchain.toml` is
picked up), [Node.js](https://nodejs.org) LTS and, on Linux, the WebKitGTK
libraries:

```bash
sudo apt install libwebkit2gtk-4.1-dev librsvg2-dev libxdo-dev
```

Then build the installers for your system:

```bash
cd crates/leiser-desktop
npm ci
npm run package
```

They land in `target/<target>/release/bundle/`. On macOS, for both Apple
silicon and Intel:

```bash
rustup target add aarch64-apple-darwin x86_64-apple-darwin
npm run package -- --target universal-apple-darwin
```

To run the app from source while you work on it, see
[`crates/leiser-desktop/README.md`](crates/leiser-desktop/README.md).

## Development

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI runs the same checks on Linux, macOS and Windows, plus `cargo deny`
(vulnerabilities and licenses) and a secret scan.

The crates:

- `leiser-service` – the harness service: chats, agent sessions, the
  drivers for Claude Code and Codex, worktrees, pull requests, Auto-fix.
- `leiser-harness-service` – the harness service as its own process.
- `leiser-desktop` – the desktop app (Tauri, TypeScript UI in `web/`).
- `leiser-terminal` – the terminal panel's PTY.
- `leiser-testkit` – test helpers and the fake agent CLI.

Architecture decisions are in [`docs/adr/`](docs/adr/), the domain language
in [`GLOSSARY.md`](GLOSSARY.md).

### Releases

A release is a merge of `development` into `main` (see
[`docs/agents/workflow.md`](docs/agents/workflow.md)). Tagging that commit
on `main` with `v<version>` runs the `Release` workflow: it builds the
installers on GitHub's macOS, Windows and Linux runners and attaches them to
a draft GitHub release, which a maintainer then publishes. A beta comes from
`development` instead, tagged `v<version>-beta.<n>`, and becomes a
pre-release; its installers carry the plain version. The version is
`X.Y.Z` (MSI accepts nothing else) and is set in both `Cargo.toml` and
`crates/leiser-desktop/tauri.conf.json`; the workflow checks that both and
the tag agree. A pull request that changes the packaging builds the
installers without publishing them.

## Working with coding agents

- `AGENTS.md` – rules and checks for every agent (`CLAUDE.md` imports it).
- `CODING_STANDARDS.md` – standards the `code-review` skill enforces.
- `GLOSSARY.md` and `docs/adr/` – domain language and architecture
  decisions. Created by `/grill-with-docs` when a term or decision is
  actually settled, not up front.
- `.agents/skills/` – the `engineering` and `productivity` skills from
  [mattpocock/skills](https://github.com/mattpocock/skills) (MIT, see
  `.agents/skills/LICENSE`), installed with the
  [skills](https://skills.sh) CLI and pinned in `skills-lock.json`.
  `.claude/skills/` links to them. Start with `/ask-matt` if you are unsure
  which one fits. Update with `npx skills@latest update` and review the
  diff before you commit it.
